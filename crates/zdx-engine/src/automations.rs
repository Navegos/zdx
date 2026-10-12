//! Automation discovery and parsing.
//!
//! Automations are markdown files with YAML frontmatter and prompt body.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};

use crate::config::{Config, paths};
use crate::models::ModelSpec;
use crate::subagents::{self, SubagentDefinition};

/// Source location for an automation definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationSource {
    /// `<ZDX_HOME>/automations/*.md`
    User,
}

impl AutomationSource {
    /// Returns a short display label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
        }
    }
}

/// Parsed automation definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationDefinition {
    /// Stable automation name (file stem).
    pub name: String,
    /// Absolute path to the source markdown file.
    pub path: PathBuf,
    /// Which directory this automation came from.
    pub source: AutomationSource,
    /// Optional schedule expression.
    pub schedule: Option<String>,
    /// Optional model override for this automation run.
    pub model: Option<String>,
    /// Optional named subagent for this automation run.
    pub subagent: Option<String>,
    /// Optional working root, as written in frontmatter (`~/...` or absolute).
    /// Expanded and checked by [`resolve_run`], so a bad root fails only this
    /// automation instead of discovery.
    pub root: Option<String>,
    /// Optional per-run timeout in seconds.
    pub timeout_secs: Option<u32>,
    /// Number of retries after the initial attempt.
    pub max_retries: u32,
    /// Prompt body (markdown content after frontmatter).
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct AutomationFrontmatter {
    schedule: Option<String>,
    model: Option<String>,
    subagent: Option<String>,
    root: Option<String>,
    timeout_secs: Option<u32>,
    max_retries: Option<u32>,
}

/// Which layer supplied a resolved automation setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingOrigin {
    /// The automation's own frontmatter.
    Automation,
    /// The named subagent definition's `model`.
    Subagent,
    /// `[subagents.overrides.<name>]` in config.
    SubagentOverride,
    /// Inherited from the config layers in effect for the run: the caller's
    /// config, or the declared root's layers when `root` is set.
    Config,
    /// Inherited from the caller's root: whoever resolved the settings (CLI
    /// `--root`, the daemon, or a Monitor), not necessarily the daemon.
    Caller,
}

impl SettingOrigin {
    /// Stable label, matching the serialized form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automation => "automation",
            Self::Subagent => "subagent",
            Self::SubagentOverride => "subagent_override",
            Self::Config => "config",
            Self::Caller => "caller",
        }
    }

    /// Whether the value was inherited rather than set for this automation
    /// or its subagent.
    #[must_use]
    pub const fn is_inherited(self) -> bool {
        matches!(self, Self::Config | Self::Caller)
    }
}

/// The working root an automation runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedAutomationRoot {
    pub path: PathBuf,
    /// `Automation` when set in frontmatter, otherwise `Caller`.
    pub origin: SettingOrigin,
}

/// The model an automation runs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedAutomationModel {
    /// Canonical spec with the thinking level inline, e.g.
    /// `claude-cli@work:claude-opus-5-5@high`.
    pub spec: String,
    /// Provider id, or the custom provider's prefix.
    pub provider: String,
    /// Named OAuth account; `None` means the provider's default account.
    pub account: Option<String>,
    /// Bare model id without provider or modifiers.
    pub model: String,
    /// Requested thinking level. Providers may still serve a model without
    /// reasoning at `off`.
    pub thinking: String,
    pub fast: bool,
    pub origin: SettingOrigin,
    /// Layer that supplied the thinking level; differs from `origin` when the
    /// winning spec had no `@<level>` suffix and inherited it.
    pub thinking_origin: SettingOrigin,
}

/// Resolved settings for one automation, shared by every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationRunSettings {
    pub root: ResolvedAutomationRoot,
    pub model: ResolvedAutomationModel,
    pub subagent: Option<String>,
}

