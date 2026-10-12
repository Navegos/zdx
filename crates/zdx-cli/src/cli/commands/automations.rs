//! Automation command handlers.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use zdx_engine::automations::{self, AutomationDefinition, AutomationRunSettings};
use zdx_engine::core::context::{EffectivePrompt, PromptContextInclusion};
use zdx_engine::core::thread_persistence::ThreadPersistenceOptions;
use zdx_engine::{config, prompts, subagents};

use super::exec;

/// Trigger source for automation runs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunTrigger {
    /// User-triggered run via `zdx automations run <name>`.
    Manual,
    /// Scheduled run via `zdx automations daemon`.
    Daemon,
}

#[derive(Debug, Serialize, Deserialize)]
struct AutomationRunRecord {
    automation: String,
    trigger: RunTrigger,
    thread_id: Option<String>,
    attempt: u32,
    max_attempts: u32,
    started_at: String,
    finished_at: String,
    duration_ms: u64,
    ok: bool,
    error: Option<String>,
    schedule: Option<String>,
    model: Option<String>,
    subagent: Option<String>,
    /// Root the run used; absent in records written before roots were
    /// recorded, and when the root could not be resolved.
    #[serde(default)]
    root: Option<String>,
}

#[derive(Debug)]
struct PreparedAutomationRun {
    config: config::Config,
    settings: AutomationRunSettings,
    effective_prompt: EffectivePrompt,
    tools_override: Option<String>,
}

/// Automations are one-shot non-interactive runs, so they carry the exec layer and
/// add the automation-specific rules on top of it.
fn automation_instruction_layers() -> [&'static str; 2] {
    [
        prompts::EXEC_INSTRUCTION_LAYER,
        prompts::AUTOMATION_INSTRUCTION_LAYER,
    ]
}

/// Who is asking for an automation run or listing: the caller's root and
/// config (already carrying CLI overrides), plus the raw `--system-prompt`
/// override so a run that reloads config for a declared `root` keeps it.
pub struct Caller<'a> {
    pub root: &'a Path,
    pub config: &'a config::Config,
    pub system_prompt_override: Option<&'a str>,
}

fn automation_prompt_context() -> PromptContextInclusion {
    PromptContextInclusion {
        project_context: true,
        memory_index: true,
        skills: true,
    }
}

const ERROR_SUMMARY_MAX_LEN: usize = 400;

/// Options for listing automation runs.
#[derive(Debug, Clone, Default)]
pub struct RunsOptions {
    pub name: Option<String>,
    pub date: Option<String>,
    pub date_start: Option<String>,
    pub date_end: Option<String>,
    pub json: bool,
}

/// Prints automation run history from JSONL.
pub fn runs(options: RunsOptions) -> Result<()> {
    let RunsOptions {
        name,
        date,
        date_start,
        date_end,
        json,
    } = options;

    let exact_date = parse_run_date_filter(date.as_deref(), "date")?;
    let start_date = parse_run_date_filter(date_start.as_deref(), "date-start")?;
    let end_date = parse_run_date_filter(date_end.as_deref(), "date-end")?;
    if let (Some(start), Some(end)) = (start_date, end_date)
        && start > end
    {
        anyhow::bail!("--date-start must be on or before --date-end");
    }

    let path = runs_log_path();
    if !path.exists() {
        println!("No automation runs found.");
        return Ok(());
    }

    let records = read_run_records(&path)?;
    let filtered_by_name: Vec<&AutomationRunRecord> = if let Some(raw) = name.as_deref() {
        let needle = raw.trim();
        if needle.is_empty() {
            Vec::new()
        } else {
            records.iter().filter(|r| r.automation == needle).collect()
        }
    } else {
        records.iter().collect()
    };

    let filtered: Vec<&AutomationRunRecord> = filtered_by_name
        .into_iter()
        .filter(|record| matches_run_date_filters(record, exact_date, start_date, end_date))
        .collect();

    if filtered.is_empty() {
        if let Some(name) = name.as_deref() {
            println!("No runs found for automation '{}'.", name.trim());
        } else {
            println!("No automation runs found.");
        }
        return Ok(());
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&filtered).context("serialize automation runs")?
        );
        return Ok(());
    }

    for record in filtered {
        let thread_display = record.thread_id.as_deref().unwrap_or("-");
        println!(
            "{} | {} | {} | {} | {}ms | attempt {}/{} | thread {}",
            record.finished_at,
            record.automation,
            match record.trigger {
                RunTrigger::Manual => "manual",
                RunTrigger::Daemon => "daemon",
            },
            if record.ok { "ok" } else { "failed" },
            record.duration_ms,
            record.attempt,
            record.max_attempts,
            thread_display
        );
        if let Some(err) = &record.error {
            println!("  error: {err}");
        }
    }

    Ok(())
}

