//! Orchestrator worker bridge.
//!
//! Consumes [`WorkerEvent`]s from the engine's `WorkerManager`. Workers are
//! **silent**: a new worker gets no Telegram topic and posts nothing. The user
//! reaches it through Mini App deep links (`?startapp=<worker_id>`) on the
//! orchestrator's reply, pinned card, and completion notice.
//!
//! - `Created` refreshes the owner's pinned card.
//! - `Completed` posts a one-line notice in the orchestrator topic and wakes
//!   it with a synthetic queued turn. Updates that arrive while the orchestrator is busy
//!   are merged by the topic queue into one turn (see
//!   `bot::queue::drain_worker_updates`), so a burst of completions is
//!   reviewed once with all of them in context.
//!
//! **Legacy mirror topics.** Workers created before silent workers may still
//! have a mirror topic (aliased to the worker and marked `worker_topic`). The
//! worker→topic map is rebuilt at startup from that persisted metadata, and
//! for those workers only the bridge keeps the old feed: orchestrator prompts
//! (`Prompted`), one debounced live status line per running turn (`Activity`,
//! carrying `⏹ Cancel worker` = `wk:c` and `💬 Open Thread`), and the final
//! result. Messages typed in such a topic are still queued into the worker
//! FIFO by the message handler. No new mirror topics are ever opened.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use zdx_engine::core::thread_persistence;
use zdx_engine::core::workers::{CompletionEvent, WorkerActivity, WorkerEvent, WorkerStatus};

use crate::agent::{STATUS_WAITING, tool_running_status};
use crate::bot::context::BotContext;
use crate::bot::queue::ChatQueueMap;
use crate::bot::synthetic::{SyntheticKind, dispatch_synthetic_prompt};
use crate::handlers::message::{
    append_thread_link, escape_html, mini_app_base_url, parse_topic_thread_id,
    refresh_thread_header, resolve_effective_thread_id, thread_id_for_chat, worker_link,
};
use crate::telegram::markdown::{to_telegram_html, truncate_telegram_html};
use crate::telegram::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient, topic_link,
};

/// Longest worker final-text excerpt embedded in the callback prompt; the
/// orchestrator is told to use `Read_Thread` for anything longer.
const MAX_CALLBACK_TEXT_CHARS: usize = 2000;
/// Longest final-text excerpt posted into a mirror topic message.
const MAX_MIRROR_TEXT_CHARS: usize = 3500;
/// Minimum spacing between edits of one live status message.
const LIVE_EDIT_INTERVAL: Duration = Duration::from_secs(3);

const CANCEL_CALLBACK: &str = "wk:c";

/// Mirror topic destination for one worker.
#[derive(Clone, Copy)]
struct MirrorTopic {
    chat: i64,
    topic: i64,
}

/// One running worker turn's live status message: a single line naming the
/// tool currently running (same shape as a normal turn's status), deleted
/// when the result posts.
struct LiveTurn {
    mirror: MirrorTopic,
    message_id: i64,
    status: String,
    dirty: bool,
    last_edit: Instant,
}

impl LiveTurn {
    /// Returns whether the visible status changed.
    fn apply(&mut self, activity: &WorkerActivity) -> bool {
        let next = match activity {
            WorkerActivity::ToolStarted { name, .. } => tool_running_status(name),
            WorkerActivity::ToolFinished { .. } => STATUS_WAITING.to_string(),
            WorkerActivity::ToolInput { .. } => return false,
        };
        if next == self.status {
            return false;
        }
        self.status = next;
        self.dirty = true;
        true
    }

    fn next_flush_at(&self) -> Option<Instant> {
        self.dirty.then(|| self.last_edit + LIVE_EDIT_INTERVAL)
    }
}

struct Bridge {
    context: Arc<BotContext>,
    queues: ChatQueueMap,
    mirrors: HashMap<String, MirrorTopic>,
    live: HashMap<String, LiveTurn>,
}