/// A resolved automation run: the config to run with plus its reported settings.
#[derive(Debug, Clone)]
pub struct ResolvedAutomationRun {
    pub config: Config,
    pub subagent: Option<SubagentDefinition>,
    pub settings: AutomationRunSettings,
}

/// Declared and resolved view of one automation for listing surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationOverview {
    pub name: String,
    pub path: PathBuf,
    pub schedule: Option<String>,
    pub declared_root: Option<String>,
    pub declared_model: Option<String>,
    pub subagent: Option<String>,
    pub timeout_secs: Option<u32>,
    pub max_retries: u32,
    /// `None` when resolution failed; `error` then says why.
    pub settings: Option<AutomationRunSettings>,
    pub error: Option<String>,
}

/// Resolves the root, config, subagent, and model one automation runs with.
///
/// `base` and `caller_root` are the caller's config and root. Without a
/// `root` in frontmatter the run uses both unchanged. With one, the root must
/// be an existing directory and config layers are loaded for that root, so
/// its workspace overlays apply. Model layers apply in order: config →
/// subagent definition → `[subagents.overrides.<name>]` → automation `model`;
/// a suffixless spec inherits the previous thinking level.
///
/// # Errors
/// Returns an error if the root is invalid or missing, config layers for the
/// root fail to load, or the named subagent cannot be resolved.
pub fn resolve_run(
    base: &Config,
    caller_root: &Path,
    automation: &AutomationDefinition,
) -> Result<ResolvedAutomationRun> {
    let root = resolve_root(
        automation.root.as_deref(),
        caller_root,
        paths::home_dir().as_deref(),
    )?;
    let mut config = if root.origin == SettingOrigin::Automation {
        Config::load_layered(&paths::config_layer_paths_for(&root.path))
            .with_context(|| format!("load config for root {}", root.path.display()))?
    } else {
        base.clone()
    };

    let mut model_origin = SettingOrigin::Config;
    let mut thinking_origin = SettingOrigin::Config;
    let mut apply = |config: &mut Config, spec: &str, origin: SettingOrigin| {
        config.apply_model_spec(spec);
        model_origin = origin;
        if ModelSpec::parse(spec).thinking.is_some() {
            thinking_origin = origin;
        }
    };

    let subagent = match automation.subagent.as_deref() {
        Some(name) => {
            let definition = subagents::resolve_named(&root.path, name, None)
                .with_context(|| format!("load subagent '{name}'"))?;
            if let Some(model) = definition.model.as_deref() {
                apply(&mut config, model, SettingOrigin::Subagent);
            }
            if let Some(model) = config
                .subagents
                .overrides
                .get(name)
                .and_then(|over| over.model.clone())
            {
                apply(&mut config, &model, SettingOrigin::SubagentOverride);
            }
            Some(definition)
        }
        None => None,
    };
    if let Some(model) = automation.model.as_deref() {
        apply(&mut config, model, SettingOrigin::Automation);
    }

    let model = describe_model(&config, model_origin, thinking_origin);
    Ok(ResolvedAutomationRun {
        config,
        subagent,
        settings: AutomationRunSettings {
            root,
            model,
            subagent: automation.subagent.clone(),
        },
    })
}

/// Resolves only the reported settings of one automation.
///
/// # Errors
/// Same as [`resolve_run`].
pub fn resolve_run_settings(
    base: &Config,
    caller_root: &Path,
    automation: &AutomationDefinition,
) -> Result<AutomationRunSettings> {
    resolve_run(base, caller_root, automation).map(|run| run.settings)
}