/// Lists discovered automations. `json` prints the shared resolved overview
/// (declared and effective root/model per automation).
pub fn list(caller: &Caller<'_>, json: bool) -> Result<()> {
    let (root, config) = (caller.root, caller.config);
    let automations = automations::discover(root)
        .with_context(|| format!("discover automations from {}", root.display()))?;

    if json {
        let overviews: Vec<_> = automations
            .iter()
            .map(|automation| automations::overview(config, root, automation))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&overviews).context("serialize automations")?
        );
        return Ok(());
    }

    if automations.is_empty() {
        println!("No automations found.");
        return Ok(());
    }

    for automation in automations {
        println!(
            "{} ({}) - {}",
            automation.name,
            automation.source.as_str(),
            automation.schedule.as_deref().unwrap_or("manual")
        );
    }

    Ok(())
}

/// Validates all discovered automations.
pub fn validate(caller: &Caller<'_>) -> Result<()> {
    let root = caller.root;
    let automations = automations::discover(root)
        .with_context(|| format!("discover automations from {}", root.display()))?;

    let mut prepared = Vec::with_capacity(automations.len());
    for automation in &automations {
        let run = prepare_automation_run(caller, automation)
            .with_context(|| format!("validate automation '{}'", automation.name))?;
        prepared.push(run.settings);
    }

    println!("Validated {} automation(s).", automations.len());
    for (automation, settings) in automations.iter().zip(&prepared) {
        print_validation_line(automation, settings);
    }

    Ok(())
}

/// Runs one automation by name.
pub async fn run(
    caller: &Caller<'_>,
    thread_opts: &ThreadPersistenceOptions,
    name: &str,
) -> Result<()> {
    let automation = automations::load_by_name(caller.root, name)
        .with_context(|| format!("load automation '{name}' from {}", caller.root.display()))?;

    Box::pin(run_definition(
        caller,
        thread_opts,
        &automation,
        RunTrigger::Manual,
    ))
    .await
}

/// Runs one parsed automation definition.
///
/// # Errors
/// Returns an error when execution fails after retries.
pub async fn run_definition(
    caller: &Caller<'_>,
    thread_opts: &ThreadPersistenceOptions,
    automation: &AutomationDefinition,
    trigger: RunTrigger,
) -> Result<()> {
    let attempts = automation.max_retries.saturating_add(1);
    let effective_thread_opts =
        resolve_automation_thread_opts(thread_opts, &automation.name, Utc::now());

    for attempt in 1..=attempts {
        let started_at = Utc::now();
        let started = Instant::now();

        // A run that cannot be prepared (missing root, bad subagent) is
        // recorded as failed and not retried: nothing transient can fix it,
        // and it must never fall back to another root.
        let (result, settings) = match prepare_automation_run(caller, automation) {
            Ok(prepared) => {
                let root_string = prepared.settings.root.path.to_string_lossy().to_string();
                let result = Box::pin(exec::run(exec::ExecRunOptions {
                    root: &root_string,
                    thread_opts: &effective_thread_opts,
                    prompt: &automation.prompt,
                    config: &prepared.config,
                    model_override: Some(prepared.settings.model.spec.as_str()),
                    effective_prompt_override: Some(&prepared.effective_prompt),
                    tool_timeout_override: automation.timeout_secs,
                    event_filter_override: None,
                    stream: false,
                    tools_override: prepared.tools_override.as_deref(),
                    no_tools: false,
                    no_system_prompt: false,
                    no_skills: false,
                    subagent: None,
                    activity_kind: Some("automation"),
                    activity_parent_thread_id: None,
                    activity_subagent_name: None,
                }))
                .await;
                (result, Some(prepared.settings))
            }
            Err(err) => (Err(err.context("prepare automation run")), None),
        };
        let prepared_ok = settings.is_some();

        let finished_at = Utc::now();
        let elapsed_ms = started.elapsed().as_millis();
        let duration_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
        let error = result.as_ref().err().map(summarize_error);

        let record = AutomationRunRecord {
            automation: automation.name.clone(),
            trigger,
            thread_id: effective_thread_opts.thread_id.clone(),
            attempt,
            max_attempts: attempts,
            started_at: started_at.to_rfc3339(),
            finished_at: finished_at.to_rfc3339(),
            duration_ms,
            ok: result.is_ok(),
            error,
            schedule: automation.schedule.clone(),
            model: settings.as_ref().map(|s| s.model.spec.clone()),
            subagent: automation.subagent.clone(),
            root: settings
                .as_ref()
                .map(|s| s.root.path.to_string_lossy().to_string()),
        };

        if let Err(err) = append_run_record(&record) {
            tracing::warn!(
                err = format!("{err:#}"),
                "Failed to append automation run log"
            );
        }

        match result {
            Ok(()) => return Ok(()),
            Err(err) if prepared_ok && attempt < attempts => {
                tracing::warn!(name = %automation.name, attempt, attempts, err = format!("{err:#}"), "Automation failed, retrying");
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "automation '{}' failed after {attempt} attempt(s)",
                        automation.name
                    )
                });
            }
        }
    }

    Ok(())
}