/// Spawns the process-lifetime bridge task. It also performs queued restarts
/// (see `restart_queue`), since it alone knows no completion is in flight.
pub(crate) fn spawn_completion_bridge(
    context: Arc<BotContext>,
    queues: ChatQueueMap,
    mut events_rx: UnboundedReceiver<WorkerEvent>,
) {
    tokio::spawn(async move {
        let mut bridge = Bridge {
            context,
            queues,
            mirrors: HashMap::new(),
            live: HashMap::new(),
        };
        bridge.recover_mirrors().await;
        let mut restart_poll = tokio::time::interval(crate::restart_queue::POLL);

        loop {
            let next_flush = bridge.next_flush_at();
            tokio::select! {
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    bridge.handle(event).await;
                }
                _ = restart_poll.tick() => {
                    // Checked on this task, between events, so a worker
                    // completion is never half-delivered when a queued restart
                    // fires. Order matters: idle workers have already sent their
                    // completion, so an empty channel after that means none is
                    // pending; a non-empty chat queue means an orchestrator turn
                    // (including a `[worker update]`) is still queued or running.
                    let settled = bridge.context.worker_manager().all_idle()
                        && events_rx.is_empty()
                        && bridge.queues.lock().await.is_empty();
                    if settled {
                        crate::restart_queue::tick(&bridge.context).await;
                    }
                }
                () = async {
                    match next_flush {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    bridge.flush_due().await;
                }
            }
        }
    });
}