/// Builds the listing view of one automation. Resolution errors are kept in
/// `error` so one bad automation does not hide the others.
#[must_use]
pub fn overview(
    base: &Config,
    caller_root: &Path,
    automation: &AutomationDefinition,
) -> AutomationOverview {
    let (settings, error) = match resolve_run_settings(base, caller_root, automation) {
        Ok(settings) => (Some(settings), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    AutomationOverview {
        name: automation.name.clone(),
        path: automation.path.clone(),
        schedule: automation.schedule.clone(),
        declared_root: automation.root.clone(),
        declared_model: automation.model.clone(),
        subagent: automation.subagent.clone(),
        timeout_secs: automation.timeout_secs,
        max_retries: automation.max_retries,
        settings,
        error,
    }
}

fn resolve_root(
    declared: Option<&str>,
    caller_root: &Path,
    home: Option<&Path>,
) -> Result<ResolvedAutomationRoot> {
    let Some(raw) = declared else {
        return Ok(ResolvedAutomationRoot {
            path: caller_root.to_path_buf(),
            origin: SettingOrigin::Caller,
        });
    };
    let path = expand_root(raw, home)?;
    if !path.is_dir() {
        bail!(
            "root '{}' does not exist or is not a directory",
            path.display()
        );
    }
    Ok(ResolvedAutomationRoot {
        path,
        origin: SettingOrigin::Automation,
    })
}

fn expand_root(raw: &str, home: Option<&Path>) -> Result<PathBuf> {
    if raw.is_empty() {
        bail!("root cannot be empty");
    }
    let home_required = || {
        home.map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("cannot expand root '{raw}': home directory is unknown"))
    };
    let path = if raw == "~" {
        home_required()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_required()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        bail!("root '{raw}' must be an absolute path or start with '~/'");
    }
    Ok(path)
}

fn describe_model(
    config: &Config,
    origin: SettingOrigin,
    thinking_origin: SettingOrigin,
) -> ResolvedAutomationModel {
    let spec = crate::models::format_model_thinking(&config.model, config.thinking_level);
    let parsed = ModelSpec::parse(&spec);
    let (provider, account, model) =
        if let Some((_, bare)) = config.providers.custom_provider_for_model(parsed.base) {
            let provider = parsed
                .base
                .split_once(':')
                .or_else(|| parsed.base.split_once('/'))
                .map(|(prefix, _)| prefix.trim().to_string())
                .unwrap_or_default();
            (provider, None, bare)
        } else {
            let selection = crate::providers::resolve_provider(&spec);
            (
                selection.kind.id().to_string(),
                selection.account,
                selection.model,
            )
        };
    ResolvedAutomationModel {
        provider,
        account,
        model,
        thinking: config.thinking_level.display_name().to_string(),
        fast: parsed.fast,
        origin,
        thinking_origin,
        spec,
    }
}

/// Discovers and parses automations from user-global directory.
///
/// Scans:
/// - `<ZDX_HOME>/automations/*.md`
///
/// # Errors
/// Returns an error if parsing fails, duplicate names are found, or I/O fails.
pub fn discover(root: &Path) -> Result<Vec<AutomationDefinition>> {
    let user_dir = paths::zdx_home().join("automations");
    discover_with_user_dir(root, &user_dir)
}

/// Discovers and parses automations with an explicit user automations directory.
///
/// Intended for testing without env-var mutation.
///
/// # Errors
/// Returns an error if parsing fails, duplicate names are found, or I/O fails.
pub fn discover_with_user_dir(root: &Path, user_dir: &Path) -> Result<Vec<AutomationDefinition>> {
    let _ = root;
    let mut entries: Vec<(PathBuf, AutomationSource)> = Vec::new();
    collect_markdown_files(user_dir, AutomationSource::User, &mut entries)?;

    let mut by_name: BTreeMap<String, AutomationDefinition> = BTreeMap::new();
    for (path, source) in entries {
        let definition = parse_automation_file(&path, source)
            .with_context(|| format!("parse automation {}", path.display()))?;

        if let Some(existing) = by_name.get(&definition.name) {
            bail!(
                "Duplicate automation name '{}': '{}' and '{}'",
                definition.name,
                existing.path.display(),
                definition.path.display()
            );
        }

        by_name.insert(definition.name.clone(), definition);
    }

    Ok(by_name.into_values().collect())
}

/// Loads one automation by file-stem name.
///
/// # Errors
/// Returns an error if discovery/parsing fails or the name is not found.
pub fn load_by_name(root: &Path, name: &str) -> Result<AutomationDefinition> {
    let user_dir = paths::zdx_home().join("automations");
    load_by_name_with_user_dir(root, &user_dir, name)
}

/// Loads one automation by name with explicit user automations directory.
///
/// # Errors
/// Returns an error if discovery/parsing fails or the name is not found.
pub fn load_by_name_with_user_dir(
    root: &Path,
    user_dir: &Path,
    name: &str,
) -> Result<AutomationDefinition> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("Automation name cannot be empty");
    }

    let all = discover_with_user_dir(root, user_dir)?;
    all.into_iter()
        .find(|a| a.name == trimmed)
        .ok_or_else(|| anyhow::anyhow!("Automation '{trimmed}' not found"))
}