fn prepare_automation_run(
    caller: &Caller<'_>,
    automation: &AutomationDefinition,
) -> Result<PreparedAutomationRun> {
    let mut resolved = automations::resolve_run(caller.config, caller.root, automation)?;
    crate::cli::apply_system_prompt_override(&mut resolved.config, caller.system_prompt_override);
    let run_root = resolved.settings.root.path.clone();
    let model = resolved.config.model.clone();
    let instruction_layers = automation_instruction_layers();

    let (effective_prompt, tools_override) = match resolved.subagent.as_ref() {
        Some(definition) => {
            let (prompt, runtime_context) = subagents::render_prompt_with_layers(
                &resolved.config,
                &run_root,
                definition,
                &model,
                automation_prompt_context(),
                &instruction_layers,
            )?;
            (
                EffectivePrompt {
                    prompt: Some(prompt),
                    runtime_context,
                    ..EffectivePrompt::default()
                },
                definition.tools.as_ref().map(|tools| tools.join(",")),
            )
        }
        None => (
            zdx_engine::core::context::build_prompt_with_context_and_layers(
                &resolved.config,
                &run_root,
                &model,
                &instruction_layers,
                automation_prompt_context(),
            )
            .context("render automation prompt")?,
            None,
        ),
    };

    Ok(PreparedAutomationRun {
        config: resolved.config,
        settings: resolved.settings,
        effective_prompt,
        tools_override,
    })
}

fn resolve_automation_thread_opts(
    thread_opts: &ThreadPersistenceOptions,
    automation_name: &str,
    now: DateTime<Utc>,
) -> ThreadPersistenceOptions {
    if thread_opts.no_save || thread_opts.thread_id.is_some() {
        return thread_opts.clone();
    }

    let default_thread_id = format!("automation-{automation_name}-{}", now.format("%Y%m%d-%H%M"));

    ThreadPersistenceOptions {
        thread_id: Some(default_thread_id),
        no_save: false,
        ..Default::default()
    }
}

fn print_validation_line(automation: &AutomationDefinition, settings: &AutomationRunSettings) {
    println!(
        "- {} ({}): schedule={}, model={} [{}], root={} [{}], subagent={}, timeout_secs={}, max_retries={}",
        automation.name,
        automation.source.as_str(),
        automation.schedule.as_deref().unwrap_or("manual"),
        settings.model.spec,
        settings.model.origin.as_str(),
        settings.root.path.display(),
        settings.root.origin.as_str(),
        automation.subagent.as_deref().unwrap_or("<default>"),
        automation
            .timeout_secs
            .map_or_else(|| "<default>".to_string(), |v| v.to_string()),
        automation.max_retries
    );
}