impl Bridge {
    /// Rebuilds the worker→mirror map from persisted mirror-topic metadata so
    /// feeds resume after a restart, and registers each link on the manager.
    async fn recover_mirrors(&mut self) {
        let pairs = tokio::task::spawn_blocking(thread_persistence::list_worker_topics).await;
        let pairs = match pairs {
            Ok(Ok(pairs)) => pairs,
            Ok(Err(err)) => {
                tracing::warn!(err = %format!("{err:#}"), "Failed to scan persisted worker mirror topics");
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "Mirror topic scan task failed");
                return;
            }
        };
        for (topic_thread_id, worker_thread_id) in pairs {
            let Some((chat, topic)) = parse_topic_thread_id(&topic_thread_id) else {
                continue;
            };
            let mirror = MirrorTopic { chat, topic };
            if let Some(link) = topic_link(chat, topic) {
                self.context
                    .worker_manager()
                    .set_mirror_url(&worker_thread_id, link);
            }
            self.mirrors.insert(worker_thread_id, mirror);
        }
        if !self.mirrors.is_empty() {
            tracing::info!(count = self.mirrors.len(), "Recovered worker mirror topics");
        }
    }

    async fn handle(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Created {
                owner_thread_id, ..
            } => {
                // Workers are silent: no Telegram topic is opened for them.
                // Only the owner's pinned card changes.
                refresh_thread_header(&self.context, &owner_thread_id).await;
            }
            WorkerEvent::Prompted {
                worker_thread_id,
                prompt,
            } => {
                post_mirror_prompt(
                    &self.context,
                    self.mirrors.get(&worker_thread_id),
                    &worker_thread_id,
                    &prompt,
                )
                .await;
                self.refresh_owner_header_of(&worker_thread_id).await;
            }
            WorkerEvent::Activity {
                worker_thread_id,
                activity,
            } => {
                self.handle_activity(&worker_thread_id, activity).await;
            }
            WorkerEvent::Completed(event) => {
                self.finish_live_turn(&event).await;
                post_mirror_update(
                    &self.context,
                    self.mirrors.get(&event.worker_thread_id),
                    &event,
                )
                .await;
                dispatch_owner_callback(&self.context, &self.queues, &event).await;
                refresh_thread_header(&self.context, &event.owner_thread_id).await;
            }
        }
    }

    /// Refreshes the pinned card of whichever orchestrator owns `worker` (the
    /// card carries the live worker summary).
    async fn refresh_owner_header_of(&self, worker_thread_id: &str) {
        if let Some(snapshot) = self.context.worker_manager().snapshot(worker_thread_id) {
            refresh_thread_header(&self.context, &snapshot.owner_thread_id).await;
        }
    }

    async fn handle_activity(&mut self, worker_thread_id: &str, activity: WorkerActivity) {
        if let Some(turn) = self.live.get_mut(worker_thread_id) {
            if turn.apply(&activity) && turn.last_edit.elapsed() >= LIVE_EDIT_INTERVAL {
                self.flush(worker_thread_id).await;
            }
            return;
        }

        let Some(mirror) = self.mirrors.get(worker_thread_id).copied() else {
            return;
        };
        let mut turn = LiveTurn {
            mirror,
            message_id: 0,
            status: STATUS_WAITING.to_string(),
            dirty: false,
            last_edit: Instant::now(),
        };
        turn.apply(&activity);
        turn.dirty = false;
        let keyboard = mirror_keyboard(&self.context, mirror.chat, worker_thread_id);
        let sent = self
            .context
            .client()
            .send_message_with_markup(
                mirror.chat,
                &turn.status,
                None,
                Some(mirror.topic),
                &keyboard,
            )
            .await;
        match sent {
            Ok(message) => {
                turn.message_id = message.id;
                self.live.insert(worker_thread_id.to_string(), turn);
            }
            Err(err) => {
                tracing::warn!(worker = %worker_thread_id, err = %format!("{err:#}"), "Failed to post live status message");
            }
        }
        // First activity of a turn: the worker just went queued → running.
        self.refresh_owner_header_of(worker_thread_id).await;
    }

    fn next_flush_at(&self) -> Option<Instant> {
        self.live.values().filter_map(LiveTurn::next_flush_at).min()
    }

    async fn flush_due(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .live
            .iter()
            .filter(|(_, turn)| turn.next_flush_at().is_some_and(|at| at <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for worker_thread_id in due {
            self.flush(&worker_thread_id).await;
        }
    }

    async fn flush(&mut self, worker_thread_id: &str) {
        let Some(turn) = self.live.get_mut(worker_thread_id) else {
            return;
        };
        let keyboard = mirror_keyboard(&self.context, turn.mirror.chat, worker_thread_id);
        turn.dirty = false;
        turn.last_edit = Instant::now();
        if let Err(err) = self
            .context
            .client()
            .edit_message_text(
                turn.mirror.chat,
                turn.message_id,
                &turn.status,
                Some(&keyboard),
            )
            .await
            && !err.to_string().contains("message is not modified")
        {
            tracing::warn!(message_id = turn.message_id, err = %format!("{err:#}"), "Failed to edit live status message");
        }
    }

    /// Removes the live status message; the result posted right after it is
    /// the record of the turn, exactly like a normal turn's status.
    async fn finish_live_turn(&mut self, event: &CompletionEvent) {
        let Some(turn) = self.live.remove(&event.worker_thread_id) else {
            return;
        };
        if let Err(err) = self
            .context
            .client()
            .delete_message(turn.mirror.chat, turn.message_id)
            .await
        {
            tracing::warn!(message_id = turn.message_id, err = %format!("{err:#}"), "Failed to delete live status message");
        }
    }
}

/// Buttons under the live status message: cancel the worker from its
/// topic, and open the worker thread in the Mini App when configured.
fn mirror_keyboard(
    context: &BotContext,
    chat_id: i64,
    worker_thread_id: &str,
) -> InlineKeyboardMarkup {
    mirror_keyboard_for_url(
        mini_app_base_url(context, chat_id).as_deref(),
        worker_thread_id,
    )
}

fn mirror_keyboard_for_url(
    mini_app_url: Option<&str>,
    worker_thread_id: &str,
) -> InlineKeyboardMarkup {
    let mut row = vec![InlineKeyboardButton::callback(
        "⏹ Cancel worker",
        CANCEL_CALLBACK,
    )];
    if let Some(mini_app_url) = mini_app_url {
        row.push(InlineKeyboardButton::url(
            "💬 Open Thread",
            format!("{mini_app_url}?startapp={worker_thread_id}"),
        ));
    }
    InlineKeyboardMarkup {
        inline_keyboard: vec![row],
    }
}

/// Handles `wk:*` callbacks from mirror-topic keyboards. The worker is
/// resolved from the topic's persisted alias, never from callback data.
pub(crate) async fn handle_callback(
    context: &BotContext,
    client: &TelegramClient,
    callback: &CallbackQuery,
    rest: &str,
) {
    let answer = |text: &'static str| async move {
        if let Err(err) = client.answer_callback_query(&callback.id, Some(text)).await {
            tracing::warn!(err = %format!("{err:#}"), "Failed to answer worker callback");
        }
    };
    if rest != "c" {
        answer("Unknown action").await;
        return;
    }
    let Some((chat_id, topic_id)) = callback
        .message
        .as_ref()
        .and_then(|message| Some((message.chat.id, message.effective_thread_id()?)))
    else {
        answer("Not inside a worker topic").await;
        return;
    };
    let topic_thread_id = thread_id_for_chat(chat_id, Some(topic_id));
    if !thread_persistence::read_thread_worker_topic(&topic_thread_id).unwrap_or(false) {
        answer("Not a worker topic").await;
        return;
    }
    let worker_thread_id = resolve_effective_thread_id(&topic_thread_id);
    match context.worker_manager().cancel(&worker_thread_id) {
        Ok(_) => {
            tracing::info!(worker = %worker_thread_id, chat_id, topic_id, "Worker cancelled from mirror topic");
            answer("Cancelling worker…").await;
        }
        Err(_) => answer("Worker is idle; nothing to cancel").await,
    }
}

