//! The jobs scheduler daemon (`zdx jobs daemon`).
//!
//! Each tick plans with `zdx_engine::jobs::plan::plan_tick` and carries the
//! actions out. A due occurrence runs as a child `zdx jobs run` process, which
//! takes the job's lock and claims the occurrence before anything else, so
//! each run has its own runtime environment (`ZDX_THREAD_ID`, artifact
//! directory) and a crash in one run cannot stop the scheduler or another run.
//! The daemon itself writes only missed and interrupted outcomes, each under
//! the job's lock.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeDelta, Utc};
use tokio::sync::mpsc;
use zdx_engine::job_daemon;
use zdx_engine::jobs::plan::{self, Action, ClaimStatus, TickInput};
use zdx_engine::jobs::schedule::Occurrence;
use zdx_engine::jobs::store::{Claim, JobBusy, JobRunRecord, SchedulerState, new_run_id};
use zdx_engine::jobs::{
    self, JobCatalog, JobDefinition, JobSchedule, JobsHome, LastRun, Outcome, RunTrigger,
};

/// At most this many scheduled runs are live at once.
const MAX_LIVE_RUNS: usize = 3;
/// Finished recurring claims older than this are pruned.
const CLAIM_RETENTION: TimeDelta = TimeDelta::days(8);
/// How often unfinished claims are rescanned, to catch runs that died in
/// processes this daemon did not start.
const RESCAN_INTERVAL: TimeDelta = TimeDelta::minutes(10);

/// How the daemon was started; children inherit these.
pub struct DaemonOptions<'a> {
    pub root: &'a Path,
    pub system_prompt_override: Option<&'a str>,
    pub no_thread: bool,
    pub poll_interval_secs: u64,
}

/// Runs the scheduler loop.
///
/// # Errors
/// Returns an error when another daemon runs, old `automations` files still
/// need `zdx jobs migrate`, or startup state cannot be written.
pub async fn run(options: DaemonOptions<'_>) -> Result<()> {
    let poll = Duration::from_secs(options.poll_interval_secs.max(1));
    zdx_engine::pidfile::ensure_unique("daemon")?;
    let home = JobsHome::current();
    jobs::migrate::ensure_migrated(&home)?;
    let _pid_guard = zdx_engine::pidfile::write("daemon").context("write daemon PID file")?;
    let config_dir = std::env::current_dir().context("read daemon working directory")?;
    let _record_guard = job_daemon::record_running(options.root, &config_dir)
        .context("write daemon context record")?;

    let now = Utc::now();
    let state = home.scheduler_state().unwrap_or(SchedulerState {
        since: now,
        last_checked_at: now,
    });
    home.save_scheduler_state(&state)
        .context("write scheduler state")?;

    let (exits_tx, mut exits) = mpsc::unbounded_channel();
    let mut scheduler = Scheduler {
        home,
        root: options.root.to_path_buf(),
        system_prompt: options.system_prompt_override.map(str::to_string),
        no_thread: options.no_thread,
        exe: std::env::current_exe().context("locate the zdx binary")?,
        live: HashMap::new(),
        attempted: HashSet::new(),
        watch: Vec::new(),
        state,
        exits_tx,
        jobs: Vec::new(),
        last_error: None,
        last_invalid: Vec::new(),
        last_prune: None,
        last_rescan: None,
    };
    tracing::info!(
        root = %options.root.display(),
        poll_secs = poll.as_secs(),
        since = %scheduler.state.since,
        "Jobs daemon started"
    );
    scheduler.recover_at_start();

    loop {
        scheduler.tick(Utc::now());
        let wait = scheduler.wait(poll);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            Some(exit) = exits.recv() => {
                scheduler.handle_exit(&exit);
                while let Ok(exit) = exits.try_recv() {
                    scheduler.handle_exit(&exit);
                }
            }
        }
    }
}

struct ChildExit {
    job: String,
    occurrence: Occurrence,
    /// Exit code; `None` when killed by a signal.
    code: Option<i32>,
}

struct Scheduler {
    home: JobsHome,
    root: PathBuf,
    system_prompt: Option<String>,
    no_thread: bool,
    exe: PathBuf,
    /// Live child runs: job name -> occurrence.
    live: HashMap<String, Occurrence>,
    /// Occurrences already handed to a child in this process.
    attempted: HashSet<(String, String)>,
    /// Unfinished claims held by another process at start; re-checked each
    /// tick until they end.
    watch: Vec<(String, String)>,
    state: SchedulerState,
    exits_tx: mpsc::UnboundedSender<ChildExit>,
    /// Valid jobs from the last tick.
    jobs: Vec<JobDefinition>,
    last_error: Option<String>,
    last_invalid: Vec<String>,
    last_prune: Option<DateTime<Utc>>,
    last_rescan: Option<DateTime<Utc>>,
}