fn summarize_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}").replace('\n', " | ");
    if text.len() > ERROR_SUMMARY_MAX_LEN {
        format!("{}...", &text[..ERROR_SUMMARY_MAX_LEN])
    } else {
        text
    }
}

fn parse_run_date_filter(raw: Option<&str>, flag: &str) -> Result<Option<NaiveDate>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--{flag} cannot be empty");
    }

    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .with_context(|| format!("invalid --{flag} value '{trimmed}' (expected YYYY-MM-DD)"))
        .map(Some)
}

fn matches_run_date_filters(
    record: &AutomationRunRecord,
    exact: Option<NaiveDate>,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
) -> bool {
    if exact.is_none() && start.is_none() && end.is_none() {
        return true;
    }

    let Some(record_date) = record_finished_date(record) else {
        return false;
    };

    if let Some(exact) = exact
        && record_date != exact
    {
        return false;
    }
    if let Some(start) = start
        && record_date < start
    {
        return false;
    }
    if let Some(end) = end
        && record_date > end
    {
        return false;
    }

    true
}

fn record_finished_date(record: &AutomationRunRecord) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(&record.finished_at)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).date_naive())
}

fn append_run_record(record: &AutomationRunRecord) -> Result<()> {
    let path = runs_log_path();
    append_run_record_to(&path, record)
}

fn runs_log_path() -> std::path::PathBuf {
    config::paths::zdx_home().join("automations_runs.jsonl")
}

fn append_run_record_to(path: &Path, record: &AutomationRunRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create automation runs dir {}", parent.display()))?;
    }

    let line = serde_json::to_string(record).context("serialize automation run record")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open automation runs log {}", path.display()))?;
    writeln!(file, "{line}")
        .with_context(|| format!("append automation run log {}", path.display()))?;
    Ok(())
}