/// Posts an orchestrator-sent follow-up prompt into the worker's legacy mirror
/// topic (Markdown → Telegram HTML, entity-safe truncation). New workers have
/// no mirror, so this is a no-op for them.
async fn post_mirror_prompt(
    context: &Arc<BotContext>,
    mirror: Option<&MirrorTopic>,
    worker_thread_id: &str,
    prompt: &str,
) {
    let Some(mirror) = mirror else {
        return;
    };
    let body = truncate_telegram_html(&to_telegram_html(prompt), MAX_MIRROR_TEXT_CHARS);
    let text = format!("<b>📤 Prompt from the orchestrator</b>\n\n{body}");
    if let Err(err) = context
        .client()
        .send_message(mirror.chat, &text, None, Some(mirror.topic))
        .await
    {
        tracing::warn!(worker = %worker_thread_id, err = %format!("{err:#}"), "Failed to post mirror prompt");
    }
}

/// Posts one finished worker turn into its mirror topic, when known.
async fn post_mirror_update(
    context: &Arc<BotContext>,
    mirror: Option<&MirrorTopic>,
    event: &CompletionEvent,
) {
    let Some(mirror) = mirror else {
        return;
    };

    let text = match event.status {
        WorkerStatus::Completed => {
            let body = event.final_text.as_deref().unwrap_or_default();
            truncate_telegram_html(&to_telegram_html(body), MAX_MIRROR_TEXT_CHARS)
        }
        WorkerStatus::Cancelled => "🚫 Turn cancelled.".to_string(),
        _ => {
            let err_raw = event.error.as_deref().unwrap_or("unknown error");
            let summary = summarize_worker_error(err_raw);
            format!("❌ Turn {}: {summary}", event.status.as_str())
        }
    };
    // Same trailing Mini App link as a normal answer: results here are
    // bounded, and the full transcript is one tap away.
    let text = append_thread_link(
        mini_app_base_url(context, mirror.chat).as_deref(),
        &event.worker_thread_id,
        &text,
    );

    if let Err(err) = context
        .client()
        .send_message_without_preview(mirror.chat, &text, None, Some(mirror.topic))
        .await
    {
        tracing::warn!(worker = %event.worker_thread_id, err = %format!("{err:#}"), "Failed to post mirror update");
    }
}

/// Wakes the owning orchestrator topic with a synthetic `[worker update]` turn.
async fn dispatch_owner_callback(
    context: &Arc<BotContext>,
    queues: &ChatQueueMap,
    event: &CompletionEvent,
) {
    // Self-owned workers (re-attached from a mirror topic) have no
    // orchestrator to wake; the mirror post is the whole delivery.
    if event.owner_thread_id == event.worker_thread_id {
        return;
    }
    let Some(route) = context.orchestrator_route(&event.owner_thread_id) else {
        tracing::warn!(
            worker = %event.worker_thread_id,
            owner = %event.owner_thread_id,
            "No orchestrator route for worker completion; dropping callback"
        );
        return;
    };

    // The synthetic prompt itself is invisible in chat, so post a one-line
    // notice first: the user sees which worker woke the orchestrator and can
    // open it.
    let link = worker_link(
        event.mirror_url.clone(),
        mini_app_base_url(context, route.chat).as_deref(),
        &event.worker_thread_id,
    );
    let notice = worker_update_notice(event, link.as_deref());
    if let Err(err) = context
        .client()
        .send_message_without_preview(route.chat, &notice, None, route.topic)
        .await
    {
        tracing::warn!(worker = %event.worker_thread_id, err = %format!("{err:#}"), "Failed to post worker update notice");
    }

    let config = context.config_for_chat(route.chat);
    let worker_context = zdx_engine::tools::orchestrator::thread_context_json(
        &event.worker_thread_id,
        Some(&config),
    );
    let prompt = build_worker_update_prompt(event, &worker_context);
    let dispatched = dispatch_synthetic_prompt(
        context,
        queues,
        route.chat,
        route.topic,
        route.user,
        prompt,
        SyntheticKind::WorkerUpdate,
    )
    .await;
    if dispatched {
        tracing::info!(
            worker = %event.worker_thread_id,
            status = event.status.as_str(),
            "Dispatched worker completion callback"
        );
    } else {
        tracing::warn!(
            worker = %event.worker_thread_id,
            "Failed to dispatch worker completion callback"
        );
    }
}

