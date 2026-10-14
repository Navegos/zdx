//! The scheduler's decisions for one tick, as a pure function of the job
//! definitions, the clock, existing claims, and which jobs are busy. The
//! daemon carries the actions out; every action that writes state re-checks
//! it under the job's lock.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};

use super::JobDefinition;
use super::schedule::{
    JobSchedule, Occurrence, next_recurring_after, once_occurrence, recurring_occurrences,
};

/// How far back one tick looks for recurring occurrences that expired
/// unstarted. Longer outages are summarized from this window only.
pub const MISSED_SCAN_CAP: TimeDelta = TimeDelta::days(7);

/// What is known about an occurrence's claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimStatus {
    /// Claimed, no outcome yet.
    Running,
    /// Claimed and final.
    Finished,
}

/// Clock inputs for one tick.
#[derive(Debug, Clone, Copy)]
pub struct TickInput {
    pub now: DateTime<Utc>,
    /// Recurring occurrences at or before this instant are ignored (see
    /// [`super::store::SchedulerState::since`]).
    pub since: DateTime<Utc>,
    pub last_checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start a run for this occurrence.
    Start { job: String, occurrence: Occurrence },
    /// A once job passed its grace without starting: record it as missed.
    MissOnce { job: String, occurrence: Occurrence },
    /// Recurring occurrences left their grace without starting. Record one
    /// summary, then move the job's missed checkpoint to `latest.at`.
    MissRecurring {
        job: String,
        latest: Occurrence,
        count: usize,
    },
    /// A once job's occurrence is final but the file is still in `jobs/`.
    Archive { job: String, occurrence: Occurrence },
    /// A once job's claim has no outcome and nothing holds its lock: the run
    /// died. Mark it interrupted.
    Recover { job: String, occurrence: Occurrence },
}

/// What the planner reads about each job's durable state.
pub trait JobView {
    /// The occurrence's claim, if any.
    fn claim(&self, job: &str, key: &str) -> Option<ClaimStatus>;
    /// True while a run of the job is live (or about to start).
    fn busy(&self, job: &str) -> bool;
    /// Latest recurring occurrence already recorded as missed.
    fn missed_through(&self, job: &str) -> Option<DateTime<Utc>>;
}

/// Plans one tick from the clock and each job's durable state.
#[must_use]
pub fn plan_tick<Tz: TimeZone>(
    input: &TickInput,
    jobs: &[JobDefinition],
    tz: &Tz,
    view: &impl JobView,
) -> Vec<Action> {
    let claim = |job: &str, key: &str| view.claim(job, key);
    let busy = |job: &str| view.busy(job);
    let mut starts = Vec::new();
    let mut others = Vec::new();
    for job in jobs {
        let Some(grace) = job.grace() else {
            continue;
        };
        let name = job.name.clone();
        match &job.schedule {
            JobSchedule::Manual => {}
            JobSchedule::Once { run_at } => {
                let occurrence = once_occurrence(*run_at);
                match claim(&name, &occurrence.key) {
                    Some(ClaimStatus::Finished) => others.push(Action::Archive {
                        job: name,
                        occurrence,
                    }),
                    Some(ClaimStatus::Running) if !busy(&name) => {
                        others.push(Action::Recover {
                            job: name,
                            occurrence,
                        });
                    }
                    Some(ClaimStatus::Running) => {}
                    None if occurrence.at > input.now => {}
                    None if input.now - occurrence.at <= grace => {
                        if !busy(&name) {
                            starts.push(Action::Start {
                                job: name,
                                occurrence,
                            });
                        }
                    }
                    None => others.push(Action::MissOnce {
                        job: name,
                        occurrence,
                    }),
                }
            }
            JobSchedule::Recurring { parsed, .. } => {
                // An occurrence is in grace at time t while it is at or after
                // `t - grace`, i.e. strictly after `edge(t)`.
                let edge = |t: DateTime<Utc>| t - grace - TimeDelta::nanoseconds(1);
                let due =
                    recurring_occurrences(parsed, edge(input.now).max(input.since), input.now, tz);
                if let Some(latest) = due.last()
                    && claim(&name, &latest.key).is_none()
                    && !busy(&name)
                {
                    starts.push(Action::Start {
                        job: name.clone(),
                        occurrence: latest.clone(),
                    });
                }

                // `last_checked_at` bounds the scan; the per-job checkpoint
                // keeps a summary that was written, but whose tick was not
                // saved (crash, or a tick held back), from counting again.
                let mut expired_after = edge(input.last_checked_at)
                    .max(input.since)
                    .max(edge(input.now) - MISSED_SCAN_CAP);
                if let Some(through) = view.missed_through(&name) {
                    expired_after = expired_after.max(through);
                }
                let missed: Vec<Occurrence> =
                    recurring_occurrences(parsed, expired_after, edge(input.now), tz)
                        .into_iter()
                        .filter(|occurrence| claim(&name, &occurrence.key).is_none())
                        .collect();
                if let Some(latest) = missed.last() {
                    others.push(Action::MissRecurring {
                        job: name,
                        latest: latest.clone(),
                        count: missed.len(),
                    });
                }
            }
        }
    }
    starts.sort_by_key(|action| match action {
        Action::Start { occurrence, .. } => occurrence.at,
        _ => input.now,
    });
    others.extend(starts);
    others
}

