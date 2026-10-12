//! Daemon command handler for scheduled automations.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use zdx_engine::automations::AutomationDefinition;
use zdx_engine::core::thread_persistence::ThreadPersistenceOptions;
use zdx_engine::{automation_daemon, automations, config};

use super::automations as automation_commands;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DaemonState {
    /// Last minute-bucket run per automation name.
    last_run_minute: BTreeMap<String, i64>,
}

/// Runs the daemon loop for scheduled automations.
///
/// # Errors
/// Returns an error if state loading/writing fails.
pub async fn run(
    caller: &automation_commands::Caller<'_>,
    thread_opts: &ThreadPersistenceOptions,
    poll_interval_secs: u64,
) -> Result<()> {
    let root = caller.root;
    let poll = Duration::from_secs(poll_interval_secs.max(1));
    zdx_engine::pidfile::ensure_unique("daemon")?;
    let state_path = daemon_state_path();
    let mut state = load_state(&state_path)?;
    let _pid_guard = zdx_engine::pidfile::write("daemon").context("write daemon PID file")?;
    let config_dir = std::env::current_dir().context("read daemon working directory")?;
    let _record_guard = automation_daemon::record_running(root, &config_dir)
        .context("write daemon context record")?;

    tracing::info!(root = %root.display(), poll_secs = poll.as_secs(), "Daemon started");

    loop {
        let now = Local::now();
        let current_bucket = now.timestamp() / 60;

        let due: Vec<_> = match automations::discover(root) {
            Ok(defs) => defs
                .into_iter()
                .filter(|automation| is_due(automation, now, &state, current_bucket))
                .collect(),
            Err(err) => {
                tracing::error!(err = format!("{err:#}"), "Failed to discover automations");
                Vec::new()
            }
        };

        if !due.is_empty() {
            // Reload config for every due tick, so runs (and every surface that
            // shows the daemon's settings) use the config files as they are now,
            // not as they were when the daemon started.
            match load_config(caller.system_prompt_override) {
                Ok(config) => {
                    let tick_caller = automation_commands::Caller {
                        root,
                        config: &config,
                        system_prompt_override: caller.system_prompt_override,
                    };
                    for automation in due {
                        tracing::info!(name = %automation.name, "Running automation");
                        match automation_commands::run_definition(
                            &tick_caller,
                            thread_opts,
                            &automation,
                            automation_commands::RunTrigger::Daemon,
                        )
                        .await
                        {
                            Ok(()) => {
                                state
                                    .last_run_minute
                                    .insert(automation.name.clone(), current_bucket);
                                if let Err(err) = save_state(&state_path, &state) {
                                    tracing::error!(
                                        err = format!("{err:#}"),
                                        "Failed to persist daemon state"
                                    );
                                }
                            }
                            Err(err) => {
                                tracing::error!(name = %automation.name, err = format!("{err:#}"), "Automation failed");
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(
                        err = format!("{err:#}"),
                        "Failed to load config for due automations"
                    );
                    for automation in &due {
                        automation_commands::record_failed_start(
                            automation,
                            automation_commands::RunTrigger::Daemon,
                            &err,
                        );
                    }
                }
            }
        }

        tokio::time::sleep(poll).await;
    }
}

fn is_due(
    automation: &AutomationDefinition,
    now: chrono::DateTime<Local>,
    state: &DaemonState,
    current_bucket: i64,
) -> bool {
    let Some(schedule) = automation.schedule.as_deref() else {
        return false;
    };
    match automations::schedule_matches_local_time(schedule, now) {
        Ok(true) => state.last_run_minute.get(&automation.name) != Some(&current_bucket),
        Ok(false) => false,
        Err(err) => {
            tracing::warn!(name = %automation.name, err = format!("{err:#}"), "Invalid schedule for automation");
            false
        }
    }
}

fn load_config(system_prompt_override: Option<&str>) -> Result<config::Config> {
    let mut config = config::Config::load().context("load config")?;
    crate::cli::apply_system_prompt_override(&mut config, system_prompt_override);
    Ok(config)
}

fn daemon_state_path() -> PathBuf {
    config::paths::zdx_home().join("automations_daemon_state.json")
}

fn load_state(path: &Path) -> Result<DaemonState> {
    if !path.exists() {
        return Ok(DaemonState::default());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("read daemon state file {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parse daemon state file {}", path.display()))
}

fn save_state(path: &Path, state: &DaemonState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create daemon state dir {}", parent.display()))?;
    }

    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(state).context("serialize daemon state")?;
    fs::write(&tmp, body).with_context(|| format!("write state temp file {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "persist daemon state from {} to {}",
            tmp.display(),
            path.display()
        )
    })?;

    Ok(())
}