/// One-line notice posted in the orchestrator topic when a worker turn ends:
/// status glyph, worker title linked per [`worker_link`] when a link exists.
fn worker_update_notice(event: &CompletionEvent, link: Option<&str>) -> String {
    let title = thread_persistence::read_thread_title(&event.worker_thread_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| event.worker_thread_id.chars().take(8).collect());
    let title = escape_html(&title);
    let worker = match link {
        Some(url) => format!("<a href=\"{url}\">{title}</a>"),
        None => title,
    };
    let (glyph, verb) = match event.status {
        WorkerStatus::Completed => ("✅", "finished"),
        WorkerStatus::Cancelled => ("🚫", "was cancelled"),
        _ => ("❌", "failed"),
    };
    format!("{glyph} Worker 🛠 {worker} {verb} · reviewing…")
}

/// Compact synthetic prompt describing one finished worker turn.
///
/// `worker_context` is the [`zdx_engine::tools::orchestrator::thread_context_json`]
/// value for the worker: the latest recorded request's input tokens with its
/// limit and percentage. It is rendered as its own metadata section after the
/// final message, never merged into it, with no cutoff and no recommendation.
fn build_worker_update_prompt(
    event: &CompletionEvent,
    worker_context: &serde_json::Value,
) -> String {
    let worker = &event.worker_thread_id;
    let status = event.status.as_str();

    let mut prompt = format!(
        "[worker update] Worker thread `{worker}` finished a turn with status: {status}.\n"
    );
    match event.status {
        WorkerStatus::Completed => {
            let text = event.final_text.as_deref().unwrap_or_default();
            prompt.push_str("\nFinal message:\n");
            prompt.push_str(&truncate_chars(text, MAX_CALLBACK_TEXT_CHARS));
            prompt.push('\n');
        }
        _ => {
            if let Some(error) = event.error.as_deref() {
                prompt.push_str("\nError:\n");
                prompt.push_str(&truncate_chars(error, MAX_CALLBACK_TEXT_CHARS));
                prompt.push('\n');
            }
        }
    }
    prompt.push_str(&worker_context_section(worker_context));
    prompt
}

/// Runtime context-usage metadata for a worker completion prompt.
///
/// Same basis as `Get_Thread_Status`: the latest recorded request's input
/// tokens (including cache reads/writes), not cumulative spend, with output
/// tokens excluded. Unknown usage renders as `unknown`, never `0%`; known
/// tokens without a resolvable limit keep the tokens with the limit unknown.
/// Deliberately reports only: no cutoff, no fresh-thread recommendation.
fn worker_context_section(worker_context: &serde_json::Value) -> String {
    let input = worker_context
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64);
    let limit = worker_context
        .get("context_limit")
        .and_then(serde_json::Value::as_u64)
        .filter(|limit| *limit > 0);
    let percent = worker_context
        .get("percent_used")
        .and_then(serde_json::Value::as_f64)
        .filter(|percent| percent.is_finite());
    match (input, limit, percent) {
        (Some(input), Some(limit), Some(percent)) => format!(
            "\nWorker context: ~{:.0}% used ({} / {} tokens)\nBasis: last recorded request input\n",
            percent,
            format_compact_tokens(input),
            format_compact_tokens(limit)
        ),
        (Some(input), _, _) => format!(
            "\nWorker context: {} tokens recorded (limit unknown)\nBasis: last recorded request input\n",
            format_compact_tokens(input)
        ),
        (None, _, _) => "\nWorker context: unknown (no recorded request input)\n".to_string(),
    }
}