/// The next instant any job becomes due after `now`, so the daemon can wake
/// for it instead of waiting a full poll interval.
#[must_use]
pub fn next_due_after<Tz: TimeZone>(
    jobs: &[JobDefinition],
    now: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    jobs.iter()
        .filter_map(|job| match &job.schedule {
            JobSchedule::Manual => None,
            JobSchedule::Once { run_at } => {
                let at = once_occurrence(*run_at).at;
                (at > now).then_some(at)
            }
            JobSchedule::Recurring { parsed, .. } => next_recurring_after(parsed, now, tz),
        })
        .min()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use chrono::FixedOffset;

    use super::*;
    use crate::jobs::JobSource;

    fn tz() -> FixedOffset {
        FixedOffset::west_opt(3 * 3600).unwrap()
    }

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, h, m, s).unwrap()
    }

    fn job(
        name: &str,
        schedule: Option<&str>,
        run_at: Option<&str>,
        grace: Option<u32>,
    ) -> JobDefinition {
        JobDefinition {
            name: name.to_string(),
            path: PathBuf::from(format!("/jobs/{name}.md")),
            source: JobSource::User,
            schedule: JobSchedule::parse(schedule, run_at).unwrap(),
            grace_secs: grace,
            model: None,
            subagent: None,
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "prompt".to_string(),
        }
    }

    struct World {
        claims: HashMap<(String, String), ClaimStatus>,
        busy: Vec<String>,
        missed: HashMap<String, DateTime<Utc>>,
    }

    impl JobView for World {
        fn claim(&self, job: &str, key: &str) -> Option<ClaimStatus> {
            self.claims
                .get(&(job.to_string(), key.to_string()))
                .copied()
        }

        fn busy(&self, job: &str) -> bool {
            self.busy.iter().any(|busy| busy == job)
        }

        fn missed_through(&self, job: &str) -> Option<DateTime<Utc>> {
            self.missed.get(job).copied()
        }
    }

    impl World {
        fn new() -> Self {
            Self {
                claims: HashMap::new(),
                busy: Vec::new(),
                missed: HashMap::new(),
            }
        }

        fn claim(mut self, job: &str, key: &str, status: ClaimStatus) -> Self {
            self.claims
                .insert((job.to_string(), key.to_string()), status);
            self
        }

        fn plan(&self, input: &TickInput, jobs: &[JobDefinition]) -> Vec<Action> {
            plan_tick(input, jobs, &tz(), self)
        }
    }

    fn tick(now: DateTime<Utc>, last: DateTime<Utc>) -> TickInput {
        TickInput {
            now,
            since: at(0, 0, 0),
            last_checked_at: last,
        }
    }

    fn start_keys(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Start { job, occurrence } => Some(format!("{job}@{}", occurrence.key)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn once_job_starts_on_time_and_late_within_grace() {
        let jobs = [job("remind", None, Some("2026-10-20T09:00:00-03:00"), None)];
        let world = World::new();
        assert!(
            world
                .plan(&tick(at(11, 59, 30), at(11, 59, 0)), &jobs)
                .is_empty(),
            "pending"
        );
        assert_eq!(
            start_keys(&world.plan(&tick(at(12, 0, 5), at(11, 59, 35)), &jobs)),
            ["remind@20261020T120000Z"]
        );
        assert_eq!(
            start_keys(&world.plan(&tick(at(17, 59, 0), at(17, 58, 30)), &jobs)),
            ["remind@20261020T120000Z"],
            "still within the 6 h default grace"
        );
    }

    #[test]
    fn once_job_past_grace_is_missed_not_run() {
        let jobs = [job("remind", None, Some("2026-10-20T12:00:00Z"), Some(600))];
        let actions = World::new().plan(&tick(at(12, 10, 1), at(12, 10, 0)), &jobs);
        assert!(matches!(&actions[..], [Action::MissOnce { job, .. }] if job == "remind"));
    }

    #[test]
    fn once_job_claims_decide_archive_wait_or_recover() {
        let jobs = [job("remind", None, Some("2026-10-20T12:00:00Z"), None)];
        let key = "20261020T120000Z";
        let now = tick(at(12, 1, 0), at(12, 0, 30));

        let finished = World::new().claim("remind", key, ClaimStatus::Finished);
        assert!(matches!(
            &finished.plan(&now, &jobs)[..],
            [Action::Archive { .. }]
        ));

        let mut running = World::new().claim("remind", key, ClaimStatus::Running);
        running.busy.push("remind".to_string());
        assert!(running.plan(&now, &jobs).is_empty(), "live run: wait");

        let dead = World::new().claim("remind", key, ClaimStatus::Running);
        assert!(matches!(
            &dead.plan(&now, &jobs)[..],
            [Action::Recover { .. }]
        ));
    }

    #[test]
    fn busy_job_waits_instead_of_starting_twice() {
        let jobs = [job("remind", None, Some("2026-10-20T12:00:00Z"), None)];
        let mut world = World::new();
        world.busy.push("remind".to_string());
        assert!(
            world
                .plan(&tick(at(12, 0, 5), at(12, 0, 0)), &jobs)
                .is_empty()
        );
    }

    #[test]
    fn recurring_job_runs_only_the_latest_occurrence_within_grace() {
        // Every 5 minutes, local time UTC-3. Runs at 11:50 and 11:55 (UTC)
        // happened; the daemon was down from 11:58 to 12:07, so 12:00 and
        // 12:05 passed while it was down.
        let jobs = [job("poll", Some("*/5 * * * *"), None, None)];
        let world = World::new()
            .claim("poll", "20261020T0850", ClaimStatus::Finished)
            .claim("poll", "20261020T0855", ClaimStatus::Finished);
        let actions = world.plan(&tick(at(12, 7, 0), at(11, 58, 0)), &jobs);
        assert_eq!(start_keys(&actions), ["poll@20261020T0905"], "{actions:?}");
        assert_eq!(
            actions.len(),
            1,
            "12:00 is still inside the grace: {actions:?}"
        );

        // After 12:05 ran, 12:00 expires unstarted and counts as missed once.
        let world = world.claim("poll", "20261020T0905", ClaimStatus::Finished);
        let first = world.plan(&tick(at(12, 10, 30), at(12, 7, 0)), &jobs);
        assert!(
            matches!(
                &first[..],
                [Action::MissRecurring { count: 1, latest, .. }, Action::Start { .. }]
                    if latest.key == "20261020T0900"
            ),
            "{first:?}"
        );
        let world = world.claim("poll", "20261020T0910", ClaimStatus::Finished);
        let second = world.plan(&tick(at(12, 15, 30), at(12, 10, 30)), &jobs);
        assert!(
            !second
                .iter()
                .any(|a| matches!(a, Action::MissRecurring { .. })),
            "{second:?}"
        );
    }

    #[test]
    fn a_recorded_missed_summary_is_not_repeated_when_the_tick_was_not_saved() {
        // 10:55, 11:00, and 11:05 expired unstarted; their summary was written, but
        // `last_checked_at` stayed at 11:05 (crash before saving, or a tick
        // held back by a busy job).
        let jobs = [job("poll", Some("*/5 * * * *"), None, None)];
        let input = tick(at(11, 16, 0), at(11, 5, 0));
        let fresh = World::new().plan(&input, &jobs);
        assert!(
            matches!(
                fresh.iter().find(|a| matches!(a, Action::MissRecurring { .. })),
                Some(Action::MissRecurring { count: 3, latest, .. }) if latest.key == "20261020T0805"
            ),
            "{fresh:?}"
        );

        let mut world = World::new();
        world.missed.insert("poll".to_string(), at(11, 5, 0));
        let again = world.plan(&input, &jobs);
        assert!(
            !again
                .iter()
                .any(|a| matches!(a, Action::MissRecurring { .. })),
            "{again:?}"
        );
    }

    #[test]
    fn recurring_occurrence_already_claimed_is_not_started_again() {
        let jobs = [job("daily", Some("0 9 * * *"), None, None)];
        let world = World::new().claim("daily", "20261020T0900", ClaimStatus::Finished);
        assert!(
            world
                .plan(&tick(at(12, 0, 30), at(12, 0, 0)), &jobs)
                .is_empty()
        );
        let failed_fast = World::new().claim("daily", "20261020T0900", ClaimStatus::Finished);
        assert!(
            failed_fast
                .plan(&tick(at(12, 0, 59), at(12, 0, 30)), &jobs)
                .is_empty(),
            "a fast failure in the same minute is not retried by the scheduler"
        );
    }

    #[test]
    fn recurring_occurrences_before_the_first_start_are_ignored() {
        let jobs = [job("daily", Some("0 9 * * *"), None, None)];
        let input = TickInput {
            now: at(12, 5, 0),
            since: at(12, 4, 0),
            last_checked_at: at(12, 4, 0),
        };
        assert!(World::new().plan(&input, &jobs).is_empty());
    }

    #[test]
    fn long_outage_reports_missed_occurrences_from_a_capped_window() {
        let jobs = [job("hourly", Some("0 * * * *"), None, None)];
        let input = TickInput {
            now: at(12, 30, 0),
            since: Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            last_checked_at: Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
        };
        let actions = World::new().plan(&input, &jobs);
        let missed = actions
            .iter()
            .find_map(|a| match a {
                Action::MissRecurring { count, .. } => Some(*count),
                _ => None,
            })
            .unwrap();
        assert_eq!(missed, 7 * 24, "{actions:?}");
        assert!(
            start_keys(&actions).is_empty(),
            "12:00 is outside the 10 min grace"
        );
    }

    #[test]
    fn manual_jobs_are_never_planned_and_next_due_ignores_them() {
        let jobs = [
            job("adhoc", None, None, None),
            job("later", None, Some("2026-10-20T15:00:00Z"), None),
            job("daily", Some("0 9 * * *"), None, None),
        ];
        assert!(
            World::new()
                .plan(&tick(at(12, 30, 0), at(12, 29, 30)), &jobs)
                .is_empty()
        );
        assert_eq!(
            next_due_after(&jobs, at(12, 30, 0), &tz()),
            Some(at(15, 0, 0))
        );
    }
}
