use std::path::{Path, PathBuf};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use zdx_engine::automations::{self, AutomationOverview, SettingOrigin};
use zdx_engine::config::{Config, paths};

use crate::app::MonitorApp;
use crate::ui::{SELECTED_BG, truncate_chars};

/// Automations resolved against the monitor root, or why they could not be
/// listed at all. Per-automation resolution errors stay on each overview.
pub(crate) struct AutomationsSnapshot {
    pub items: Vec<AutomationOverview>,
    pub error: Option<String>,
}

/// Discovers automations and resolves their settings through the shared
/// engine contract, as a caller rooted at `root` would. Reads config and
/// subagent files, so call it on tab entry, not on the refresh tick.
pub(crate) fn load_automations(root: &Path) -> AutomationsSnapshot {
    let failed = |error: String| AutomationsSnapshot {
        items: Vec::new(),
        error: Some(error),
    };
    let config = match Config::load_layered(&paths::config_layer_paths_for(root)) {
        Ok(config) => config,
        Err(err) => return failed(format!("load config: {err:#}")),
    };
    let definitions = match automations::discover(root) {
        Ok(definitions) => definitions,
        Err(err) => return failed(format!("discover automations: {err:#}")),
    };
    AutomationsSnapshot {
        items: definitions
            .iter()
            .map(|definition| automations::overview(&config, root, definition))
            .collect(),
        error: None,
    }
}

struct AutomationsView<'a> {
    items: &'a [AutomationOverview],
    error: Option<&'a str>,
    selected: usize,
    monitor_root: &'a Path,
    home: Option<&'a Path>,
}

pub(crate) fn render_automations(f: &mut Frame, app: &MonitorApp, area: Rect) {
    let home = paths::home_dir();
    let view = AutomationsView {
        items: &app.automations,
        error: app.automations_error.as_deref(),
        selected: app.selected_index,
        monitor_root: &app.root,
        home: home.as_deref(),
    };
    render_view(f, &view, area);
}

/// Rows below which the detail pane is dropped so the list stays usable.
const MIN_HEIGHT_FOR_DETAIL: u16 = 14;
const MAX_DETAIL_HEIGHT: u16 = 11;
const LABEL_WIDTH: usize = 10;