/// Compact token count for prompts (`120k`, `1M`); an estimate, so whole
/// units are enough.
fn format_compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.0}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn summarize_worker_error(error: &str) -> String {
    // Strip ANSI escape sequences.
    let stripped = strip_ansi_escapes(error);

    // If the error indicates a provider overload or rate limit on terminal failure,
    // summarize cleanly without misleading "retrying" claims.
    let lower = stripped.to_lowercase();
    if lower.contains("overloaded_error") || lower.contains(": overloaded") {
        return "provider overloaded".to_string();
    }
    if lower.contains("rate_limit") || lower.contains("rate limit") {
        return "provider rate limited".to_string();
    }

    // Otherwise, pick the most informative line (ignoring boilerplate log lines).
    for line in stripped.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(msg) = trimmed.strip_prefix("execute prompt: ") {
            return escape_html(&truncate_chars(msg, 200));
        }
        if let Some(msg) = trimmed.strip_prefix("provider: ") {
            return escape_html(&truncate_chars(msg, 200));
        }
        if let Some(msg) = trimmed.strip_prefix("Subagent failed: ") {
            // Avoid returning raw timestamped tracing lines.
            if !msg.contains("WARN") && !msg.contains("INFO") && !msg.contains("ERROR") {
                return escape_html(&truncate_chars(msg, 200));
            }
        }
    }

    format!(
        "<pre>{}</pre>",
        escape_html(&truncate_chars(&stripped, 500))
    )
}

