use std::path::{Path, PathBuf};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use zdx_engine::automation_daemon::{
    self, DaemonAutomations, DaemonContext, DaemonContextSource, DaemonContextState,
};
use zdx_engine::automations::{AutomationOverview, SettingOrigin};
use zdx_engine::config::paths;

use crate::app::MonitorApp;
use crate::ui::{SELECTED_BG, truncate_chars};

/// Automations resolved the way the scheduled daemon resolves them, through
/// the shared engine view. Never uses the Monitor's own root. Reads the
/// daemon record, service plist, config, and subagent files, so call it on
/// tab entry, not on the refresh tick.
pub(crate) fn load_automations() -> DaemonAutomations {
    automation_daemon::automations()
}

struct AutomationsView<'a> {
    items: &'a [AutomationOverview],
    error: Option<&'a str>,
    selected: usize,
    daemon: Option<&'a DaemonContextState>,
    home: Option<&'a Path>,
}

pub(crate) fn render_automations(f: &mut Frame, app: &MonitorApp, area: Rect) {
    let home = paths::home_dir();
    let view = AutomationsView {
        items: &app.automations,
        error: app.automations_error.as_deref(),
        selected: app.selected_index,
        daemon: app.automations_daemon.as_ref(),
        home: home.as_deref(),
    };
    render_view(f, &view, area);
}

/// Short daemon-context label for the list title.
fn daemon_title(daemon: Option<&DaemonContextState>, home: Option<&Path>) -> String {
    match daemon {
        Some(DaemonContextState::Available { context }) => format!(
            "{} · root {}",
            source_label(context),
            display_path(&context.root, home)
        ),
        Some(DaemonContextState::Unavailable { .. }) => "daemon settings unavailable".to_string(),
        None => "daemon not checked".to_string(),
    }
}

fn source_label(context: &DaemonContext) -> &'static str {
    match context.source {
        DaemonContextSource::Running => "running daemon",
        DaemonContextSource::Installed => "installed daemon (next start)",
    }
}

/// Explains where inherited values come from, for the detail pane.
fn daemon_note(daemon: Option<&DaemonContextState>, home: Option<&Path>) -> String {
    match daemon {
        Some(DaemonContextState::Available { context }) => {
            let mut text = format!(
                "Inherited values come from the {} (root {}, config from {}).",
                source_label(context),
                display_path(&context.root, home),
                display_path(&context.config_dir, home)
            );
            if let Some(note) = &context.note {
                text.push(' ');
                text.push_str(note);
                text.push('.');
            }
            text
        }
        Some(DaemonContextState::Unavailable { reason }) => {
            format!("Daemon settings unavailable: {reason}.")
        }
        None => "Daemon settings not checked yet.".to_string(),
    }
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
            " Automations · * inherited · {} ",
            daemon_title(view.daemon, view.home)
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
        let detail = Paragraph::new(detail_lines(overview, view.daemon, view.home))
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
    daemon: Option<&DaemonContextState>,
    home: Option<&Path>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        labeled("file", vec![Span::raw(display_path(&overview.path, home))]),
        labeled("schedule", vec![Span::raw(schedule_text(overview))]),
    ];

    let Some(settings) = &overview.settings else {
        lines.extend(unresolved_lines(overview));
        if overview.declared_root.is_none() {
            lines.push(note_line(daemon_note(daemon, home)));
        }
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
        lines.push(note_line(daemon_note(daemon, home)));
    }
    lines
}