/// Returns whether a 5-field cron schedule matches the provided local time.
///
/// Supported field forms per position:
/// - `*`
/// - `N`
/// - `*/N`
/// - `A-B`
/// - comma-separated combinations of the above
///
/// Cron fields order: minute hour day-of-month month day-of-week.
/// Day-of-week accepts 0-6 (Sun-Sat), and 7 as Sunday.
///
/// # Errors
/// Returns an error for invalid cron syntax/ranges.
///
/// # Panics
/// Panics only if chrono returns invalid field values that do not fit in `i32`.
pub fn schedule_matches_local_time(schedule: &str, now: DateTime<Local>) -> Result<bool> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        bail!(
            "Invalid schedule '{schedule}': expected 5 cron fields (minute hour day month weekday)"
        );
    }

    let minute = i32::try_from(now.minute()).expect("minute in i32 range");
    let hour = i32::try_from(now.hour()).expect("hour in i32 range");
    let day = i32::try_from(now.day()).expect("day in i32 range");
    let month = i32::try_from(now.month()).expect("month in i32 range");
    let weekday = i32::try_from(now.weekday().num_days_from_sunday()).expect("weekday in range");

    Ok(field_matches(fields[0], minute, 0, 59, false)?
        && field_matches(fields[1], hour, 0, 23, false)?
        && field_matches(fields[2], day, 1, 31, false)?
        && field_matches(fields[3], month, 1, 12, false)?
        && field_matches(fields[4], weekday, 0, 6, true)?)
}

fn collect_markdown_files(
    dir: &Path,
    source: AutomationSource,
    out: &mut Vec<(PathBuf, AutomationSource)>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }

    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("read automation dir {}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        })
        .collect();

    files.sort();
    for path in files {
        out.push((path, source));
    }
    Ok(())
}

fn parse_automation_file(path: &Path, source: AutomationSource) -> Result<AutomationDefinition> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read automation file {}", path.display()))?;
    let (yaml, body) = split_frontmatter(&content)?;

    let frontmatter: AutomationFrontmatter = if yaml.trim().is_empty() {
        AutomationFrontmatter::default()
    } else {
        serde_yaml::from_str(&yaml)
            .with_context(|| format!("parse YAML frontmatter in {}", path.display()))?
    };

    let name = file_stem(path)?;
    let schedule = normalize_optional_string(frontmatter.schedule, "schedule")?;
    let model = normalize_optional_string(frontmatter.model, "model")?;
    let subagent = normalize_optional_string(frontmatter.subagent, "subagent")?;
    // Root problems (including empty) are reported by `resolve_run`, so they
    // fail only this automation rather than discovery of every automation.
    let root = frontmatter.root.map(|raw| raw.trim().to_string());

    if matches!(frontmatter.timeout_secs, Some(0)) {
        bail!("timeout_secs must be greater than zero");
    }

    let prompt = body.trim().to_string();
    if prompt.is_empty() {
        bail!("Automation prompt body cannot be empty");
    }

    Ok(AutomationDefinition {
        name,
        path: path.to_path_buf(),
        source,
        schedule,
        model,
        subagent,
        root,
        timeout_secs: frontmatter.timeout_secs,
        max_retries: frontmatter.max_retries.unwrap_or(0),
        prompt,
    })
}