fn strip_ansi_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}… [truncated — use Read_Thread for the rest]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(status: WorkerStatus) -> CompletionEvent {
        CompletionEvent {
            owner_thread_id: "owner".to_string(),
            worker_thread_id: "worker-1".to_string(),
            status,
            final_text: None,
            error: None,
            mirror_url: None,
        }
    }

    #[test]
    fn completed_prompt_carries_final_text_without_topic_link() {
        let prompt = build_worker_update_prompt(
            &CompletionEvent {
                final_text: Some("all done".to_string()),
                mirror_url: Some("https://t.me/c/1/2".to_string()),
                ..completion(WorkerStatus::Completed)
            },
            &serde_json::Value::Null,
        );
        assert!(prompt.starts_with("[worker update]"));
        assert!(prompt.contains("`worker-1`"));
        assert!(prompt.contains("status: completed"));
        assert!(!prompt.contains("https://t.me/c/1/2"));
        assert!(prompt.contains("all done"));
        assert!(prompt.contains("Worker context: unknown"));
    }

    #[test]
    fn failed_prompt_carries_error_and_bounds_length() {
        let prompt = build_worker_update_prompt(
            &CompletionEvent {
                error: Some("x".repeat(10_000)),
                ..completion(WorkerStatus::Failed)
            },
            &serde_json::Value::Null,
        );
        assert!(prompt.contains("status: failed"));
        assert!(prompt.contains("[truncated"));
        assert!(prompt.chars().count() < 3100);
    }

    #[test]
    fn worker_context_reports_percent_and_basis() {
        let prompt = build_worker_update_prompt(
            &CompletionEvent {
                final_text: Some("all done".to_string()),
                ..completion(WorkerStatus::Completed)
            },
            &serde_json::json!({
                "input_tokens": 120_000,
                "context_limit": 200_000,
                "percent_used": 60.0,
                "basis": "last_recorded_request_input",
            }),
        );
        assert!(prompt.contains("Worker context: ~60% used (120k / 200k tokens)"));
        assert!(prompt.contains("Basis: last recorded request input"));
    }

    #[test]
    fn worker_context_unknown_is_never_zero_percent() {
        let prompt = build_worker_update_prompt(
            &completion(WorkerStatus::Completed),
            &serde_json::Value::Null,
        );
        assert!(prompt.contains("Worker context: unknown"));
        assert!(!prompt.contains("0%"));
    }

    #[test]
    fn worker_context_keeps_tokens_when_limit_is_unknown() {
        let prompt = build_worker_update_prompt(
            &completion(WorkerStatus::Completed),
            &serde_json::json!({ "input_tokens": 120_000 }),
        );
        assert!(prompt.contains("120k tokens recorded (limit unknown)"));
        assert!(prompt.contains("Basis: last recorded request input"));
        assert!(!prompt.contains("used ("));
    }

    /// The metadata is its own section after the final answer: it must not be
    /// merged into the worker's text, and it reports only with no cutoff or
    /// fresh-thread recommendation.
    #[test]
    fn worker_context_is_separate_metadata_without_recommendation() {
        let prompt = build_worker_update_prompt(
            &CompletionEvent {
                final_text: Some("shipped it".to_string()),
                ..completion(WorkerStatus::Completed)
            },
            &serde_json::json!({
                "input_tokens": 120_000,
                "context_limit": 200_000,
                "percent_used": 60.0,
                "basis": "last_recorded_request_input",
            }),
        );
        let final_at = prompt.find("Final message:").expect("final answer section");
        let context_at = prompt
            .find("Worker context:")
            .expect("context metadata section");
        assert!(
            final_at < context_at,
            "context metadata follows the final answer, it is not part of it"
        );
        assert!(prompt.contains("shipped it"));
        assert!(!prompt.contains("thread size"));
        assert!(!prompt.contains("fresh thread"));
        assert!(!prompt.contains("new thread"));
    }

    #[test]
    fn update_notice_links_the_worker_when_a_link_exists() {
        let linked = worker_update_notice(
            &completion(WorkerStatus::Completed),
            Some("https://t.me/zdx_bot/app?startapp=worker-1"),
        );
        assert_eq!(
            linked,
            "✅ Worker 🛠 <a href=\"https://t.me/zdx_bot/app?startapp=worker-1\">worker-1</a> finished · reviewing…"
        );
        // No thread on disk → short id; no link → plain text.
        let plain = worker_update_notice(&completion(WorkerStatus::Failed), None);
        assert_eq!(plain, "❌ Worker 🛠 worker-1 failed · reviewing…");
    }

    #[test]
    fn summarizes_worker_error_cleanly() {
        let overload_log = "Subagent failed: \u{1b}[2m2026-09-03T13:28:05.481367Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m \u{1b}[2mzdx_engine::core::agent\u{1b}[0m:\u{1b}[0m Transient provider error, retrying attempt=2 max=3 delay_ms=4000 error=overloaded_error: Overloaded\nprovider: overloaded_error: Overloaded";
        assert_eq!(summarize_worker_error(overload_log), "provider overloaded");

        let simple_err = "execute prompt: something broke";
        assert_eq!(summarize_worker_error(simple_err), "something broke");
    }

    #[test]
    fn mirror_keyboard_has_cancel_and_optional_open_thread() {
        assert!(CANCEL_CALLBACK.len() <= 64);

        let with_app = mirror_keyboard_for_url(Some("https://t.me/zdx_bot/threads"), "w-1");
        let row = &with_app.inline_keyboard[0];
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].callback_data.as_deref(), Some(CANCEL_CALLBACK));
        assert_eq!(
            row[1].url.as_deref(),
            Some("https://t.me/zdx_bot/threads?startapp=w-1")
        );

        let without_app = mirror_keyboard_for_url(None, "w-1");
        assert_eq!(without_app.inline_keyboard[0].len(), 1);
    }

    fn live_turn() -> LiveTurn {
        LiveTurn {
            mirror: MirrorTopic { chat: -1, topic: 1 },
            message_id: 1,
            status: STATUS_WAITING.to_string(),
            dirty: false,
            last_edit: Instant::now(),
        }
    }

    #[test]
    fn live_turn_tracks_only_the_current_tool() {
        let mut turn = live_turn();
        assert!(turn.apply(&WorkerActivity::ToolStarted {
            id: "a".to_string(),
            name: "bash".to_string(),
        }));
        assert_eq!(turn.status, "🔧 Running `bash`...");
        // Arguments are noise here; the thread has the detail.
        assert!(!turn.apply(&WorkerActivity::ToolInput {
            id: "a".to_string(),
            arg: "cat <file>".to_string(),
        }));
        assert!(turn.apply(&WorkerActivity::ToolFinished {
            id: "a".to_string(),
            ok: true,
        }));
        assert_eq!(turn.status, STATUS_WAITING);
        // Same visible status again is not a change.
        assert!(!turn.apply(&WorkerActivity::ToolFinished {
            id: "a".to_string(),
            ok: false,
        }));
    }

    #[test]
    fn flush_is_due_only_while_dirty() {
        let mut turn = live_turn();
        assert_eq!(turn.next_flush_at(), None);
        turn.apply(&WorkerActivity::ToolStarted {
            id: "a".to_string(),
            name: "glob".to_string(),
        });
        assert_eq!(
            turn.next_flush_at(),
            Some(turn.last_edit + LIVE_EDIT_INTERVAL)
        );
    }
}