fn read_run_records(path: &Path) -> Result<Vec<AutomationRunRecord>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read automation runs log {}", path.display()))?;

    let mut records = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record = serde_json::from_str::<AutomationRunRecord>(trimmed).with_context(|| {
            format!(
                "parse automation run record at {} line {}",
                path.display(),
                idx + 1
            )
        })?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::tempdir;
    use zdx_engine::automations::AutomationSource;
    use zdx_engine::config::{Config, SkillSourceToggles, ThinkingLevel};

    use super::*;

    #[test]
    fn append_run_record_writes_jsonl_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("runs.jsonl");

        let record = AutomationRunRecord {
            automation: "morning-report".to_string(),
            trigger: RunTrigger::Manual,
            thread_id: Some("automation-morning-report".to_string()),
            attempt: 1,
            max_attempts: 1,
            started_at: "2026-02-11T08:00:00Z".to_string(),
            finished_at: "2026-02-11T08:00:03Z".to_string(),
            duration_ms: 3000,
            ok: true,
            error: None,
            schedule: Some("0 8 * * *".to_string()),
            model: Some("gemini:gemini-2.5-flash".to_string()),
            subagent: Some("task".to_string()),
            root: Some("/tmp/demo-root".to_string()),
        };

        append_run_record_to(&path, &record).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["automation"], "morning-report");
        assert_eq!(parsed["trigger"], "manual");
        assert_eq!(parsed["thread_id"], "automation-morning-report");
        assert_eq!(parsed["ok"], true);
    }

    #[test]
    fn resolve_automation_thread_opts_defaults_to_prefixed_thread() {
        let opts = ThreadPersistenceOptions {
            thread_id: None,
            no_save: false,
            ..Default::default()
        };

        let now = Utc.with_ymd_and_hms(2026, 2, 12, 7, 45, 0).unwrap();
        let resolved = resolve_automation_thread_opts(&opts, "daily-report", now);
        assert_eq!(
            resolved.thread_id.as_deref(),
            Some("automation-daily-report-20260212-0745")
        );
        assert!(!resolved.no_save);
    }

    #[test]
    fn resolve_automation_thread_opts_daemon_uses_timestamped_thread_id() {
        let opts = ThreadPersistenceOptions {
            thread_id: None,
            no_save: false,
            ..Default::default()
        };

        let now = Utc.with_ymd_and_hms(2026, 2, 12, 7, 45, 0).unwrap();
        let resolved = resolve_automation_thread_opts(&opts, "daily-report", now);
        assert_eq!(
            resolved.thread_id.as_deref(),
            Some("automation-daily-report-20260212-0745")
        );
        assert!(!resolved.no_save);
    }

    #[test]
    fn resolve_automation_thread_opts_honors_no_thread() {
        let opts = ThreadPersistenceOptions {
            thread_id: None,
            no_save: true,
            ..Default::default()
        };

        let now = Utc.with_ymd_and_hms(2026, 2, 12, 7, 45, 0).unwrap();
        let resolved = resolve_automation_thread_opts(&opts, "daily-report", now);
        assert!(resolved.thread_id.is_none());
        assert!(resolved.no_save);
    }

    #[test]
    fn resolve_automation_thread_opts_honors_explicit_thread() {
        let opts = ThreadPersistenceOptions {
            thread_id: Some("custom-thread".to_string()),
            no_save: false,
            ..Default::default()
        };

        let now = Utc.with_ymd_and_hms(2026, 2, 12, 7, 45, 0).unwrap();
        let resolved = resolve_automation_thread_opts(&opts, "daily-report", now);
        assert_eq!(resolved.thread_id.as_deref(), Some("custom-thread"));
        assert!(!resolved.no_save);
    }

    #[test]
    fn read_run_records_parses_jsonl() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("runs.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"automation":"a","trigger":"manual","attempt":1,"max_attempts":1,"started_at":"2026-02-11T08:00:00Z","finished_at":"2026-02-11T08:00:01Z","duration_ms":1000,"ok":true,"error":null,"schedule":null,"model":null,"subagent":null}"#,
                "\n",
                r#"{"automation":"b","trigger":"daemon","attempt":1,"max_attempts":2,"started_at":"2026-02-11T08:01:00Z","finished_at":"2026-02-11T08:01:02Z","duration_ms":2000,"ok":false,"error":"oops","schedule":"0 8 * * *","model":"m","subagent":"task"}"#,
                "\n"
            ),
        )
        .unwrap();

        let records = read_run_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].automation, "a");
        assert_eq!(records[1].automation, "b");
        assert!(!records[1].ok);
    }

    #[test]
    fn parse_run_date_filter_accepts_valid_date() {
        let date = parse_run_date_filter(Some("2026-02-11"), "date").unwrap();
        assert_eq!(
            date,
            Some(chrono::NaiveDate::from_ymd_opt(2026, 2, 11).unwrap())
        );
    }

    #[test]
    fn matches_run_date_filters_uses_finished_at() {
        let record = AutomationRunRecord {
            automation: "daily".to_string(),
            trigger: RunTrigger::Manual,
            thread_id: Some("automation-daily-20260211-0800".to_string()),
            attempt: 1,
            max_attempts: 1,
            started_at: "2026-02-11T08:00:00Z".to_string(),
            finished_at: "2026-02-11T08:00:03Z".to_string(),
            duration_ms: 3000,
            ok: true,
            error: None,
            schedule: None,
            model: None,
            subagent: None,
            root: None,
        };

        let exact = chrono::NaiveDate::from_ymd_opt(2026, 2, 11).unwrap();
        let miss = chrono::NaiveDate::from_ymd_opt(2026, 2, 12).unwrap();

        assert!(matches_run_date_filters(&record, Some(exact), None, None));
        assert!(!matches_run_date_filters(&record, Some(miss), None, None));
        assert!(matches_run_date_filters(
            &record,
            None,
            Some(exact),
            Some(exact)
        ));
    }

    #[test]
    fn prepare_automation_run_uses_effective_model_and_project_context() {
        // Building the prompt materializes bundled skills into $ZDX_HOME.
        let _home = zdx_engine::test_support::temp_zdx_home();
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Automation project note").unwrap();

        let mut config = Config {
            model: "anthropic:claude-opus-4-6".to_string(),
            thinking_level: ThinkingLevel::Low,
            system_prompt: Some("Base prompt".to_string()),
            ..Default::default()
        };
        config.subagents.enabled = false;
        config.skills.sources = SkillSourceToggles {
            zdx_user: false,
            zdx_project: false,
            codex_user: false,
            claude_user: false,
            claude_project: false,
            agents_user: false,
            agents_project: false,
        };

        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: dir.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: Some("openai-codex:gpt-5.4".to_string()),
            subagent: None,
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let prepared =
            prepare_automation_run(&test_caller(&config, dir.path()), &automation).unwrap();
        let prompt = prepared.effective_prompt.prompt.clone().unwrap_or_default();

        assert_eq!(prepared.settings.model.spec, "openai-codex:gpt-5.4@low");
        assert_eq!(
            prepared.settings.model.origin,
            automations::SettingOrigin::Automation
        );
        assert_eq!(
            prepared.settings.model.thinking_origin,
            automations::SettingOrigin::Config
        );
        assert_eq!(prepared.config.model, "openai-codex:gpt-5.4@low");
        assert_eq!(
            prepared.settings.root.origin,
            automations::SettingOrigin::Caller
        );
        assert!(prompt.contains("Automation project note"));
        assert_eq!(prompt.matches(NON_INTERACTIVE_MARKER).count(), 1);
        assert_eq!(prompt.matches(AUTOMATION_MARKER).count(), 1);
        assert!(
            initial_context(&prepared).contains("<runtime_context>"),
            "runtime context (skills, memory index) must reach the run"
        );
    }

    fn test_caller<'a>(config: &'a Config, root: &'a Path) -> Caller<'a> {
        Caller {
            root,
            config,
            system_prompt_override: None,
        }
    }

    fn initial_context(prepared: &PreparedAutomationRun) -> String {
        prepared
            .effective_prompt
            .runtime_context
            .as_ref()
            .and_then(|context| context.initial.clone())
            .unwrap_or_default()
    }

    const NON_INTERACTIVE_MARKER: &str = "This run is non-interactive";
    const AUTOMATION_MARKER: &str = "This run is a ZDX automation";

    fn no_skill_sources() -> SkillSourceToggles {
        SkillSourceToggles {
            zdx_user: false,
            zdx_project: false,
            codex_user: false,
            claude_user: false,
            claude_project: false,
            agents_user: false,
            agents_project: false,
        }
    }

    #[test]
    fn prepare_automation_run_subagent_keeps_non_interactive_layers_and_context() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "workspace file").unwrap();
        let subagents_dir = dir.path().join(".zdx").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        std::fs::write(
            subagents_dir.join("helper.md"),
            "---\ndescription: Helper\nmodel: openai-codex:gpt-5.4@high\ntools:\n  - read\n---\nHelper body sentinel.",
        )
        .unwrap();

        let mut config = Config {
            model: "anthropic:claude-opus-4-6".to_string(),
            ..Default::default()
        };
        config.skills.sources = no_skill_sources();

        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: dir.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: None,
            subagent: Some("helper".to_string()),
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let prepared =
            prepare_automation_run(&test_caller(&config, dir.path()), &automation).unwrap();
        let prompt = prepared.effective_prompt.prompt.clone().unwrap_or_default();

        assert!(prompt.contains("Helper body sentinel."));
        assert_eq!(prompt.matches(NON_INTERACTIVE_MARKER).count(), 1);
        assert_eq!(prompt.matches(AUTOMATION_MARKER).count(), 1);
        assert!(initial_context(&prepared).contains("<runtime_context>"));
        assert_eq!(prepared.tools_override.as_deref(), Some("read"));
        assert_eq!(prepared.settings.model.spec, "openai-codex:gpt-5.4@high");
        assert_eq!(
            prepared.settings.model.origin,
            automations::SettingOrigin::Subagent
        );
    }

    #[test]
    fn prepare_automation_run_uses_declared_root_and_its_config() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let caller = tempdir().unwrap();
        std::fs::write(caller.path().join("AGENTS.md"), "Caller root note").unwrap();
        let declared = tempdir().unwrap();
        std::fs::write(declared.path().join("AGENTS.md"), "Declared root note").unwrap();
        std::fs::create_dir_all(declared.path().join(".zdx")).unwrap();
        std::fs::write(
            declared.path().join(".zdx").join("config.toml"),
            "model = \"openai-codex:gpt-5.4@medium\"\n",
        )
        .unwrap();

        let config = Config {
            model: "anthropic:claude-opus-4-6@high".to_string(),
            ..Default::default()
        };
        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: caller.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: None,
            subagent: None,
            root: Some(declared.path().to_string_lossy().to_string()),
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let prepared =
            prepare_automation_run(&test_caller(&config, caller.path()), &automation).unwrap();
        let prompt = prepared.effective_prompt.prompt.clone().unwrap_or_default();

        assert_eq!(prepared.settings.root.path, declared.path());
        assert_eq!(
            prepared.settings.root.origin,
            automations::SettingOrigin::Automation
        );
        assert_eq!(prepared.settings.model.spec, "openai-codex:gpt-5.4@medium");
        assert_eq!(
            prepared.settings.model.origin,
            automations::SettingOrigin::Config
        );
        assert!(prompt.contains("Declared root note"));
        assert!(!prompt.contains("Caller root note"));
    }

    #[test]
    fn prepare_automation_run_keeps_cli_system_prompt_with_declared_root() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let caller = tempdir().unwrap();
        let declared = tempdir().unwrap();
        std::fs::create_dir_all(declared.path().join(".zdx")).unwrap();
        std::fs::write(
            declared.path().join(".zdx").join("config.toml"),
            "system_prompt = \"WORKSPACE-PROMPT\"\n",
        )
        .unwrap();
        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: caller.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: Some("anthropic:claude-opus-4-6".to_string()),
            subagent: None,
            root: Some(declared.path().to_string_lossy().to_string()),
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };
        let config = Config::default();
        let caller_ctx = Caller {
            root: caller.path(),
            config: &config,
            system_prompt_override: Some("CLI-PROMPT"),
        };

        let prepared = prepare_automation_run(&caller_ctx, &automation).unwrap();
        let prompt = prepared.effective_prompt.prompt.unwrap_or_default();
        assert!(prompt.contains("CLI-PROMPT"));
        assert!(!prompt.contains("WORKSPACE-PROMPT"));
    }

    #[test]
    fn prepare_automation_run_rejects_missing_root() {
        let caller = tempdir().unwrap();
        let missing = caller.path().join("does-not-exist");
        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: caller.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: None,
            subagent: None,
            root: Some(missing.to_string_lossy().to_string()),
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let err =
            prepare_automation_run(&test_caller(&Config::default(), caller.path()), &automation)
                .unwrap_err();
        assert!(
            format!("{err:#}").contains("does not exist or is not a directory"),
            "{err:#}"
        );
    }

    #[test]
    fn validate_fails_for_missing_subagent() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: dir.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: None,
            subagent: Some("missing-subagent".to_string()),
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let Err(err) = prepare_automation_run(&test_caller(&config, dir.path()), &automation)
        else {
            panic!("missing subagent should fail validation");
        };
        assert!(format!("{err:#}").contains("missing-subagent"));
    }

    #[test]
    fn validate_fails_for_subagent_with_unknown_tool() {
        let dir = tempdir().unwrap();
        let subagents_dir = dir.path().join(".zdx").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        std::fs::write(
            subagents_dir.join("broken.md"),
            "---\ndescription: Broken\ntools:\n  - reed\n---\nPrompt",
        )
        .unwrap();

        let config = Config::default();
        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: dir.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: None,
            subagent: Some("broken".to_string()),
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let err =
            prepare_automation_run(&test_caller(&config, dir.path()), &automation).unwrap_err();
        assert!(format!("{err:#}").contains("Unknown tool(s): reed"));
    }

    #[test]
    fn prepare_automation_run_rejects_unknown_subagent() {
        let dir = tempdir().unwrap();
        let config = Config {
            model: "anthropic:claude-opus-4-6".to_string(),
            ..Default::default()
        };

        let automation = AutomationDefinition {
            name: "demo".to_string(),
            path: dir.path().join("demo.md"),
            source: AutomationSource::User,
            schedule: None,
            model: None,
            subagent: Some("task".to_string()),
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "Do the thing".to_string(),
        };

        let err =
            prepare_automation_run(&test_caller(&config, dir.path()), &automation).unwrap_err();
        assert!(format!("{err:#}").contains("load subagent 'task'"));
    }
}
