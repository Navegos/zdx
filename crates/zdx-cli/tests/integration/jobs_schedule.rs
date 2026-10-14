//! Tests for scheduled and once jobs through the CLI: consuming a once job,
//! `--keep`, the per-job lock, invalid files, the daemon (run, missed,
//! interrupted), and `zdx jobs migrate`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta, Utc};
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use crate::fixtures::{MOCK_MODEL, sse_response, text_sse};

fn can_bind_localhost() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

fn write_job(zdx_home: &Path, name: &str, frontmatter: &str) -> PathBuf {
    let dir = zdx_home.join("jobs");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.md"));
    fs::write(&path, format!("---\n{frontmatter}---\nSay hello.\n")).unwrap();
    path
}

fn records(zdx_home: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(zdx_home.join("jobs_runs.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn claim(zdx_home: &Path, job: &str, key: &str) -> Option<serde_json::Value> {
    let path = zdx_home
        .join("state/jobs")
        .join(job)
        .join("claims")
        .join(format!("{key}.json"));
    fs::read_to_string(path)
        .ok()
        .map(|text| serde_json::from_str(&text).unwrap())
}

fn list_json(zdx_home: &Path, root: &Path) -> Vec<serde_json::Value> {
    let output = crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home)
        .current_dir(root)
        .args(["--root", root.to_str().unwrap(), "jobs", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice::<serde_json::Value>(&output)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

async fn mock_provider(expected_requests: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(&text_sse("hello")))
        .expect(expected_requests)
        .mount(&server)
        .await;
    server
}

/// A manual run consumes a once job (claim + archive) unless `--keep`, and a
/// restored copy of the same occurrence does not run again.
#[tokio::test]
async fn test_manual_run_consumes_a_once_job_and_keep_does_not() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let server = mock_provider(2).await;
    let frontmatter = format!("model: \"{MOCK_MODEL}\"\nrun_at: \"2030-01-02T09:00:00-03:00\"\n");
    let job_path = write_job(zdx_home.path(), "remind", &frontmatter);
    let run = |extra: &[&str]| {
        let mut cmd = crate::fixtures::zdx_cmd();
        cmd.env("ZDX_HOME", zdx_home.path())
            .env("ANTHROPIC_API_KEY", "test-api-key")
            .env("ANTHROPIC_BASE_URL", server.uri())
            .current_dir(root.path())
            .args(["--root", root.path().to_str().unwrap(), "--no-thread"])
            .args(["jobs", "run", "remind"])
            .args(extra);
        cmd.assert()
    };

    run(&["--keep"]).success();
    assert!(job_path.exists(), "--keep leaves the job scheduled");
    assert!(claim(zdx_home.path(), "remind", "20300102T120000Z").is_none());
    let status = &list_json(zdx_home.path(), root.path())[0];
    assert_eq!(status["status"], "pending");
    assert_eq!(status["last_run"]["outcome"], "succeeded");

    run(&[])
        .success()
        .stdout(predicate::str::contains("Archived 'remind'"));
    assert!(!job_path.exists());
    let archived = zdx_home
        .path()
        .join("jobs/archive/remind--20300102T120000Z.md");
    assert!(archived.exists());
    let taken = claim(zdx_home.path(), "remind", "20300102T120000Z").unwrap();
    assert_eq!(taken["outcome"], "succeeded");
    assert_eq!(taken["trigger"], "manual");

    let all = records(zdx_home.path());
    assert_eq!(all.len(), 2);
    assert!(
        all[0]["scheduled_for"].is_null(),
        "a --keep run takes no occurrence"
    );
    assert_eq!(all[1]["scheduled_for"], "2030-01-02T12:00:00+00:00");
    assert_eq!(all[1]["schedule_kind"], "once");
    assert_eq!(all[1]["outcome"], "succeeded");
    assert_eq!(all[1]["run_id"], taken["run_id"]);

    let output = crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .args(["jobs", "list", "--archived", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(listed[0]["name"], "remind");
    assert_eq!(listed[0]["key"], "20300102T120000Z");
    assert_eq!(listed[0]["last_run"]["outcome"], "succeeded");
    assert_eq!(listed[0]["last_run"]["trigger"], "manual");

    // Restoring the same file does not make the occurrence run twice.
    write_job(zdx_home.path(), "remind", &frontmatter);
    assert_eq!(
        list_json(zdx_home.path(), root.path())[0]["status"],
        "finished"
    );
    run(&[])
        .failure()
        .stderr(predicate::str::contains("already ran for run_at"));
}

/// A second run of a job that holds its lock exits with the busy code.
#[test]
fn test_run_refuses_while_the_job_lock_is_held() {
    let zdx_home = TempDir::new().unwrap();
    write_job(zdx_home.path(), "busy", "");
    let lock_dir = zdx_home.path().join("state/jobs/busy");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock = fs::File::create(lock_dir.join("lock")).unwrap();
    lock.try_lock().unwrap();

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .args(["--no-thread", "jobs", "run", "busy"])
        .assert()
        .code(75)
        .stderr(predicate::str::contains("already running"));
    assert!(records(zdx_home.path()).is_empty());
}

/// One broken file is listed with its error and does not hide the others;
/// validate reports it and fails.
#[test]
fn test_invalid_job_file_fails_alone() {
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    write_job(zdx_home.path(), "fine", "schedule: \"0 8 * * *\"\n");
    write_job(zdx_home.path(), "typo", "schedul: \"0 8 * * *\"\n");
    write_job(
        zdx_home.path(),
        "no-offset",
        "run_at: \"2030-01-02T09:00:00\"\n",
    );

    let list = list_json(zdx_home.path(), root.path());
    let find = |name: &str| list.iter().find(|item| item["name"] == name).unwrap();
    assert_eq!(find("fine")["status"], "scheduled");
    assert_eq!(find("fine")["schedule"]["kind"], "recurring");
    assert!(find("fine")["next_run_at"].is_string());
    assert_eq!(find("typo")["status"], "invalid");
    assert!(
        find("typo")["error"]
            .as_str()
            .unwrap()
            .contains("unknown field")
    );
    assert!(
        find("no-offset")["error"]
            .as_str()
            .unwrap()
            .contains("explicit offset")
    );

    crate::fixtures::zdx_cmd()
        .env("ZDX_HOME", zdx_home.path())
        .current_dir(root.path())
        .args(["--root", root.path().to_str().unwrap(), "jobs", "validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("2 of 3 job(s) failed validation"))
        .stderr(predicate::str::contains("typo"));
}

fn rfc3339(at: chrono::DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The claim a run leaves behind when its process dies mid-run.
fn write_unfinished_claim(zdx_home: &Path, job: &str, at: chrono::DateTime<Utc>) {
    let key = at.format("%Y%m%dT%H%M%SZ").to_string();
    let claims = zdx_home.join("state/jobs").join(job).join("claims");
    fs::create_dir_all(&claims).unwrap();
    fs::write(
        claims.join(format!("{key}.json")),
        serde_json::json!({
            "job": job,
            "key": key,
            "run_id": "run-from-a-dead-daemon",
            "trigger": "daemon",
            "schedule_kind": "once",
            "scheduled_for": rfc3339(at),
            "claimed_at": rfc3339(at),
            "pid": 1,
            "outcome": null,
            "finished_at": null,
            "error": null,
            "thread_id": null,
        })
        .to_string(),
    )
    .unwrap();
}

/// Runs `zdx jobs daemon` until `done` exists (at most 45 s), then stops it
/// and returns its combined output.
async fn run_daemon_until(zdx_home: &Path, root: &Path, provider: &str, done: &Path) -> String {
    run_daemon_while(zdx_home, root, provider, || !done.exists()).await
}

/// Runs `zdx jobs daemon` while `waiting()` holds (at most 45 s), then stops
/// it and returns its combined output.
async fn run_daemon_while(
    zdx_home: &Path,
    root: &Path,
    provider: &str,
    waiting: impl Fn() -> bool,
) -> String {
    let out = zdx_home.join("daemon.out");
    let err = zdx_home.join("daemon.err");
    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_zdx"))
        .env("ZDX_HOME", zdx_home)
        .env("ANTHROPIC_API_KEY", "test-api-key")
        .env("ANTHROPIC_BASE_URL", provider)
        .current_dir(root)
        .args(["--root", root.to_str().unwrap(), "--no-thread"])
        .args(["jobs", "daemon", "--poll-interval-secs", "1"])
        .stdin(Stdio::null())
        .stdout(fs::File::create(&out).unwrap())
        .stderr(fs::File::create(&err).unwrap())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn zdx jobs daemon");
    let mut waited = Duration::ZERO;
    while waiting() && waited < Duration::from_secs(45) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        waited += Duration::from_millis(250);
    }
    daemon.kill().await.unwrap();
    format!(
        "{}{}",
        fs::read_to_string(&out).unwrap_or_default(),
        fs::read_to_string(&err).unwrap_or_default()
    )
}

/// The daemon runs a due once job in a child process and archives it, marks
/// a run that died with the previous daemon as interrupted, records a once
/// job past its grace as missed, and keeps going despite an invalid file.
#[tokio::test]
async fn test_daemon_runs_misses_and_recovers_once_jobs() {
    if !can_bind_localhost() {
        eprintln!("Skipping: cannot bind localhost TCP port in this environment.");
        return;
    }
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let server = mock_provider(1).await;
    let now = Utc::now();
    let soon = now + TimeDelta::seconds(3);
    let soon_key = soon.format("%Y%m%dT%H%M%SZ").to_string();
    write_job(
        zdx_home.path(),
        "remind",
        &format!("model: \"{MOCK_MODEL}\"\nrun_at: \"{}\"\n", rfc3339(soon)),
    );
    let late = now - TimeDelta::hours(7);
    write_job(
        zdx_home.path(),
        "late",
        &format!("model: \"{MOCK_MODEL}\"\nrun_at: \"{}\"\n", rfc3339(late)),
    );
    let crashed_at = now - TimeDelta::minutes(5);
    let crashed_key = crashed_at.format("%Y%m%dT%H%M%SZ").to_string();
    write_job(
        zdx_home.path(),
        "crashed",
        &format!(
            "model: \"{MOCK_MODEL}\"\nrun_at: \"{}\"\n",
            rfc3339(crashed_at)
        ),
    );
    write_unfinished_claim(zdx_home.path(), "crashed", crashed_at);
    // Rescheduled after its run died: the old occurrence is recorded, the
    // file stays for its new run_at.
    write_job(
        zdx_home.path(),
        "moved",
        &format!(
            "model: \"{MOCK_MODEL}\"\nrun_at: \"{}\"\n",
            rfc3339(now + TimeDelta::hours(1))
        ),
    );
    write_unfinished_claim(zdx_home.path(), "moved", crashed_at);
    write_job(zdx_home.path(), "broken", "schedule: \"not cron\"\n");

    let archive = zdx_home.path().join("jobs/archive");
    let done = archive.join(format!("remind--{soon_key}.md"));
    let daemon_log = run_daemon_until(zdx_home.path(), root.path(), &server.uri(), &done).await;
    assert!(
        done.exists(),
        "once job was not run and archived:\n{daemon_log}"
    );

    let remind = claim(zdx_home.path(), "remind", &soon_key).unwrap();
    assert_eq!(remind["outcome"], "succeeded");
    assert_eq!(remind["trigger"], "daemon");

    let crashed = claim(zdx_home.path(), "crashed", &crashed_key).unwrap();
    assert_eq!(crashed["outcome"], "interrupted");
    assert_eq!(crashed["run_id"], "run-from-a-dead-daemon");
    assert!(archive.join(format!("crashed--{crashed_key}.md")).exists());

    let moved = claim(zdx_home.path(), "moved", &crashed_key).unwrap();
    assert_eq!(moved["outcome"], "interrupted");
    assert!(
        zdx_home.path().join("jobs/moved.md").exists(),
        "rescheduled job stays"
    );

    let late_key = late.format("%Y%m%dT%H%M%SZ").to_string();
    assert_eq!(
        claim(zdx_home.path(), "late", &late_key).unwrap()["outcome"],
        "missed"
    );
    assert!(archive.join(format!("late--{late_key}.md")).exists());
    assert!(zdx_home.path().join("jobs/broken.md").exists());

    let all = records(zdx_home.path());
    let outcome_of = |job: &str| {
        all.iter()
            .filter(|record| record["job"] == job)
            .map(|record| record["outcome"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(outcome_of("remind"), ["succeeded"]);
    assert_eq!(outcome_of("crashed"), ["interrupted"]);
    assert_eq!(outcome_of("moved"), ["interrupted"]);
    assert_eq!(outcome_of("late"), ["missed"]);
    assert!(daemon_log.contains("Invalid job file"), "{daemon_log}");
}

/// Old `automations` files block every jobs command until `zdx jobs migrate`
/// moves them; the run history carries over.
#[test]
fn test_migrate_moves_legacy_automations_and_history() {
    let zdx_home = TempDir::new().unwrap();
    let legacy = zdx_home.path().join("automations");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(
        legacy.join("daily.md"),
        "---\nschedule: \"0 8 * * *\"\n---\nReport.\n",
    )
    .unwrap();
    fs::write(
        zdx_home.path().join("automations_runs.jsonl"),
        "{\"automation\":\"daily\",\"trigger\":\"daemon\",\"attempt\":1,\"max_attempts\":1,\"started_at\":\"2026-10-01T11:00:00Z\",\"finished_at\":\"2026-10-01T11:00:10Z\",\"duration_ms\":10000,\"ok\":true,\"error\":null,\"schedule\":\"0 8 * * *\",\"model\":null,\"subagent\":null}\n",
    )
    .unwrap();
    let jobs = |args: &[&str]| {
        let mut cmd = crate::fixtures::zdx_cmd();
        cmd.env("ZDX_HOME", zdx_home.path()).arg("jobs").args(args);
        cmd.assert()
    };

    jobs(&["list"])
        .failure()
        .stderr(predicate::str::contains("zdx jobs migrate"));
    jobs(&["migrate", "--dry-run"])
        .success()
        .stdout(predicate::str::contains("Dry run"));
    assert!(legacy.join("daily.md").exists());

    jobs(&["migrate"]).success();
    assert!(!legacy.exists());
    assert!(zdx_home.path().join("jobs/daily.md").exists());
    assert!(
        zdx_home
            .path()
            .join("automations_runs.jsonl.migrated")
            .exists()
    );
    jobs(&["list"])
        .success()
        .stdout(predicate::str::contains("daily - 0 8 * * * - scheduled"));
    jobs(&["runs", "daily"])
        .success()
        .stdout(predicate::str::contains("daily | daemon | ok"));
    jobs(&["migrate"])
        .success()
        .stdout(predicate::str::contains("Nothing to migrate."));
}

/// A recurring occurrence that passed its grace while the daemon was down is
/// recorded as missed once, and not again when the daemon restarts from a
/// scheduler state saved before that record (a crash between the two).
#[tokio::test]
async fn test_daemon_records_a_recurring_miss_once_across_restarts() {
    let zdx_home = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let missed_at = chrono::Local::now() - TimeDelta::minutes(30);
    write_job(
        zdx_home.path(),
        "daily",
        &format!(
            "schedule: \"{} {} * * *\"\n",
            missed_at.format("%-M"),
            missed_at.format("%-H")
        ),
    );
    let state_dir = zdx_home.path().join("state/jobs");
    fs::create_dir_all(&state_dir).unwrap();
    let down_since = rfc3339(Utc::now() - TimeDelta::hours(2));
    let seed =
        serde_json::json!({ "since": down_since, "last_checked_at": down_since }).to_string();
    let scheduler = state_dir.join("scheduler.json");
    fs::write(&scheduler, &seed).unwrap();
    let checkpoint = state_dir.join("daily/missed.json");
    let no_provider = "http://127.0.0.1:9";

    let log = run_daemon_until(zdx_home.path(), root.path(), no_provider, &checkpoint).await;
    assert!(checkpoint.exists(), "{log}");
    let missed = |all: &[serde_json::Value]| {
        all.iter()
            .filter(|record| record["job"] == "daily" && record["outcome"] == "missed")
            .count()
    };
    let first = records(zdx_home.path());
    assert_eq!(missed(&first), 1, "{first:?}");
    assert!(
        first[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("1 occurrence(s)")
    );
    assert_eq!(
        list_json(zdx_home.path(), root.path())[0]["last_run"]["outcome"],
        "missed"
    );

    fs::write(&scheduler, &seed).unwrap();
    let ticked = || {
        fs::read_to_string(&scheduler)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|state| state["last_checked_at"] != down_since.as_str())
    };
    let log = run_daemon_while(zdx_home.path(), root.path(), no_provider, || !ticked()).await;
    assert!(ticked(), "{log}");
    assert_eq!(missed(&records(zdx_home.path())), 1, "{log}");
}