fn file_stem(path: &Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Invalid automation file name: {}", path.display()))?;
    Ok(stem.to_string())
}

fn normalize_optional_string(value: Option<String>, field: &str) -> Result<Option<String>> {
    match value {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("{field} cannot be empty");
            }
            Ok(Some(trimmed.to_string()))
        }
        None => Ok(None),
    }
}

fn split_frontmatter(content: &str) -> Result<(String, String)> {
    let content = content.trim_start_matches('\u{feff}');
    let lines: Vec<&str> = content.lines().collect();

    let Some(first) = lines.first() else {
        bail!("Missing YAML frontmatter");
    };

    if first.trim() != "---" {
        bail!("Missing YAML frontmatter");
    }

    for idx in 1..lines.len() {
        let trimmed = lines[idx].trim();
        if trimmed == "---" || trimmed == "..." {
            let yaml = lines[1..idx].join("\n");
            let body = lines[idx + 1..].join("\n");
            return Ok((yaml, body));
        }
    }

    bail!("Unterminated YAML frontmatter")
}

fn field_matches(expr: &str, value: i32, min: i32, max: i32, is_dow: bool) -> Result<bool> {
    for part in expr.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if part == "*" {
            return Ok(true);
        }

        if let Some(step_str) = part.strip_prefix("*/") {
            let step = parse_value(step_str, min, max, is_dow)?;
            if step <= 0 {
                bail!("Invalid step '{part}': must be greater than zero");
            }
            if (value - min) % step == 0 {
                return Ok(true);
            }
            continue;
        }

        if let Some((start_str, end_str)) = part.split_once('-') {
            let start = parse_value(start_str, min, max, is_dow)?;
            let end = parse_value(end_str, min, max, is_dow)?;
            if start > end {
                bail!("Invalid range '{part}': start must be <= end");
            }
            if (start..=end).contains(&value) {
                return Ok(true);
            }
            continue;
        }

        let n = parse_value(part, min, max, is_dow)?;
        if n == value {
            return Ok(true);
        }
    }

    Ok(false)
}

