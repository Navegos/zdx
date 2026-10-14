//! One-time move from the pre-rename layout (`$ZDX_HOME/automations/`,
//! `automations_runs.jsonl`) to Jobs (`$ZDX_HOME/jobs/`, `jobs_runs.jsonl`).
//!
//! Until it runs, [`ensure_migrated`] makes every jobs surface fail with a
//! pointer to `zdx jobs migrate`, so a missed step cannot silently stop all
//! scheduled jobs. Remove this module one release after the rename.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::markdown_files;
use super::store::JobsHome;

/// Fails while the old automations directory still holds job files.
///
/// # Errors
/// Returns the migration hint when legacy files exist.
pub fn ensure_migrated(home: &JobsHome) -> Result<()> {
    let legacy = home.legacy_dir();
    if markdown_files(&legacy).is_ok_and(|files| !files.is_empty()) {
        bail!(
            "job files are still in {} (the old automations location); run `zdx jobs migrate` \
             to move them to {}",
            legacy.display(),
            home.jobs_dir().display()
        );
    }
    Ok(())
}

/// What `zdx jobs migrate` will do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationPlan {
    /// Job files to move, `(from, to)`.
    pub moves: Vec<(PathBuf, PathBuf)>,
    /// Run log conversion, if the old log exists.
    pub runs_log: Option<RunsLogMigration>,
    /// Old files that are no longer read and are left in place.
    pub unused: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunsLogMigration {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Where the old log is kept after the conversion.
    pub backup: PathBuf,
    pub records: usize,
}

impl MigrationPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.runs_log.is_none()
    }
}

/// Plans the migration without changing anything.
///
/// # Errors
/// Returns an error when a target job file already exists (nothing is moved
/// then), or when the old run log cannot be read or parsed.
pub fn plan(home: &JobsHome) -> Result<MigrationPlan> {
    let mut plan = MigrationPlan::default();
    let jobs_dir = home.jobs_dir();
    let mut conflicts = Vec::new();
    for from in markdown_files(&home.legacy_dir())? {
        let file_name = from.file_name().context("job file without a name")?;
        let to = jobs_dir.join(file_name);
        if to.exists() {
            conflicts.push(to.display().to_string());
        }
        plan.moves.push((from, to));
    }
    if !conflicts.is_empty() {
        bail!(
            "cannot migrate: these jobs already exist in {}: {}. Rename or remove one copy, \
             then run `zdx jobs migrate` again",
            jobs_dir.display(),
            conflicts.join(", ")
        );
    }

    let from = home.legacy_runs_log();
    if from.is_file() {
        let records = convert_runs_log(&read(&from)?)
            .with_context(|| format!("convert {}", from.display()))?
            .lines()
            .count();
        plan.runs_log = Some(RunsLogMigration {
            backup: with_suffix(&from, ".migrated"),
            to: home.runs_log(),
            from,
            records,
        });
    }

    let old_state = home.zdx_home().join("automations_daemon_state.json");
    if old_state.is_file() {
        plan.unused.push(old_state);
    }
    Ok(plan)
}

/// Applies `plan`. The run log goes first and the job files last, so an
/// interrupted migration keeps the guard active and can be run again.
///
/// # Errors
/// Returns an error when a write or move fails; finished steps stay done.
pub fn apply(home: &JobsHome, plan: &MigrationPlan) -> Result<()> {
    if let Some(log) = &plan.runs_log {
        if log.backup.exists() {
            bail!(
                "{} already exists; move it away, then run `zdx jobs migrate` again",
                log.backup.display()
            );
        }
        let _lock = home.lock_runs_log()?;
        let converted = convert_runs_log(&read(&log.from)?)?;
        let existing = match fs::read_to_string(&log.to) {
            Ok(existing) => existing,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| format!("read {}", log.to.display())),
        };
        // A rerun after a crash between the two steps below finds the
        // converted records already at the start of the new log.
        if !existing.starts_with(&converted) {
            let tmp = with_suffix(&log.to, ".migrating");
            fs::write(&tmp, format!("{converted}{existing}"))
                .with_context(|| format!("write {}", tmp.display()))?;
            fs::rename(&tmp, &log.to).with_context(|| format!("write {}", log.to.display()))?;
        }
        fs::rename(&log.from, &log.backup)
            .with_context(|| format!("keep {} as {}", log.from.display(), log.backup.display()))?;
    }

    if !plan.moves.is_empty() {
        fs::create_dir_all(home.jobs_dir())
            .with_context(|| format!("create {}", home.jobs_dir().display()))?;
    }
    for (from, to) in &plan.moves {
        // Link, then unlink: unlike rename, linking never replaces a file
        // that appeared at the destination meanwhile.
        fs::hard_link(from, to).with_context(|| {
            format!(
                "move {} to {} (stopped; nothing was overwritten)",
                from.display(),
                to.display()
            )
        })?;
        fs::remove_file(from).with_context(|| format!("remove {}", from.display()))?;
    }
    // Only an empty directory is removed; anything else in it stays.
    let _ = fs::remove_dir(home.legacy_dir());
    Ok(())
}

