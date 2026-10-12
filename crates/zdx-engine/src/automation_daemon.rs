//! Where the scheduled automations daemon runs, and the settings it
//! resolves for each automation.
//!
//! Surfaces must show the daemon's context, never their own working
//! directory. The context comes from one of two sources, reported as-is:
//! - `Running`: the live daemon wrote `run/daemon.context.json` at start, and
//!   its PID still owns the daemon PID file.
//! - `Installed`: the launchd plist, which applies at the next start. Used when
//!   the daemon is stopped, or running without a context record (an older
//!   build).
//!
//! Otherwise the context is `Unavailable` with a reason, and automations that
//! do not declare their own `root` stay unresolved instead of falling back to
//! the caller's root.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::automations::{self, AutomationOverview, InheritedContext, SettingOrigin};
use crate::config::{Config, paths};
use crate::pidfile::{self, ServiceStatus};
use crate::service::{self, InstalledLaunch, Service};

const RECORD_FILE: &str = "daemon.context.json";

/// What the running daemon records about itself at start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub pid: u32,
    /// `--root` the daemon resolves inherited automations against.
    pub root: PathBuf,
    /// Working directory the daemon loads config layers from.
    pub config_dir: PathBuf,
    pub started_at: DateTime<Utc>,
}

/// Removes the daemon record when the daemon exits, if it is still ours.
pub struct DaemonRecordGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for DaemonRecordGuard {
    fn drop(&mut self) {
        if read_record_at(&self.path).is_some_and(|record| record.pid == self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Records the current process as the running daemon.
///
/// # Errors
/// Returns an error if the record cannot be written.
pub fn record_running(root: &Path, config_dir: &Path) -> Result<DaemonRecordGuard> {
    let path = record_path();
    let record = DaemonRecord {
        pid: std::process::id(),
        root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
        config_dir: config_dir.to_path_buf(),
        started_at: Utc::now(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create run dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(&record).context("serialize daemon record")?,
    )
    .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))?;
    Ok(DaemonRecordGuard {
        path,
        pid: record.pid,
    })
}

/// Where the daemon's settings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonContextSource {
    /// Reported by the running daemon process.
    Running,
    /// Read from the installed launchd service; applies at its next start.
    Installed,
}

/// The root and config directory the daemon resolves automations with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonContext {
    pub source: DaemonContextSource,
    pub root: PathBuf,
    pub config_dir: PathBuf,
    /// PID of the running daemon, if one is running.
    pub pid: Option<u32>,
    /// Start time reported by the running daemon (`Running` only).
    pub started_at: Option<DateTime<Utc>>,
    /// Why the context is not the running process's own report, if it is not.
    pub note: Option<String>,
}

/// Daemon context, or why it is not known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonContextState {
    Available { context: DaemonContext },
    Unavailable { reason: String },
}

/// Automations resolved the way the scheduled daemon resolves them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonAutomations {
    pub daemon: DaemonContextState,
    pub automations: Vec<AutomationOverview>,
    /// Set when automations could not be listed at all.
    pub error: Option<String>,
}

/// Finds the daemon's context from the PID file, its record, and the
/// installed service.
#[must_use]
pub fn discover() -> DaemonContextState {
    let running_pid = match pidfile::status(Service::Daemon.name()) {
        ServiceStatus::Running { pid, .. } => Some(pid),
        ServiceStatus::Stopped => None,
    };
    let installed = service::installed_launch(Service::Daemon)
        .map(|launch| launch.map_err(|err| format!("{err:#}")));
    discover_from(DiscoveryInputs {
        running_pid,
        record: read_record_at(&record_path()),
        installed,
        zdx_home: paths::zdx_home(),
    })
}

/// Inputs to [`discover`], separated so the decision is testable.
#[derive(Debug, Clone)]
pub struct DiscoveryInputs {
    pub running_pid: Option<u32>,
    pub record: Option<DaemonRecord>,
    /// `None` when no service is installed.
    pub installed: Option<std::result::Result<InstalledLaunch, String>>,
    /// `ZDX_HOME` of the process asking.
    pub zdx_home: PathBuf,
}