fn note_line(text: String) -> Line<'static> {
    labeled(
        "note",
        vec![Span::styled(text, Style::default().fg(Color::Yellow))],
    )
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
                "not set (inherits daemon root)",
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

    fn daemon(source: DaemonContextSource, note: Option<&str>) -> DaemonContextState {
        DaemonContextState::Available {
            context: DaemonContext {
                source,
                root: PathBuf::from(format!("{HOME}/workspace")),
                config_dir: PathBuf::from(format!("{HOME}/workspace")),
                pid: Some(42),
                started_at: None,
                note: note.map(str::to_string),
            },
        }
    }

    fn unavailable() -> DaemonContextState {
        DaemonContextState::Unavailable {
            reason: "daemon is not running and no daemon service is installed".to_string(),
        }
    }

    fn detail_text(item: &AutomationOverview, state: &DaemonContextState) -> String {
        detail_lines(item, Some(state), Some(Path::new(HOME)))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

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
                SettingOrigin::Daemon,
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
            SettingOrigin::Daemon,
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
            SettingOrigin::Daemon,
        );
        for width in [20, 40, 69] {
            let text = line_text(&row_line(&item, width, Some(Path::new(HOME))));
            assert!(text.chars().count() <= width, "{width}: {text:?}");
            assert!(text.ends_with('*'), "{width}: {text:?}");
            assert!(!text.contains("~/projects"), "root hidden at {width}");
        }
    }

    #[test]
    fn detail_shows_provenance_and_daemon_source() {
        let item = overview(
            "daily-report",
            SettingOrigin::Automation,
            SettingOrigin::Config,
            SettingOrigin::Daemon,
        );
        let text = detail_text(&item, &daemon(DaemonContextSource::Running, None));
        assert!(text.contains("provider-a@work:model-x@high  set by automation"));
        assert!(text.contains("account work"));
        assert!(text.contains("thinking high (inherited from config)"));
        assert!(text.contains("~/projects/example  inherited from daemon"));
        assert!(text.contains("from the running daemon (root ~/workspace"));

        let text = detail_text(
            &item,
            &daemon(
                DaemonContextSource::Installed,
                Some("daemon is not running; showing the installed service"),
            ),
        );
        assert!(text.contains("installed daemon (next start)"));
        assert!(text.contains("daemon is not running"));

        let explicit_root = overview(
            "daily-report",
            SettingOrigin::SubagentOverride,
            SettingOrigin::SubagentOverride,
            SettingOrigin::Automation,
        );
        let text = detail_text(&explicit_root, &daemon(DaemonContextSource::Running, None));
        assert!(text.contains("set by subagent override"));
        assert!(text.contains("thinking high"));
        assert!(!text.contains("thinking high ("));
        assert!(!text.contains("note"));
    }

    #[test]
    fn detail_shows_declared_values_and_error() {
        let text = detail_text(
            &failed("broken"),
            &daemon(DaemonContextSource::Running, None),
        );
        assert!(text.contains("not set (inherits)"));
        assert!(text.contains("declared ~/missing"));
        assert!(text.contains("error"));
        assert!(text.contains("does not exist"));
        assert!(!text.contains("note"), "declared root needs no daemon note");

        let inherited = AutomationOverview {
            declared_root: None,
            error: Some("daemon settings unavailable: daemon is not running".into()),
            ..failed("needs-daemon")
        };
        let text = detail_text(&inherited, &unavailable());
        assert!(text.contains("not set (inherits daemon root)"));
        assert!(text.contains("Daemon settings unavailable: daemon is not running"));
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
        let running = daemon(DaemonContextSource::Running, None);
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
            daemon: Some(&running),
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
            daemon: None,
            home: None,
        };
        let text = render_text(&view, 80, 10);
        assert!(text.contains("error: discover automations: duplicate name"));
    }

    #[test]
    fn title_names_the_daemon_context_not_the_monitor() {
        let home = Some(Path::new(HOME));
        let running = daemon(DaemonContextSource::Running, None);
        let installed = daemon(DaemonContextSource::Installed, Some("not running"));
        assert_eq!(
            daemon_title(Some(&running), home),
            "running daemon · root ~/workspace"
        );
        assert_eq!(
            daemon_title(Some(&installed), home),
            "installed daemon (next start) · root ~/workspace"
        );
        assert_eq!(
            daemon_title(Some(&unavailable()), home),
            "daemon settings unavailable"
        );

        let items = vec![overview(
            "daily-report",
            SettingOrigin::Config,
            SettingOrigin::Config,
            SettingOrigin::Daemon,
        )];
        let view = AutomationsView {
            items: &items,
            error: None,
            selected: 0,
            daemon: Some(&running),
            home,
        };
        let text = render_text(&view, 100, 20);
        assert!(text.contains("running daemon · root ~/workspace"));
        assert!(text.contains("inherited from daemon"));
    }
}