/// Renames the `automation` key of each run record to `job`. Other fields
/// stay as they are.
fn convert_runs_log(raw: &str) -> Result<String> {
    let mut out = String::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line)
            .with_context(|| format!("line {} is not a JSON object", index + 1))?;
        if let Some(name) = value.remove("automation") {
            value.insert("job".to_string(), name);
        }
        out.push_str(&serde_json::to_string(&value)?);
        out.push('\n');
    }
    Ok(out)
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    const OLD_LOG: &str = "{\"automation\":\"daily\",\"trigger\":\"daemon\",\"attempt\":1,\"max_attempts\":1,\"started_at\":\"2026-10-01T11:00:00Z\",\"finished_at\":\"2026-10-01T11:00:10Z\",\"duration_ms\":10000,\"ok\":true,\"error\":null,\"schedule\":\"0 8 * * *\",\"model\":null,\"subagent\":null}\n\n";

    fn legacy_home() -> (tempfile::TempDir, JobsHome) {
        let dir = tempdir().unwrap();
        let home = JobsHome::at(dir.path());
        fs::create_dir_all(home.legacy_dir()).unwrap();
        fs::write(
            home.legacy_dir().join("daily.md"),
            "---\nschedule: \"0 8 * * *\"\n---\nReport.\n",
        )
        .unwrap();
        fs::write(home.legacy_dir().join("adhoc.md"), "---\n---\nOn demand.\n").unwrap();
        fs::write(home.legacy_runs_log(), OLD_LOG).unwrap();
        fs::write(dir.path().join("automations_daemon_state.json"), "{}").unwrap();
        (dir, home)
    }

    #[test]
    fn guard_blocks_until_migrated_then_jobs_and_history_carry_over() {
        let (_dir, home) = legacy_home();
        let err = super::super::catalog(&home).unwrap_err();
        assert!(err.to_string().contains("zdx jobs migrate"), "{err}");

        let plan = plan(&home).unwrap();
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(plan.runs_log.as_ref().unwrap().records, 1);
        assert_eq!(plan.unused.len(), 1);
        apply(&home, &plan).unwrap();

        let catalog = super::super::catalog(&home).unwrap();
        let names: Vec<_> = catalog.jobs.iter().map(|job| job.name.as_str()).collect();
        assert_eq!(names, ["adhoc", "daily"]);
        assert!(!home.legacy_dir().exists());
        let records = home.read_run_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].job, "daily");
        assert_eq!(
            fs::read_to_string(home.legacy_runs_log().with_extension("jsonl.migrated")).unwrap(),
            OLD_LOG
        );
        assert!(
            home.zdx_home()
                .join("automations_daemon_state.json")
                .exists(),
            "left in place"
        );
        assert!(plan_is_empty(&home), "a second run has nothing to do");
    }

    fn plan_is_empty(home: &JobsHome) -> bool {
        plan(home).unwrap().is_empty()
    }

    #[test]
    fn existing_new_log_is_kept_after_the_old_records() {
        let (_dir, home) = legacy_home();
        let newer = "{\"job\":\"daily\",\"trigger\":\"manual\",\"attempt\":1,\"max_attempts\":1,\"started_at\":\"2026-10-14T11:00:00Z\",\"finished_at\":\"2026-10-14T11:00:10Z\",\"duration_ms\":10000,\"ok\":true}\n";
        fs::write(home.runs_log(), newer).unwrap();
        apply(&home, &plan(&home).unwrap()).unwrap();
        let records = home.read_run_records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].started_at, "2026-10-01T11:00:00Z");
        assert_eq!(records[1].started_at, "2026-10-14T11:00:00Z");
    }

    #[test]
    fn rerun_after_a_partial_migration_does_not_duplicate_history() {
        let (_dir, home) = legacy_home();
        let plan_before = plan(&home).unwrap();
        // Simulate a crash after the new log was written but before the old
        // one was renamed.
        fs::write(home.runs_log(), convert_runs_log(OLD_LOG).unwrap()).unwrap();
        apply(&home, &plan_before).unwrap();
        assert_eq!(home.read_run_records().unwrap().len(), 1);
    }

    #[test]
    fn a_name_clash_stops_before_anything_moves() {
        let (_dir, home) = legacy_home();
        fs::create_dir_all(home.jobs_dir()).unwrap();
        fs::write(home.jobs_dir().join("daily.md"), "---\n---\nNewer.\n").unwrap();
        let err = plan(&home).unwrap_err();
        assert!(err.to_string().contains("already exist"), "{err}");
        assert!(home.legacy_dir().join("daily.md").exists());
        assert!(home.legacy_runs_log().exists());
    }

    #[test]
    fn an_unparseable_old_log_stops_the_migration() {
        let (_dir, home) = legacy_home();
        fs::write(home.legacy_runs_log(), "not json\n").unwrap();
        assert!(plan(&home).is_err());
        assert!(home.legacy_dir().join("daily.md").exists());
    }
}