fn parse_value(raw: &str, min: i32, max: i32, is_dow: bool) -> Result<i32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Invalid empty cron value");
    }

    let mut n = trimmed
        .parse::<i32>()
        .with_context(|| format!("Invalid cron value '{trimmed}'"))?;

    // Allow 7 as Sunday for day-of-week.
    if is_dow && n == 7 {
        n = 0;
    }

    if !(min..=max).contains(&n) {
        bail!("Cron value '{trimmed}' out of range {min}..{max}");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use chrono::{LocalResult, TimeZone};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parse_minimal_automation_defaults() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("morning-report.md");
        fs::write(
            &file,
            "---\n---\nGenerate morning report from recent threads.",
        )
        .unwrap();

        let parsed = parse_automation_file(&file, AutomationSource::User).unwrap();
        assert_eq!(parsed.name, "morning-report");
        assert!(parsed.schedule.is_none());
        assert!(parsed.model.is_none());
        assert!(parsed.subagent.is_none());
        assert_eq!(parsed.max_retries, 0);
        assert_eq!(
            parsed.prompt,
            "Generate morning report from recent threads."
        );
    }

    #[test]
    fn parse_automation_with_subagent() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("morning-report.md");
        fs::write(
            &file,
            "---\nsubagent: explorer\n---\nGenerate morning report from recent threads.",
        )
        .unwrap();

        let parsed = parse_automation_file(&file, AutomationSource::User).unwrap();
        assert_eq!(parsed.subagent.as_deref(), Some("explorer"));
    }

    #[test]
    fn parse_automation_with_root() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("scoped.md");
        fs::write(&file, "---\nroot: \"~/work/space\"\n---\nprompt").unwrap();

        let parsed = parse_automation_file(&file, AutomationSource::User).unwrap();
        assert_eq!(parsed.root.as_deref(), Some("~/work/space"));
    }

    #[test]
    fn empty_root_fails_resolution_not_discovery() {
        let root = tempdir().unwrap();
        let user_dir = root.path().join("automations");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(
            user_dir.join("empty-root.md"),
            "---\nroot: \"  \"\n---\nprompt",
        )
        .unwrap();
        fs::write(user_dir.join("fine.md"), "---\n---\nprompt").unwrap();

        let all = discover_with_user_dir(root.path(), &user_dir).unwrap();
        assert_eq!(all.len(), 2);
        let broken = all.iter().find(|a| a.name == "empty-root").unwrap();
        let view = overview(&Config::default(), root.path(), broken);
        assert!(view.error.unwrap().contains("root cannot be empty"));
    }

    #[test]
    fn expand_root_accepts_home_and_absolute_and_rejects_relative() {
        let home = Path::new("/home/example");
        assert_eq!(
            expand_root("~/work", Some(home)).unwrap(),
            PathBuf::from("/home/example/work")
        );
        assert_eq!(
            expand_root("~", Some(home)).unwrap(),
            PathBuf::from("/home/example")
        );
        assert_eq!(
            expand_root("/srv/space", Some(home)).unwrap(),
            PathBuf::from("/srv/space")
        );
        assert!(
            expand_root("work/space", Some(home))
                .unwrap_err()
                .to_string()
                .contains("must be an absolute path")
        );
        assert!(
            expand_root("~other/space", Some(home))
                .unwrap_err()
                .to_string()
                .contains("must be an absolute path")
        );
        assert!(expand_root("~/work", None).is_err());
        assert!(
            expand_root("", Some(home))
                .unwrap_err()
                .to_string()
                .contains("root cannot be empty")
        );
    }

    #[test]
    fn resolve_root_falls_back_to_caller_only_when_undeclared() {
        let caller = tempdir().unwrap();
        let declared = tempdir().unwrap();

        let inherited = resolve_root(None, caller.path(), None).unwrap();
        assert_eq!(inherited.path, caller.path());
        assert_eq!(inherited.origin, SettingOrigin::Caller);

        let explicit =
            resolve_root(Some(declared.path().to_str().unwrap()), caller.path(), None).unwrap();
        assert_eq!(explicit.path, declared.path());
        assert_eq!(explicit.origin, SettingOrigin::Automation);

        let missing = declared.path().join("gone");
        assert!(resolve_root(Some(missing.to_str().unwrap()), caller.path(), None).is_err());
    }

    fn definition(model: Option<&str>) -> AutomationDefinition {
        AutomationDefinition {
            name: "demo".to_string(),
            path: PathBuf::from("/tmp/demo.md"),
            source: AutomationSource::User,
            schedule: Some("0 8 * * *".to_string()),
            model: model.map(str::to_string),
            subagent: None,
            root: None,
            timeout_secs: None,
            max_retries: 0,
            prompt: "prompt".to_string(),
        }
    }

    #[test]
    fn resolve_run_settings_reports_inherited_and_explicit_model() {
        let caller = tempdir().unwrap();
        let config = Config {
            model: "claude-cli@work:claude-sonnet-5@high".to_string(),
            thinking_level: crate::config::ThinkingLevel::High,
            ..Default::default()
        };

        let inherited = resolve_run_settings(&config, caller.path(), &definition(None)).unwrap();
        assert_eq!(inherited.model.spec, "claude-cli@work:claude-sonnet-5@high");
        assert_eq!(inherited.model.provider, "claude-cli");
        assert_eq!(inherited.model.account.as_deref(), Some("work"));
        assert_eq!(inherited.model.model, "claude-sonnet-5");
        assert_eq!(inherited.model.thinking, "high");
        assert_eq!(inherited.model.origin, SettingOrigin::Config);
        assert!(inherited.model.origin.is_inherited());

        let suffixless = resolve_run_settings(
            &config,
            caller.path(),
            &definition(Some("claude-cli:claude-opus-5")),
        )
        .unwrap();
        assert_eq!(suffixless.model.spec, "claude-cli:claude-opus-5@high");
        assert_eq!(suffixless.model.account, None);
        assert_eq!(suffixless.model.origin, SettingOrigin::Automation);
        assert_eq!(suffixless.model.thinking_origin, SettingOrigin::Config);

        let explicit = resolve_run_settings(
            &config,
            caller.path(),
            &definition(Some("claude-cli:claude-opus-5@low")),
        )
        .unwrap();
        assert_eq!(explicit.model.thinking, "low");
        assert_eq!(explicit.model.thinking_origin, SettingOrigin::Automation);
    }

    #[test]
    fn overview_keeps_resolution_errors_per_automation() {
        let caller = tempdir().unwrap();
        let mut broken = definition(None);
        broken.root = Some("relative/root".to_string());

        let view = overview(&Config::default(), caller.path(), &broken);
        assert!(view.settings.is_none());
        assert!(
            view.error
                .as_deref()
                .unwrap()
                .contains("must be an absolute path")
        );
        assert_eq!(view.declared_root.as_deref(), Some("relative/root"));

        let json = serde_json::to_value(overview(
            &Config::default(),
            caller.path(),
            &definition(None),
        ))
        .unwrap();
        assert_eq!(json["settings"]["root"]["origin"], "caller");
        assert_eq!(json["settings"]["model"]["origin"], "config");
    }

    #[test]
    fn parse_requires_frontmatter() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("invalid.md");
        fs::write(&file, "no frontmatter").unwrap();

        let err = parse_automation_file(&file, AutomationSource::User).unwrap_err();
        assert!(err.to_string().contains("Missing YAML frontmatter"));
    }

    #[test]
    fn discover_reads_user_automations() {
        let root = tempdir().unwrap();
        let user = tempdir().unwrap();

        let user_dir = user.path().join("automations");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("user-report.md"), "---\n---\nuser prompt").unwrap();

        let all = discover_with_user_dir(root.path(), &user_dir).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all.iter().any(|a| a.name == "user-report"));
    }

    #[test]
    fn discover_ignores_project_automations_directory() {
        let root = tempdir().unwrap();
        let user = tempdir().unwrap();

        let project_dir = root.path().join(".zdx").join("automations");
        let user_dir = user.path().join("automations");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&user_dir).unwrap();

        fs::write(
            project_dir.join("project-only.md"),
            "---\n---\nproject prompt",
        )
        .unwrap();
        fs::write(user_dir.join("user-only.md"), "---\n---\nuser prompt").unwrap();

        let all = discover_with_user_dir(root.path(), &user_dir).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "user-only");
    }

    #[test]
    fn cron_schedule_matches_expected_local_time() {
        let LocalResult::Single(dt) = Local.with_ymd_and_hms(2026, 2, 11, 8, 0, 0) else {
            panic!("expected unambiguous local datetime");
        };

        assert!(schedule_matches_local_time("0 8 * * *", dt).unwrap());
        assert!(!schedule_matches_local_time("1 8 * * *", dt).unwrap());
    }

    #[test]
    fn cron_schedule_supports_steps_and_ranges() {
        let LocalResult::Single(dt) = Local.with_ymd_and_hms(2026, 2, 11, 8, 30, 0) else {
            panic!("expected unambiguous local datetime");
        };

        assert!(schedule_matches_local_time("*/15 8-10 * * *", dt).unwrap());
        assert!(!schedule_matches_local_time("*/20 8-10 * * *", dt).unwrap());
    }

    #[test]
    fn cron_invalid_field_count_is_rejected() {
        let dt = Local::now();
        let err = schedule_matches_local_time("0 8 * *", dt).unwrap_err();
        assert!(err.to_string().contains("expected 5 cron fields"));
    }
}