/// Decides the daemon context from already-read inputs.
#[must_use]
pub fn discover_from(inputs: DiscoveryInputs) -> DaemonContextState {
    let unavailable = |reason: String| DaemonContextState::Unavailable { reason };

    if let (Some(pid), Some(record)) = (inputs.running_pid, inputs.record.as_ref())
        && record.pid == pid
    {
        return DaemonContextState::Available {
            context: DaemonContext {
                source: DaemonContextSource::Running,
                root: record.root.clone(),
                config_dir: record.config_dir.clone(),
                pid: Some(pid),
                started_at: Some(record.started_at),
                note: None,
            },
        };
    }

    let note = match inputs.running_pid {
        Some(pid) => format!(
            "daemon PID {pid} is running but did not report its settings (it predates \
             this report); showing the installed service, which applies at its next start"
        ),
        None => "daemon is not running; showing the installed service, which applies when \
                 it starts"
            .to_string(),
    };
    let launch = match inputs.installed {
        Some(Ok(launch)) => launch,
        Some(Err(err)) => return unavailable(format!("cannot read the daemon service: {err}")),
        None => {
            return unavailable(match inputs.running_pid {
                Some(pid) => format!(
                    "daemon PID {pid} is running but did not report its settings, and no \
                     daemon service is installed"
                ),
                None => "daemon is not running and no daemon service is installed".to_string(),
            });
        }
    };
    if let Some(home) = &launch.zdx_home
        && home != &inputs.zdx_home
    {
        return unavailable(format!(
            "the daemon service uses ZDX_HOME {}, but this process uses {}",
            home.display(),
            inputs.zdx_home.display()
        ));
    }
    DaemonContextState::Available {
        context: DaemonContext {
            source: DaemonContextSource::Installed,
            root: launch.root,
            config_dir: launch.working_directory,
            pid: inputs.running_pid,
            started_at: None,
            note: Some(note),
        },
    }
}

/// Lists automations resolved with the daemon's context.
#[must_use]
pub fn automations() -> DaemonAutomations {
    automations_for(discover(), &paths::zdx_home().join("automations"))
}

/// Like [`automations`], with an explicit context and automations directory.
#[must_use]
pub fn automations_for(daemon: DaemonContextState, automations_dir: &Path) -> DaemonAutomations {
    let definitions = match automations::discover_with_user_dir(Path::new(""), automations_dir) {
        Ok(definitions) => definitions,
        Err(err) => {
            return DaemonAutomations {
                daemon,
                automations: Vec::new(),
                error: Some(format!("discover automations: {err:#}")),
            };
        }
    };

    let config = match &daemon {
        DaemonContextState::Available { context } => Some(
            Config::load_layered(&paths::config_layer_paths_for(&context.config_dir))
                .map_err(|err| format!("load daemon config: {err:#}")),
        ),
        DaemonContextState::Unavailable { .. } => None,
    };
    let missing = match (&daemon, &config) {
        (DaemonContextState::Unavailable { reason }, _) => {
            Some(format!("daemon settings unavailable: {reason}"))
        }
        (_, Some(Err(err))) => Some(err.clone()),
        _ => None,
    };

    let overviews = definitions
        .iter()
        .map(|definition| {
            let inherited = match (&daemon, &config, &missing) {
                (DaemonContextState::Available { context }, Some(Ok(config)), None) => {
                    Ok(InheritedContext {
                        config,
                        root: &context.root,
                        root_origin: SettingOrigin::Daemon,
                    })
                }
                (_, _, reason) => Err(reason.as_deref().unwrap_or("daemon settings unavailable")),
            };
            automations::overview_from(inherited, definition)
        })
        .collect();

    DaemonAutomations {
        daemon,
        automations: overviews,
        error: None,
    }
}

fn record_path() -> PathBuf {
    paths::zdx_home().join("run").join(RECORD_FILE)
}