impl Scheduler {
    /// Marks runs that died with the previous daemon as interrupted, and
    /// watches runs that are still alive in other processes.
    fn recover_at_start(&mut self) {
        let catalog = jobs::catalog(&self.home).unwrap_or_default();
        self.rescan_unfinished(&catalog);
    }

    /// Finds unfinished claims not owned by a live child: marks dead ones
    /// interrupted and watches the rest.
    ///
    /// Also repairs `last.json` from final claims, for runs that died between
    /// finalizing their claim and writing it.
    fn rescan_unfinished(&mut self, catalog: &JobCatalog) {
        let claims = match self.home.all_claims() {
            Ok(claims) => claims,
            Err(err) => {
                tracing::error!(err = format!("{err:#}"), "Failed to read job claims");
                return;
            }
        };
        match self.home.reconcile_last_runs(&claims) {
            Ok(repaired) if !repaired.is_empty() => {
                tracing::warn!(jobs = ?repaired, "Repaired stale last-run records from claims");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(err = format!("{err:#}"), "Failed to repair last runs"),
        }
        for claim in claims.into_iter().filter(|claim| claim.outcome.is_none()) {
            let id = (claim.job.clone(), claim.key.clone());
            let owned = self
                .live
                .get(&claim.job)
                .is_some_and(|occurrence| occurrence.key == claim.key);
            if owned || self.watch.contains(&id) {
                continue;
            }
            if !self.mark_interrupted(catalog, &claim.job, &claim.key) {
                self.watch.push(id);
            }
        }
    }

    fn tick(&mut self, now: DateTime<Utc>) {
        let catalog = match jobs::catalog(&self.home) {
            Ok(catalog) => {
                self.last_error = None;
                catalog
            }
            Err(err) => {
                let message = format!("{err:#}");
                if self.last_error.as_deref() != Some(message.as_str()) {
                    tracing::error!(err = %message, "Failed to read jobs");
                    self.last_error = Some(message);
                }
                return;
            }
        };
        self.log_invalid(&catalog);
        self.check_watch(&catalog);
        if self
            .last_rescan
            .is_none_or(|last| now - last > RESCAN_INTERVAL)
        {
            self.last_rescan = Some(now);
            self.rescan_unfinished(&catalog);
        }

        let input = TickInput {
            now,
            since: self.state.since,
            last_checked_at: self.state.last_checked_at,
        };
        let actions = plan::plan_tick(
            &input,
            &catalog.jobs,
            &Local,
            &PlanView {
                home: &self.home,
                live: &self.live,
            },
        );
        let mut complete = true;
        for action in actions {
            complete &= self.apply(&catalog, action);
        }

        // A missed summary that could not be recorded keeps the tick from
        // advancing, so the next tick finds it again. Jobs whose summary was
        // recorded are protected from a repeat by their missed checkpoint.
        if complete {
            self.state.last_checked_at = self.state.last_checked_at.max(now);
        }
        let prune = self
            .last_prune
            .is_none_or(|last| now - last > TimeDelta::days(1));
        let cutoff = now - CLAIM_RETENTION;
        if prune {
            // Raise the floor first, so no occurrence whose claim is about to
            // be deleted can be scheduled again.
            self.state.since = self.state.since.max(cutoff);
        }
        match self.home.save_scheduler_state(&self.state) {
            Ok(()) if prune => {
                self.last_prune = Some(now);
                if let Err(err) = self.home.prune_recurring_claims(cutoff) {
                    tracing::warn!(err = format!("{err:#}"), "Failed to prune job claims");
                }
            }
            Ok(()) => {}
            Err(err) => {
                tracing::error!(err = format!("{err:#}"), "Failed to save scheduler state");
            }
        }
        self.jobs = catalog.jobs;
    }

    /// Sleeps until the next due instant, at most `poll`.
    fn wait(&self, poll: Duration) -> Duration {
        let now = Utc::now();
        plan::next_due_after(&self.jobs, now, &Local)
            .and_then(|next| (next - now + TimeDelta::milliseconds(200)).to_std().ok())
            .map_or(poll, |until| until.min(poll))
    }

    fn log_invalid(&mut self, catalog: &JobCatalog) {
        let current: Vec<String> = catalog
            .invalid
            .iter()
            .map(|invalid| format!("{}: {}", invalid.path.display(), invalid.error))
            .collect();
        if current != self.last_invalid {
            for line in &current {
                tracing::error!(job = %line, "Invalid job file; it will not run");
            }
            self.last_invalid = current;
        }
    }

    fn check_watch(&mut self, catalog: &JobCatalog) {
        let watch = std::mem::take(&mut self.watch);
        for (job, key) in watch {
            let unfinished = self
                .home
                .read_claim(&job, &key)
                .ok()
                .flatten()
                .is_some_and(|claim| claim.outcome.is_none());
            if unfinished && !self.mark_interrupted(catalog, &job, &key) {
                self.watch.push((job, key));
            }
        }
    }

    /// Carries out one planned action. Returns false only when a missed
    /// summary could not be recorded and must be planned again.
    fn apply(&mut self, catalog: &JobCatalog, action: Action) -> bool {
        let find = |name: &str| catalog.jobs.iter().find(|job| job.name == name);
        match action {
            Action::Start { job, occurrence } => {
                if let Some(definition) = find(&job) {
                    self.start(definition, &occurrence);
                }
            }
            Action::MissOnce { job, occurrence } => {
                if let Some(definition) = find(&job) {
                    let grace = definition.effective_grace_secs().unwrap_or_default();
                    tracing::warn!(name = %job, run_at = %occurrence.at, "Once job missed");
                    self.record_not_run(
                        definition,
                        &occurrence,
                        Outcome::Missed,
                        format!(
                            "did not start within its grace ({grace}s after run_at): the daemon \
                             was down, busy, or the job was already running"
                        ),
                    );
                }
            }
            Action::MissRecurring { job, latest, count } => {
                if let Some(definition) = find(&job) {
                    return self.record_recurring_missed(definition, &latest, count);
                }
            }
            Action::Archive { job, occurrence } => {
                if let Some(definition) = find(&job) {
                    self.archive_if_final(definition, &occurrence);
                }
            }
            Action::Recover { job, occurrence } => {
                self.mark_interrupted(catalog, &job, &occurrence.key);
            }
        }
        true
    }

    fn start(&mut self, job: &JobDefinition, occurrence: &Occurrence) {
        let id = (job.name.clone(), occurrence.key.clone());
        if self.live.len() >= MAX_LIVE_RUNS || self.attempted.contains(&id) {
            return;
        }
        let mut command = tokio::process::Command::new(&self.exe);
        command.arg("--root").arg(&self.root);
        if let Some(prompt) = &self.system_prompt {
            command.arg("--system-prompt").arg(prompt);
        }
        if self.no_thread {
            command.arg("--no-thread");
        }
        command
            .args(["jobs", "run", &job.name, "--occurrence", &occurrence.key])
            .arg("--scheduled-for")
            .arg(occurrence.at.to_rfc3339())
            .stdin(Stdio::null());

        match command.spawn() {
            Ok(mut child) => {
                tracing::info!(
                    name = %job.name,
                    occurrence = %occurrence.key,
                    pid = child.id().unwrap_or_default(),
                    "Starting job"
                );
                self.live.insert(job.name.clone(), occurrence.clone());
                self.attempted.insert(id);
                let tx = self.exits_tx.clone();
                let (name, occurrence) = (job.name.clone(), occurrence.clone());
                tokio::spawn(async move {
                    let code = match child.wait().await {
                        Ok(status) => status.code(),
                        Err(err) => {
                            tracing::error!(err = %err, name = %name, "Failed to wait for job run");
                            None
                        }
                    };
                    let _ = tx.send(ChildExit {
                        job: name,
                        occurrence,
                        code,
                    });
                });
            }
            Err(err) => {
                tracing::error!(name = %job.name, err = %err, "Failed to start job run");
                let recorded = self.record_not_run(
                    job,
                    occurrence,
                    Outcome::Failed,
                    format!("could not start the run process: {err}"),
                );
                // Unrecorded (job busy): let a later tick try again.
                if recorded {
                    self.attempted.insert(id);
                }
            }
        }
    }

    fn handle_exit(&mut self, exit: &ChildExit) {
        self.live.remove(&exit.job);
        let key = &exit.occurrence.key;
        let id = (exit.job.clone(), key.clone());
        if exit.code == Some(JobBusy::EXIT_CODE) {
            // Another run held the lock; plan the occurrence again.
            self.attempted.remove(&id);
            return;
        }
        let catalog = jobs::catalog(&self.home).unwrap_or_default();
        match self.home.read_claim(&exit.job, key) {
            Ok(Some(claim)) if claim.outcome.is_none() => {
                if !self.mark_interrupted(&catalog, &exit.job, key) {
                    self.watch.push(id);
                }
            }
            Ok(Some(_)) => {}
            // The run skipped itself (the job changed since it was planned);
            // plan the occurrence again if it is still due.
            Ok(None) if exit.code == Some(0) => {
                self.attempted.remove(&id);
            }
            Ok(None) => {
                let recorded = catalog
                    .jobs
                    .iter()
                    .find(|job| job.name == exit.job)
                    .is_some_and(|job| {
                        self.record_not_run(
                            job,
                            &exit.occurrence,
                            Outcome::Failed,
                            format!(
                                "the run process exited ({}) before taking its occurrence; see \
                                 the daemon log",
                                exit_label(exit.code)
                            ),
                        )
                    });
                if !recorded {
                    self.attempted.remove(&id);
                }
            }
            Err(err) => tracing::error!(err = format!("{err:#}"), "Failed to read job claim"),
        }
    }

    /// Records one summary for recurring occurrences that expired unstarted,
    /// under the job's lock: append the record, then move the job's missed
    /// checkpoint. Returns false when nothing was recorded (lock busy or the
    /// append failed), so the occurrences are planned again. A crash between
    /// the append and the checkpoint can repeat the summary once; it is never
    /// silently dropped.
    fn record_recurring_missed(
        &self,
        job: &JobDefinition,
        latest: &Occurrence,
        count: usize,
    ) -> bool {
        let Some(_lock) = self.lock(&job.name) else {
            return false;
        };
        tracing::warn!(name = %job.name, count, latest = %latest.at, "Recurring job missed");
        let run_id = new_run_id();
        let error = format!(
            "{count} occurrence(s) did not start within grace; latest scheduled for {}",
            latest.at.to_rfc3339()
        );
        let record = JobRunRecord::not_run(
            &job.name,
            &run_id,
            RunTrigger::Daemon,
            &job.schedule,
            latest.at,
            Outcome::Missed,
            error.clone(),
        );
        if let Err(err) = self.home.append_run_record(&record) {
            tracing::error!(
                err = format!("{err:#}"),
                "Failed to record missed occurrences"
            );
            return false;
        }
        if let Err(err) = self.home.set_missed_through(&job.name, latest.at) {
            tracing::error!(
                err = format!("{err:#}"),
                "Failed to save the missed checkpoint"
            );
        }
        self.write_last(&job.name, &run_id, Some(latest.at), Outcome::Missed, error);
        true
    }

    /// Records an occurrence that never ran (missed, or failed to start)
    /// under the job's lock, then archives a once job. Returns false when
    /// nothing was recorded (lock busy or write failed), so it can be retried.
    fn record_not_run(
        &self,
        job: &JobDefinition,
        occurrence: &Occurrence,
        outcome: Outcome,
        error: String,
    ) -> bool {
        let Some(_lock) = self.lock(&job.name) else {
            return false;
        };
        let claim = Claim::new(
            &job.name,
            &occurrence.key,
            RunTrigger::Daemon,
            job.schedule.kind(),
            occurrence.at,
        )
        .finished(outcome, Some(error.clone()));
        match self.home.create_claim(&claim) {
            Ok(true) => {}
            Ok(false) => return true,
            Err(err) => {
                tracing::error!(err = format!("{err:#}"), "Failed to write job claim");
                return false;
            }
        }
        self.append(&JobRunRecord::not_run(
            &job.name,
            &claim.run_id,
            RunTrigger::Daemon,
            &job.schedule,
            occurrence.at,
            outcome,
            error.clone(),
        ));
        self.write_last(
            &job.name,
            &claim.run_id,
            Some(occurrence.at),
            outcome,
            error,
        );
        self.archive(job, &occurrence.key);
        true
    }

    /// Marks an unfinished claim interrupted when no run holds the job's
    /// lock. Returns false while a run still holds it.
    fn mark_interrupted(&self, catalog: &JobCatalog, job: &str, key: &str) -> bool {
        let Some(_lock) = self.lock(job) else {
            return false;
        };
        let claim = match self.home.read_claim(job, key) {
            Ok(Some(claim)) if claim.outcome.is_none() => claim,
            _ => return true,
        };
        let error = "the run stopped without a result (its process exited or was killed)";
        tracing::warn!(name = %job, occurrence = %key, "Job run interrupted");
        let finished = claim
            .clone()
            .finished(Outcome::Interrupted, Some(error.to_string()));
        if let Err(err) = self.home.update_claim(&finished) {
            tracing::error!(err = format!("{err:#}"), "Failed to update job claim");
            return false;
        }
        let definition = catalog
            .jobs
            .iter()
            .find(|definition| definition.name == job);
        let mut record = JobRunRecord::not_run(
            job,
            &claim.run_id,
            claim.trigger,
            definition.map_or(&JobSchedule::Manual, |d| &d.schedule),
            claim.scheduled_for,
            Outcome::Interrupted,
            error.to_string(),
        );
        record.schedule_kind = Some(claim.schedule_kind);
        if definition.is_none() {
            record.schedule = None;
        }
        record.thread_id.clone_from(&claim.thread_id);
        self.append(&record);
        self.write_last(
            job,
            &claim.run_id,
            Some(claim.scheduled_for),
            Outcome::Interrupted,
            error.to_string(),
        );
        if let Some(definition) = definition {
            self.archive(definition, key);
        }
        true
    }

    fn archive_if_final(&self, job: &JobDefinition, occurrence: &Occurrence) {
        let Some(_lock) = self.lock(&job.name) else {
            return;
        };
        if self
            .home
            .read_claim(&job.name, &occurrence.key)
            .ok()
            .flatten()
            .is_some_and(|claim| claim.outcome.is_some())
        {
            self.archive(job, &occurrence.key);
        }
    }

    /// Archives a once job whose current occurrence is `key`. A job that was
    /// rescheduled since that occurrence keeps its file.
    fn archive(&self, job: &JobDefinition, key: &str) {
        if let JobSchedule::Once { run_at } = job.schedule
            && jobs::schedule::once_occurrence(run_at).key == key
        {
            match self.home.archive_once_job(&job.path, run_at) {
                Ok(Some(dest)) => {
                    tracing::info!(name = %job.name, dest = %dest.display(), "Archived once job");
                }
                Ok(None) => {}
                Err(err) => tracing::error!(err = format!("{err:#}"), "Failed to archive job"),
            }
        }
    }

    fn lock(&self, job: &str) -> Option<jobs::store::JobLock> {
        match self.home.try_lock_job(job) {
            Ok(lock) => lock,
            Err(err) => {
                tracing::error!(err = format!("{err:#}"), name = %job, "Failed to lock job");
                None
            }
        }
    }

    fn append(&self, record: &JobRunRecord) {
        if let Err(err) = self.home.append_run_record(record) {
            tracing::warn!(err = format!("{err:#}"), "Failed to append job run log");
        }
    }

    fn write_last(
        &self,
        job: &str,
        run_id: &str,
        scheduled_for: Option<DateTime<Utc>>,
        outcome: Outcome,
        error: String,
    ) {
        let last = LastRun {
            run_id: run_id.to_string(),
            trigger: RunTrigger::Daemon,
            outcome,
            scheduled_for,
            finished_at: Utc::now(),
            error: Some(error),
            thread_id: None,
        };
        if let Err(err) = self.home.write_last_run(job, &last) {
            tracing::warn!(
                err = format!("{err:#}"),
                "Failed to record the job's last run"
            );
        }
    }
}

/// The scheduler's live state as the planner sees it.
struct PlanView<'a> {
    home: &'a JobsHome,
    live: &'a HashMap<String, Occurrence>,
}

impl plan::JobView for PlanView<'_> {
    fn claim(&self, job: &str, key: &str) -> Option<ClaimStatus> {
        self.home.read_claim(job, key).ok().flatten().map(|claim| {
            if claim.outcome.is_some() {
                ClaimStatus::Finished
            } else {
                ClaimStatus::Running
            }
        })
    }

    fn busy(&self, job: &str) -> bool {
        self.live.contains_key(job) || self.home.is_job_running(job)
    }

    fn missed_through(&self, job: &str) -> Option<DateTime<Utc>> {
        self.home.missed_through(job)
    }
}

fn exit_label(code: Option<i32>) -> String {
    code.map_or_else(
        || "killed by a signal".to_string(),
        |code| format!("status {code}"),
    )
}