fn render_view(f: &mut Frame, view: &AutomationsView, area: Rect) {
    let detail_height = if area.height >= MIN_HEIGHT_FOR_DETAIL && !view.items.is_empty() {
        (area.height / 2).min(MAX_DETAIL_HEIGHT)
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
        .split(area);

    let title = if area.width >= 60 {
        format!(
            " Automations · * inherited · resolved for {} ",
            display_path(view.monitor_root, view.home)
        )
    } else {
        " Automations · * inherited ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    if let Some(error) = view.error {
        let text = Paragraph::new(format!("error: {error}"))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false })
            .block(block);
        f.render_widget(text, chunks[0]);
        return;
    }
    if view.items.is_empty() {
        let text = Paragraph::new("No automations found.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(text, chunks[0]);
        return;
    }

    let width = chunks[0].width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = view
        .items
        .iter()
        .enumerate()
        .map(|(i, overview)| {
            let item = ListItem::new(row_line(overview, width, view.home));
            if i == view.selected {
                item.style(Style::default().bg(SELECTED_BG))
            } else {
                item
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(view.selected));
    f.render_stateful_widget(List::new(items).block(block), chunks[0], &mut state);

    if detail_height > 0
        && let Some(overview) = view.items.get(view.selected)
    {
        let detail = Paragraph::new(detail_lines(overview, view.monitor_root, view.home))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Resolved now · not a past run "),
            );
        f.render_widget(detail, chunks[1]);
    }
}

/// One list row, laid out for `width` columns: name, then schedule, model,
/// and root as space allows. Inherited values end in `*`.
fn row_line(overview: &AutomationOverview, width: usize, home: Option<&Path>) -> Line<'static> {
    let name_width = if width >= 60 {
        22
    } else {
        (width * 2 / 5).max(1)
    };
    let schedule_width = if width >= 70 { 14 } else { 0 };
    let schedule_columns = if schedule_width > 0 {
        schedule_width + 2
    } else {
        0
    };
    let rest = width.saturating_sub(1 + name_width + 2 + schedule_columns);

    let mut spans = vec![Span::raw(format!(
        " {}  ",
        pad(&truncate_chars(&overview.name, name_width), name_width)
    ))];
    if schedule_width > 0 {
        let schedule = overview.schedule.as_deref().unwrap_or("manual");
        spans.push(Span::styled(
            format!(
                "{}  ",
                pad(&truncate_chars(schedule, schedule_width), schedule_width)
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let Some(settings) = &overview.settings else {
        let error = overview.error.as_deref().unwrap_or("unresolved");
        spans.push(Span::styled(
            truncate_chars(&format!("error: {error}"), rest),
            Style::default().fg(Color::Red),
        ));
        return Line::from(spans);
    };

    let show_root = rest >= 50;
    let model_width = if show_root { rest * 11 / 20 } else { rest };
    let model = marked(
        &settings.model.spec,
        settings.model.origin.is_inherited(),
        model_width,
        false,
    );
    spans.push(Span::styled(
        if show_root {
            format!("{}  ", pad(&model, model_width))
        } else {
            model
        },
        origin_style(settings.model.origin),
    ));
    if show_root {
        let root_width = rest.saturating_sub(model_width + 2);
        spans.push(Span::styled(
            marked(
                &display_path(&settings.root.path, home),
                settings.root.origin.is_inherited(),
                root_width,
                true,
            ),
            origin_style(settings.root.origin),
        ));
    }
    Line::from(spans)
}

/// Detail of the selected automation: declared and resolved settings with
/// provenance, or the resolution error.
fn detail_lines(
    overview: &AutomationOverview,
    monitor_root: &Path,
    home: Option<&Path>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        labeled("file", vec![Span::raw(display_path(&overview.path, home))]),
        labeled("schedule", vec![Span::raw(schedule_text(overview))]),
    ];

    let Some(settings) = &overview.settings else {
        lines.extend(unresolved_lines(overview));
        return lines;
    };

    let model = &settings.model;
    lines.push(labeled(
        "model",
        vec![
            Span::styled(model.spec.clone(), origin_style(model.origin)),
            Span::styled(
                format!("  {}", origin_label(model.origin)),
                Style::default().fg(Color::DarkGray),
            ),
        ],
    ));
    let mut parts = vec![
        format!("provider {}", model.provider),
        format!("account {}", model.account.as_deref().unwrap_or("default")),
    ];
    if model.thinking_origin == model.origin {
        parts.push(format!("thinking {}", model.thinking));
    } else {
        parts.push(format!(
            "thinking {} ({})",
            model.thinking,
            origin_label(model.thinking_origin)
        ));
    }
    if model.fast {
        parts.push("fast".to_string());
    }
    lines.push(labeled(
        "",
        vec![Span::styled(
            parts.join(" · "),
            Style::default().fg(Color::DarkGray),
        )],
    ));

    let root = &settings.root;
    lines.push(labeled(
        "root",
        vec![
            Span::styled(display_path(&root.path, home), origin_style(root.origin)),
            Span::styled(
                format!("  {}", origin_label(root.origin)),
                Style::default().fg(Color::DarkGray),
            ),
        ],
    ));
    if let Some(subagent) = &settings.subagent {
        lines.push(labeled("subagent", vec![Span::raw(subagent.clone())]));
    }
    if root.origin.is_inherited() {
        lines.push(labeled(
            "note",
            vec![Span::styled(
                format!(
                    "Inherited values are resolved for this Monitor (root {}). The daemon \
                     resolves them with its own --root, so they can differ.",
                    display_path(monitor_root, home)
                ),
                Style::default().fg(Color::Yellow),
            )],
        ));
    }
    lines
}

fn schedule_text(overview: &AutomationOverview) -> String {
    let mut parts = vec![
        overview
            .schedule
            .clone()
            .unwrap_or_else(|| "manual only".to_string()),
    ];
    if let Some(timeout) = overview.timeout_secs {
        parts.push(format!("timeout {timeout}s"));
    }
    parts.push(format!("retries {}", overview.max_retries));
    parts.join(" · ")
}

/// Declared values and the resolution error for an automation whose
/// settings could not be resolved.
fn unresolved_lines(overview: &AutomationOverview) -> Vec<Line<'static>> {
    let declared = |value: Option<&String>, fallback: &str| {
        value.map_or_else(|| fallback.to_string(), |value| format!("declared {value}"))
    };
    let mut lines = vec![
        labeled(
            "model",
            vec![Span::raw(declared(
                overview.declared_model.as_ref(),
                "not set (inherits)",
            ))],
        ),
        labeled(
            "root",
            vec![Span::raw(declared(
                overview.declared_root.as_ref(),
                "not set (inherits caller root)",
            ))],
        ),
    ];
    if let Some(subagent) = &overview.subagent {
        lines.push(labeled("subagent", vec![Span::raw(subagent.clone())]));
    }
    lines.push(labeled(
        "error",
        vec![Span::styled(
            overview
                .error
                .clone()
                .unwrap_or_else(|| "unresolved".to_string()),
            Style::default().fg(Color::Red),
        )],
    ));
    lines
}

fn labeled(label: &str, mut value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(" {label:<LABEL_WIDTH$}"),
        Style::default().fg(Color::DarkGray),
    )];
    spans.append(&mut value);
    Line::from(spans)
}