fn read_record_at(path: &Path) -> Option<DaemonRecord> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn launch(root: &Path, zdx_home: &Path) -> InstalledLaunch {
        InstalledLaunch {
            root: root.to_path_buf(),
            working_directory: root.to_path_buf(),
            zdx_home: Some(zdx_home.to_path_buf()),
        }
    }

    fn record(pid: u32, root: &Path) -> DaemonRecord {
        DaemonRecord {
            pid,
            root: root.to_path_buf(),
            config_dir: root.to_path_buf(),
            started_at: Utc::now(),
        }
    }

    fn context(state: &DaemonContextState) -> &DaemonContext {
        match state {
            DaemonContextState::Available { context } => context,
            DaemonContextState::Unavailable { reason } => panic!("unavailable: {reason}"),
        }
    }

    #[test]
    fn running_record_wins_over_installed_service() {
        let home = Path::new("/home/example/.zdx");
        let state = discover_from(DiscoveryInputs {
            running_pid: Some(42),
            record: Some(record(42, Path::new("/srv/live"))),
            installed: Some(Ok(launch(Path::new("/srv/installed"), home))),
            zdx_home: home.to_path_buf(),
        });
        let context = context(&state);
        assert_eq!(context.source, DaemonContextSource::Running);
        assert_eq!(context.root, PathBuf::from("/srv/live"));
        assert_eq!(context.pid, Some(42));
        assert!(context.note.is_none());
    }

    #[test]
    fn running_without_record_reports_installed_with_note() {
        let home = Path::new("/home/example/.zdx");
        for stale in [None, Some(record(7, Path::new("/srv/old")))] {
            let state = discover_from(DiscoveryInputs {
                running_pid: Some(42),
                record: stale,
                installed: Some(Ok(launch(Path::new("/srv/installed"), home))),
                zdx_home: home.to_path_buf(),
            });
            let context = context(&state);
            assert_eq!(context.source, DaemonContextSource::Installed);
            assert_eq!(context.root, PathBuf::from("/srv/installed"));
            assert_eq!(context.pid, Some(42));
            assert!(context.note.as_deref().unwrap().contains("next start"));
        }
    }

    #[test]
    fn stopped_daemon_reports_installed_service() {
        let home = Path::new("/home/example/.zdx");
        let state = discover_from(DiscoveryInputs {
            running_pid: None,
            record: Some(record(42, Path::new("/srv/old"))),
            installed: Some(Ok(launch(Path::new("/srv/installed"), home))),
            zdx_home: home.to_path_buf(),
        });
        let context = context(&state);
        assert_eq!(context.source, DaemonContextSource::Installed);
        assert_eq!(context.pid, None);
        assert!(context.note.as_deref().unwrap().contains("not running"));
    }

    #[test]
    fn unknown_context_is_unavailable_never_a_fallback() {
        let home = PathBuf::from("/home/example/.zdx");
        let cases = [
            (None, None, "not running and no daemon service"),
            (Some(42), None, "no daemon service is installed"),
            (None, Some(Err("bad plist".to_string())), "bad plist"),
            (
                None,
                Some(Ok(launch(Path::new("/srv"), Path::new("/other/.zdx")))),
                "ZDX_HOME /other/.zdx",
            ),
        ];
        for (running_pid, installed, expected) in cases {
            let state = discover_from(DiscoveryInputs {
                running_pid,
                record: None,
                installed,
                zdx_home: home.clone(),
            });
            match state {
                DaemonContextState::Unavailable { reason } => {
                    assert!(reason.contains(expected), "{reason}");
                }
                DaemonContextState::Available { .. } => panic!("expected unavailable"),
            }
        }
    }

    #[test]
    fn record_guard_writes_and_removes_own_record() {
        let _home = crate::test_support::temp_zdx_home();
        let root = tempdir().unwrap();
        let guard = record_running(root.path(), root.path()).unwrap();
        let stored = read_record_at(&record_path()).unwrap();
        assert_eq!(stored.pid, std::process::id());
        assert_eq!(stored.root, root.path().canonicalize().unwrap());
        drop(guard);
        assert!(read_record_at(&record_path()).is_none());
    }

    fn write_automation(dir: &Path, name: &str, frontmatter: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join(format!("{name}.md")),
            format!("---\n{frontmatter}---\nprompt"),
        )
        .unwrap();
    }

    fn write_config(dir: &Path, body: &str) {
        fs::create_dir_all(dir.join(".zdx")).unwrap();
        fs::write(dir.join(".zdx").join("config.toml"), body).unwrap();
    }

    fn available(root: &Path, source: DaemonContextSource) -> DaemonContextState {
        DaemonContextState::Available {
            context: DaemonContext {
                source,
                root: root.to_path_buf(),
                config_dir: root.to_path_buf(),
                pid: None,
                started_at: None,
                note: None,
            },
        }
    }

    #[test]
    fn automations_resolve_against_daemon_root_and_config() {
        let home = crate::test_support::temp_zdx_home();
        let daemon_root = tempdir().unwrap();
        write_config(
            daemon_root.path(),
            "model = \"claude-cli@work:claude-opus-5@high\"\n",
        );
        let own_root = tempdir().unwrap();
        write_config(own_root.path(), "model = \"openai-codex:gpt-5.4@low\"\n");

        let dir = home.path().join("automations");
        write_automation(&dir, "inherited", "");
        write_automation(
            &dir,
            "suffixless",
            "model: \"claude-cli:claude-sonnet-5\"\n",
        );
        write_automation(
            &dir,
            "scoped",
            &format!("root: \"{}\"\n", own_root.path().display()),
        );

        let view = automations_for(
            available(daemon_root.path(), DaemonContextSource::Running),
            &dir,
        );
        assert!(view.error.is_none());
        let get = |name: &str| {
            view.automations
                .iter()
                .find(|item| item.name == name)
                .and_then(|item| item.settings.clone())
                .unwrap()
        };

        let inherited = get("inherited");
        assert_eq!(inherited.root.path, daemon_root.path());
        assert_eq!(inherited.root.origin, SettingOrigin::Daemon);
        assert_eq!(inherited.model.spec, "claude-cli@work:claude-opus-5@high");
        assert_eq!(inherited.model.account.as_deref(), Some("work"));
        assert_eq!(inherited.model.thinking, "high");
        assert_eq!(inherited.model.origin, SettingOrigin::Config);

        let suffixless = get("suffixless");
        assert_eq!(suffixless.model.spec, "claude-cli:claude-sonnet-5@high");
        assert_eq!(suffixless.model.account, None);
        assert_eq!(suffixless.model.origin, SettingOrigin::Automation);
        assert_eq!(suffixless.model.thinking_origin, SettingOrigin::Config);

        let scoped = get("scoped");
        assert_eq!(scoped.root.path, own_root.path());
        assert_eq!(scoped.root.origin, SettingOrigin::Automation);
        assert_eq!(scoped.model.spec, "openai-codex:gpt-5.4@low");
    }

    #[test]
    fn unavailable_or_broken_daemon_leaves_only_inherited_jobs_unresolved() {
        let home = crate::test_support::temp_zdx_home();
        let own_root = tempdir().unwrap();
        let dir = home.path().join("automations");
        write_automation(&dir, "inherited", "");
        write_automation(
            &dir,
            "scoped",
            &format!("root: \"{}\"\n", own_root.path().display()),
        );

        let broken_root = tempdir().unwrap();
        write_config(broken_root.path(), "model = [\n");
        let states = [
            (
                DaemonContextState::Unavailable {
                    reason: "daemon is not running".to_string(),
                },
                "daemon settings unavailable: daemon is not running",
            ),
            (
                available(broken_root.path(), DaemonContextSource::Installed),
                "load daemon config",
            ),
        ];
        for (state, expected) in states {
            let view = automations_for(state, &dir);
            let inherited = view
                .automations
                .iter()
                .find(|a| a.name == "inherited")
                .unwrap();
            assert!(inherited.settings.is_none());
            assert!(inherited.error.as_deref().unwrap().contains(expected));
            let scoped = view
                .automations
                .iter()
                .find(|a| a.name == "scoped")
                .unwrap();
            assert_eq!(scoped.settings.as_ref().unwrap().root.path, own_root.path());
        }
    }
}