fn origin_label(origin: SettingOrigin) -> String {
    let source = origin.as_str().replace('_', " ");
    if origin.is_inherited() {
        format!("inherited from {source}")
    } else {
        format!("set by {source}")
    }
}

fn origin_style(origin: SettingOrigin) -> Style {
    if origin.is_inherited() {
        Style::default()
    } else {
        Style::default().fg(Color::Cyan)
    }
}

/// Fits `text` in `width` columns, ending in `*` when inherited. Paths keep
/// their tail (`from_start`), since the last components identify them.
fn marked(text: &str, inherited: bool, width: usize, from_start: bool) -> String {
    let budget = if inherited {
        width.saturating_sub(1)
    } else {
        width
    };
    let fitted = if from_start {
        truncate_start(text, budget)
    } else {
        truncate_chars(text, budget)
    };
    if inherited && width > 0 {
        format!("{fitted}*")
    } else {
        fitted
    }
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

fn truncate_start(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let tail: String = value.chars().skip(count - (max_chars - 1)).collect();
    format!("…{tail}")
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => PathBuf::from("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use zdx_engine::automations::{
        AutomationRunSettings, ResolvedAutomationModel, ResolvedAutomationRoot,
    };

    use super::*;

    const HOME: &str = "/home/user";

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn overview(
        name: &str,
        model_origin: SettingOrigin,
        thinking_origin: SettingOrigin,
        root_origin: SettingOrigin,
    ) -> AutomationOverview {
        AutomationOverview {
            name: name.to_string(),
            path: PathBuf::from(format!("{HOME}/.zdx/automations/{name}.md")),
            schedule: Some("0 9 * * *".to_string()),
            declared_root: None,
            declared_model: None,
            subagent: None,
            timeout_secs: Some(600),
            max_retries: 1,
            settings: Some(AutomationRunSettings {
                root: ResolvedAutomationRoot {
                    path: PathBuf::from(format!("{HOME}/projects/example")),
                    origin: root_origin,
                },
                model: ResolvedAutomationModel {
                    spec: "provider-a@work:model-x@high".to_string(),
                    provider: "provider-a".to_string(),
                    account: Some("work".to_string()),
                    model: "model-x".to_string(),
                    thinking: "high".to_string(),
                    fast: false,
                    origin: model_origin,
                    thinking_origin,
                },
                subagent: None,
            }),
            error: None,
        }
    }

    fn failed(name: &str) -> AutomationOverview {
        AutomationOverview {
            declared_root: Some("~/missing".to_string()),
            settings: None,
            error: Some("root '/home/user/missing' does not exist or is not a directory".into()),
            ..overview(
                name,
                SettingOrigin::Config,
                SettingOrigin::Config,
                SettingOrigin::Caller,
            )
        }
    }

    #[test]
    fn wide_row_marks_inherited_values_only() {
        let home = Path::new(HOME);
        let explicit = overview(
            "daily-report",
            SettingOrigin::Automation,
            SettingOrigin::Automation,
            SettingOrigin::Automation,
        );
        let text = line_text(&row_line(&explicit, 120, Some(home)));
        assert!(text.contains("daily-report"));
        assert!(text.contains("0 9 * * *"));
        assert!(text.contains("provider-a@work:model-x@high "));
        assert!(text.ends_with("~/projects/example"));

        let inherited = overview(
            "daily-report",
            SettingOrigin::Config,
            SettingOrigin::Config,
            SettingOrigin::Caller,
        );
        let text = line_text(&row_line(&inherited, 120, Some(home)));
        assert!(text.contains("provider-a@work:model-x@high*"));
        assert!(text.ends_with("~/projects/example*"));
    }

    #[test]
    fn error_row_shows_error_instead_of_settings() {
        let text = line_text(&row_line(&failed("broken"), 120, Some(Path::new(HOME))));
        assert!(text.contains("broken"));
        assert!(text.contains("error: root '/home/user/missing' does not exist"));
        assert!(!text.contains("provider-a"));
    }

    #[test]
    fn narrow_row_fits_width_and_keeps_model() {
        let item = overview(
            "a-very-long-automation-name",
            SettingOrigin::Config,
            SettingOrigin::Config,
            SettingOrigin::Caller,
        );
        for width in [20, 40, 69] {
            let text = line_text(&row_line(&item, width, Some(Path::new(HOME))));
            assert!(text.chars().count() <= width, "{width}: {text:?}");
            assert!(text.ends_with('*'), "{width}: {text:?}");
            assert!(!text.contains("~/projects"), "root hidden at {width}");
        }
    }

    #[test]
    fn detail_shows_provenance_and_caller_note() {
        let home = Path::new(HOME);
        let item = overview(
            "daily-report",
            SettingOrigin::Automation,
            SettingOrigin::Config,
            SettingOrigin::Caller,
        );
        let text: Vec<String> = detail_lines(&item, Path::new("/srv/monitor"), Some(home))
            .iter()
            .map(line_text)
            .collect();
        let text = text.join("\n");
        assert!(text.contains("provider-a@work:model-x@high  set by automation"));
        assert!(text.contains("account work"));
        assert!(text.contains("thinking high (inherited from config)"));
        assert!(text.contains("~/projects/example  inherited from caller"));
        assert!(text.contains("root /srv/monitor"));
        assert!(text.contains("own --root"));

        let explicit_root = overview(
            "daily-report",
            SettingOrigin::SubagentOverride,
            SettingOrigin::SubagentOverride,
            SettingOrigin::Automation,
        );
        let text: Vec<String> = detail_lines(&explicit_root, Path::new("/srv/monitor"), Some(home))
            .iter()
            .map(line_text)
            .collect();
        let text = text.join("\n");
        assert!(text.contains("set by subagent override"));
        assert!(text.contains("thinking high"));
        assert!(!text.contains("thinking high ("));
        assert!(!text.contains("own --root"));
    }

    #[test]
    fn detail_shows_declared_values_and_error() {
        let text: Vec<String> =
            detail_lines(&failed("broken"), Path::new("/srv"), Some(Path::new(HOME)))
                .iter()
                .map(line_text)
                .collect();
        let text = text.join("\n");
        assert!(text.contains("not set (inherits)"));
        assert!(text.contains("declared ~/missing"));
        assert!(text.contains("error"));
        assert!(text.contains("does not exist"));
    }

    fn render_text(view: &AutomationsView, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_view(frame, view, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn renders_list_and_detail_at_small_width() {
        let items = vec![
            overview(
                "daily-report",
                SettingOrigin::Automation,
                SettingOrigin::Automation,
                SettingOrigin::Automation,
            ),
            failed("broken"),
        ];
        let view = AutomationsView {
            items: &items,
            error: None,
            selected: 1,
            monitor_root: Path::new("/srv/monitor"),
            home: Some(Path::new(HOME)),
        };
        let text = render_text(&view, 40, 20);
        assert!(text.contains("daily-report"));
        assert!(text.contains("not a past run"));
        assert!(text.contains("declared ~/missing"));
        assert!(text.contains("error"));

        let short = render_text(&view, 40, 8);
        assert!(short.contains("broken"));
        assert!(!short.contains("not a past run"));
    }

    #[test]
    fn renders_listing_error() {
        let view = AutomationsView {
            items: &[],
            error: Some("discover automations: duplicate name"),
            selected: 0,
            monitor_root: Path::new("/srv/monitor"),
            home: None,
        };
        let text = render_text(&view, 80, 10);
        assert!(text.contains("error: discover automations: duplicate name"));
    }
}
