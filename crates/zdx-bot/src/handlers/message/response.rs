use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use zdx_engine::core::delivery::{self, DeliveryItem, DeliveryRecord, DeliveryStatus};
use zdx_engine::core::thread_persistence::{self, Thread, ThreadEvent};
use zdx_engine::telegram::flood::{self, FailureClass};

use super::ReplyContext;
use super::media::{is_valid_media_path, parse_final_response, send_media_file};
use crate::bot::context::BotContext;
use crate::telegram::markdown::visible_utf16_len;
use crate::telegram::{
    InlineKeyboardMarkup, MediaSend, Message, ReplyParameters, TelegramClient, TextSend,
    is_invalid_reply_error,
};

/// Telegram's caption limit, in characters after entity parsing.
const MAX_CAPTION_LEN: usize = 1024;

/// Delivers a turn's final reply and records what reached the chat.
///
/// The reply is always a fresh message, so its Telegram timestamp marks when
/// the turn ended and completion raises a notification (edits do not). Reply
/// text and files go through the reliable client (paced, explicit flood
/// refusals retried within policy); every outcome is persisted as a
/// `ThreadEvent::Delivery` before and after the sends, and a partial delivery
/// posts one summary.
#[allow(clippy::too_many_arguments)]
pub(super) async fn send_final_response(
    context: &BotContext,
    incoming: &crate::types::IncomingMessage,
    reply_ctx: &ReplyContext,
    status_message_id: Option<i64>,
    final_text: &str,
    thread_id: &str,
    touched_workers: &[String],
    thread: &mut Thread,
) -> Result<()> {
    let parsed = parse_final_response(final_text);
    let has_text = !parsed.text.trim().is_empty();

    // Mark the delivery as running and write its pending record before any
    // await, so a Mini App client polling the thread keeps polling from the
    // first moment until the outcome lands (see `delivery_tracker`).
    let delivery = (has_text || !parsed.media_paths.is_empty()).then(|| {
        let running = context.active_deliveries().start(thread_id);
        let mut items = Vec::new();
        if has_text {
            items.push(DeliveryItem::text());
        }
        items.extend(
            parsed
                .media_paths
                .iter()
                .map(|path| DeliveryItem::file(path.display().to_string())),
        );
        let record = DeliveryRecord::new(
            "telegram",
            Some(incoming.chat_id),
            reply_ctx.topic_id,
            items,
        );
        append_delivery(thread, &record);
        (running, record)
    });

    // Paced, so a running flood wait delays the delete instead of leaving a
    // stale status message with a live Cancel button.
    if let Some(msg_id) = status_message_id
        && let Err(err) = context
            .client()
            .paced()
            .delete_message(incoming.chat_id, msg_id)
            .await
    {
        tracing::warn!(msg_id, err = %format!("{err:#}"), "Failed to delete status message");
    }

    let Some((running, mut record)) = delivery else {
        crate::followups::send_followups(
            context,
            incoming.chat_id,
            reply_ctx.topic_id,
            parsed.followups,
        )
        .await;
        return Ok(());
    };

    let text = has_text.then(|| {
        let text = with_worker_links(context, incoming.chat_id, touched_workers, &parsed.text);
        with_thread_link(context, incoming.chat_id, thread_id, &text)
    });

    // Buttons ride on the reply text (or the captioned file); a reply without
    // text keeps the separate suggestions message.
    let keyboard = if has_text {
        crate::followups::keyboard(&parsed.followups)
    } else {
        None
    };
    let target = ReplyTarget::from_context(incoming.chat_id, reply_ctx);
    let keyboard_message = deliver_reply(
        &context.client().reliable(),
        &target,
        text.as_deref(),
        &parsed.media_paths,
        keyboard.as_ref(),
        &mut record,
    )
    .await;
    append_delivery(thread, &record);
    log_delivery(thread_id, &record);
    // The outcome is persisted; clients may stop polling.
    drop(running);

    match keyboard_message {
        Some(message_id) => {
            crate::followups::register(context, incoming.chat_id, message_id, parsed.followups);
        }
        None if !has_text => {
            crate::followups::send_followups(
                context,
                incoming.chat_id,
                reply_ctx.topic_id,
                parsed.followups,
            )
            .await;
        }
        None => {}
    }

    send_delivery_summary(context, incoming.chat_id, reply_ctx.topic_id, &record).await;
    Ok(())
}

/// Posts one summary after a partial delivery, through the same reliable
/// path. Its own failure is only logged, so a flood cannot cascade into more
/// notices.
async fn send_delivery_summary(
    context: &BotContext,
    chat_id: i64,
    topic_id: Option<i64>,
    record: &DeliveryRecord,
) {
    let Some(summary) = delivery::user_summary(record) else {
        return;
    };
    if let Err(err) = context
        .client()
        .reliable()
        .send_message(chat_id, &super::escape_html(&summary), None, topic_id)
        .await
    {
        tracing::warn!(chat_id, err = %format!("{err:#}"), "Failed to send delivery summary");
    }
}

fn append_delivery(thread: &mut Thread, record: &DeliveryRecord) {
    if let Err(err) = thread.append(&ThreadEvent::delivery(record.clone())) {
        tracing::warn!(err = %format!("{err:#}"), "Failed to record delivery outcome");
    }
}

/// One line per finished delivery: the record id, the thread, and each item's
/// outcome and message id, so the per-call `Telegram call` lines (which carry
/// the same message ids, chat, and topic) can be tied to this record. File
/// names only; no paths or bodies.
fn log_delivery(thread_id: &str, record: &DeliveryRecord) {
    let items = record
        .items
        .iter()
        .map(|item| {
            let name = item
                .path
                .as_deref()
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("text");
            let status = match item.status {
                DeliveryStatus::Pending => "pending",
                DeliveryStatus::Sent => "sent",
                DeliveryStatus::Failed => "failed",
                DeliveryStatus::Unknown => "unknown",
            };
            match item.message_id {
                Some(id) => format!("{name}={status}#{id}"),
                None => format!("{name}={status}"),
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let problems = record.has_problems();
    if problems {
        tracing::warn!(record = %record.id, thread = thread_id, chat = record.chat_id, topic = record.topic_id, items, "Telegram delivery incomplete");
    } else {
        tracing::info!(record = %record.id, thread = thread_id, chat = record.chat_id, topic = record.topic_id, items, "Telegram delivery");
    }
}

/// Where a reply goes in the chat.
#[derive(Debug, Clone)]
pub(super) struct ReplyTarget {
    chat_id: i64,
    topic_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    /// Set for replies quoting a message in another topic; replaces
    /// `reply_to_message_id`.
    cross_topic: Option<ReplyParameters>,
}

impl ReplyTarget {
    fn from_context(chat_id: i64, reply_ctx: &ReplyContext) -> Self {
        Self {
            chat_id,
            topic_id: reply_ctx.topic_id,
            reply_to_message_id: reply_ctx.reply_to_message_id,
            cross_topic: reply_ctx.cross_topic_reply_parameters.clone(),
        }
    }

    fn media_options<'a>(
        &self,
        caption: Option<&'a str>,
        reply_markup: Option<&'a InlineKeyboardMarkup>,
    ) -> MediaSend<'a> {
        MediaSend {
            caption,
            reply_markup,
            reply_to_message_id: if self.cross_topic.is_some() {
                None
            } else {
                self.reply_to_message_id
            },
            message_thread_id: self.topic_id,
            reply_parameters: self.cross_topic.clone(),
        }
    }
}

/// Sends the reply text and files, filling in each item's status in
/// `record` (items: optional text first, then one per file, in order).
/// Returns the id of the message that carries `keyboard`, if it was sent.
///
/// A single file whose complete caption fits Telegram's limit is sent with
/// the text as its caption (one message instead of two). Otherwise text goes
/// first and files follow. After a flood refusal that the retry policy could
/// not absorb, the remaining items are not attempted. An ambiguous failure is
/// recorded as unknown and never resent.
pub(super) async fn deliver_reply(
    client: &TelegramClient,
    target: &ReplyTarget,
    text: Option<&str>,
    files: &[PathBuf],
    keyboard: Option<&InlineKeyboardMarkup>,
    record: &mut DeliveryRecord,
) -> Option<i64> {
    let file_offset = usize::from(text.is_some());
    for (idx, path) in files.iter().enumerate() {
        if !is_valid_media_path(path) {
            record.items[file_offset + idx].mark_failed("file not found or not a regular file");
        }
    }

    if let Some(text) = text
        && let [path] = files
        && record.items[1].status == DeliveryStatus::Pending
        && visible_utf16_len(text) <= MAX_CAPTION_LEN
    {
        let options = target.media_options(Some(text), keyboard);
        match send_media_file(client, target.chat_id, path, &options).await {
            Ok(message) => {
                record.items[0].mark_sent(Some(message.id));
                record.items[1].mark_sent(Some(message.id));
                return Some(message.id);
            }
            Err(err) => {
                let class = flood::classify(&err);
                let error = format!("{err:#}");
                match class {
                    FailureClass::Unknown => {
                        record.items[0].mark_unknown(&error);
                        record.items[1].mark_unknown(&error);
                        return None;
                    }
                    FailureClass::FloodWait(_) => {
                        record.items[0].mark_failed(&error);
                        record.items[1].mark_failed(&error);
                        return None;
                    }
                    // The file itself was refused; the text can still go out.
                    FailureClass::Rejected | FailureClass::NotSent => {
                        record.items[1].mark_failed(&error);
                        let sent = send_reply_text(client, target, text, keyboard).await;
                        return settle(&mut record.items[0], sent).ok().flatten();
                    }
                }
            }
        }
    }

    let mut keyboard_message = None;
    let mut flood_stopped: Option<String> = None;
    if let Some(text) = text {
        let sent = send_reply_text(client, target, text, keyboard).await;
        match settle(&mut record.items[0], sent) {
            Ok(id) => keyboard_message = id,
            Err(FailureClass::FloodWait(_)) => {
                flood_stopped = record.items[0].error.clone();
            }
            Err(_) => {}
        }
    }
    for (idx, path) in files.iter().enumerate() {
        let item = &mut record.items[file_offset + idx];
        if item.status != DeliveryStatus::Pending {
            continue;
        }
        if let Some(reason) = &flood_stopped {
            item.mark_failed(&format!("not attempted after a flood refusal: {reason}"));
            continue;
        }
        let options = target.media_options(None, None);
        let sent = send_media_file(client, target.chat_id, path, &options)
            .await
            .map(|message| message.id);
        if let Err(FailureClass::FloodWait(_)) = settle(item, sent.map(Some)) {
            flood_stopped = item.error.clone();
        }
    }
    keyboard_message
}

/// Records a send result on `item`. Returns the message id on success, or the
/// failure class.
fn settle(item: &mut DeliveryItem, sent: Result<Option<i64>>) -> Result<Option<i64>, FailureClass> {
    match sent {
        Ok(id) => {
            item.mark_sent(id);
            Ok(id)
        }
        Err(err) => {
            let class = flood::classify(&err);
            let error = format!("{err:#}");
            if class.is_definitely_undelivered() {
                item.mark_failed(&error);
            } else {
                item.mark_unknown(&error);
            }
            Err(class)
        }
    }
}

/// Sends the reply text. A reply target that no longer exists is dropped and
/// the text resent once, which is safe because the first request was refused.
async fn send_reply_text(
    client: &TelegramClient,
    target: &ReplyTarget,
    text: &str,
    keyboard: Option<&InlineKeyboardMarkup>,
) -> Result<Option<i64>> {
    tracing::info!(chat_id = target.chat_id, "Sending reply");
    let mut request = TextSend {
        chat_id: target.chat_id,
        text,
        reply_to_message_id: None,
        message_thread_id: target.topic_id,
        reply_parameters: target.cross_topic.clone(),
        reply_markup: keyboard,
        disable_preview: true,
    };
    if target.cross_topic.is_some() {
        return client.send_text(&request).await.map(|m| Some(m.id));
    }
    request.reply_to_message_id = target.reply_to_message_id;
    let result: Result<Message> = match client.send_text(&request).await {
        Err(err) if request.reply_to_message_id.is_some() && is_invalid_reply_error(&err) => {
            request.reply_to_message_id = None;
            client.send_text(&request).await
        }
        other => other,
    };
    result.map(|m| Some(m.id))
}

/// Appends a subtle Mini App deep link for this thread to an answer.
///
/// Saves the trip through the pinned thread header just to reach the Mini App.
/// The link is skipped when the Mini App is not configured for this chat, and
/// the answer is sent with link previews disabled so it stays a plain line of
/// text instead of a preview card.
fn with_thread_link(context: &BotContext, chat_id: i64, thread_id: &str, text: &str) -> String {
    append_thread_link(
        super::mini_app_base_url(context, chat_id).as_deref(),
        thread_id,
        text,
    )
}

/// Pure half of [`with_thread_link`], so the rendered suffix is testable
/// without constructing a whole bot context.
pub(crate) fn append_thread_link(base: Option<&str>, thread_id: &str, text: &str) -> String {
    match base {
        Some(base) => {
            format!("{text}\n\n<a href=\"{base}?startapp={thread_id}\">↗ Open thread</a>")
        }
        None => text.to_string(),
    }
}

/// Where a worker link points: its legacy mirror topic when it still has one,
/// otherwise the worker thread in the Mini App. New workers are silent and
/// never get a topic, so for them this is always the Mini App deep link.
/// `None` only when neither exists.
pub(crate) fn worker_link(
    mirror_url: Option<String>,
    mini_app_base: Option<&str>,
    worker_thread_id: &str,
) -> Option<String> {
    mirror_url.or_else(|| mini_app_base.map(|base| format!("{base}?startapp={worker_thread_id}")))
}

/// Appends one `🛠 <title>` link per worker this turn created or messaged, so
/// the user can follow the work without relying on the model to mention it.
/// See [`worker_link`] for where each link points.
fn with_worker_links(
    context: &BotContext,
    chat_id: i64,
    touched_workers: &[String],
    text: &str,
) -> String {
    let mini_app = super::mini_app_base_url(context, chat_id);
    let links: Vec<(String, String)> = touched_workers
        .iter()
        .filter_map(|worker| {
            let url = worker_link(
                context.worker_manager().mirror_url(worker),
                mini_app.as_deref(),
                worker,
            )?;
            let title = thread_persistence::read_thread_title(worker)
                .ok()
                .flatten()
                .unwrap_or_else(|| worker.chars().take(8).collect());
            Some((title, url))
        })
        .collect();
    append_worker_links(&links, text)
}

fn append_worker_links(links: &[(String, String)], text: &str) -> String {
    if links.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.push('\n');
    for (title, url) in links {
        let _ = write!(
            out,
            "\n🛠 <a href=\"{url}\">{}</a>",
            super::escape_html(title)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zdx_engine::core::delivery::{DeliveryItem, DeliveryRecord, DeliveryStatus};

    use super::{ReplyTarget, append_thread_link, append_worker_links, deliver_reply};
    use crate::telegram::fake::{FakeTelegram, Reply, TOKEN};
    use crate::telegram::{InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient};

    const CHAT: i64 = -1001;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(ext: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("zdx-test-{}.{ext}", uuid::Uuid::new_v4()));
            std::fs::write(&path, b"synthetic file").expect("write temp file");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn target() -> ReplyTarget {
        ReplyTarget {
            chat_id: CHAT,
            topic_id: Some(7),
            reply_to_message_id: Some(42),
            cross_topic: None,
        }
    }

    fn keyboard() -> InlineKeyboardMarkup {
        InlineKeyboardMarkup {
            inline_keyboard: vec![vec![InlineKeyboardButton::callback("Next step", "fu:0")]],
        }
    }

    fn record(text: bool, files: &[PathBuf]) -> DeliveryRecord {
        let mut items = Vec::new();
        if text {
            items.push(DeliveryItem::text());
        }
        items.extend(
            files
                .iter()
                .map(|p| DeliveryItem::file(p.display().to_string())),
        );
        DeliveryRecord::new("telegram", Some(CHAT), Some(7), items)
    }

    async fn deliver(
        fake: &FakeTelegram,
        text: Option<&str>,
        files: &[PathBuf],
    ) -> (Option<i64>, DeliveryRecord) {
        let client =
            TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone()).reliable();
        let mut record = record(text.is_some(), files);
        let keyboard = keyboard();
        let id = deliver_reply(
            &client,
            &target(),
            text,
            files,
            Some(&keyboard),
            &mut record,
        )
        .await;
        (id, record)
    }

    fn statuses(record: &DeliveryRecord) -> Vec<DeliveryStatus> {
        record.items.iter().map(|item| item.status).collect()
    }

    /// The pending record and the running mark exist before the first await
    /// (the paced status delete), so a polling client never misses the
    /// delivery; both clear only once the outcome is persisted.
    #[tokio::test]
    async fn delivery_is_recorded_and_running_before_the_status_delete_waits() {
        use zdx_engine::core::thread_persistence::{Thread, ThreadEvent, load_thread_events};

        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        // A flood refusal puts the chat in a 1 s wait, so the paced status
        // delete has to wait.
        fake.script(
            "sendMessage",
            &[Reply::Error(
                429,
                "Too Many Requests: retry after 1",
                Some(1),
            )],
        );
        let client = TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone());
        assert!(client.send_message(CHAT, "x", None, Some(7)).await.is_err());
        let root = std::env::temp_dir();
        let context = std::sync::Arc::new(crate::bot::context::BotContext::for_test(
            client,
            zdx_engine::config::Config::default(),
            root,
        ));

        let thread_id = format!("telegram-delivery-order-{}", uuid::Uuid::new_v4());
        let mut thread = Thread::with_id(thread_id.clone()).expect("thread");
        thread
            .append(&ThreadEvent::user_message("go"))
            .expect("append");
        let incoming = crate::types::IncomingMessage {
            chat_id: CHAT,
            message_id: 42,
            user_id: 1,
            text: Some("go".to_string()),
            images: Vec::new(),
            audios: Vec::new(),
            documents: Vec::new(),
            videos: Vec::new(),
            notices: Vec::new(),
            message_thread_id: Some(7),
            is_forum: true,
        };
        let reply_ctx = super::ReplyContext {
            reply_to_message_id: Some(42),
            topic_id: Some(7),
            cross_topic_reply_parameters: None,
        };

        let task_context = std::sync::Arc::clone(&context);
        let task_thread_id = thread_id.clone();
        let task = tokio::spawn(async move {
            super::send_final_response(
                &task_context,
                &incoming,
                &reply_ctx,
                Some(55),
                "Ready",
                &task_thread_id,
                &[],
                &mut thread,
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(context.active_deliveries().is_active(&thread_id));
        let pending = load_thread_events(&thread_id).expect("events");
        assert!(matches!(pending.last(), Some(ThreadEvent::Delivery { .. })));
        assert_eq!(fake.methods(), ["sendMessage"], "delete still waiting");

        task.await.expect("join").expect("send final response");
        assert!(!context.active_deliveries().is_active(&thread_id));
        assert_eq!(
            fake.methods(),
            ["sendMessage", "deleteMessage", "sendMessage"]
        );
        let events = load_thread_events(&thread_id).expect("events");
        let Some(ThreadEvent::Delivery { record, .. }) = events.last() else {
            panic!("outcome recorded last");
        };
        assert_eq!(record.items[0].status, DeliveryStatus::Sent);
    }

    #[tokio::test]
    async fn short_text_and_one_file_go_out_as_one_captioned_message() {
        let fake = FakeTelegram::start().await;
        let file = TempFile::new("html");
        let (id, record) =
            deliver(&fake, Some("<b>Ready</b>"), std::slice::from_ref(&file.0)).await;

        assert_eq!(fake.methods(), ["sendDocument"]);
        let body = &fake.seen()[0].body;
        assert!(body.contains("<b>Ready</b>"));
        assert!(body.contains("fu:0"), "buttons ride on the captioned file");
        assert_eq!(
            statuses(&record),
            [DeliveryStatus::Sent, DeliveryStatus::Sent]
        );
        assert_eq!(id, record.items[0].message_id);
        assert_eq!(record.items[0].message_id, record.items[1].message_id);
    }

    #[tokio::test]
    async fn long_text_goes_first_with_the_buttons_then_the_file() {
        let fake = FakeTelegram::start().await;
        let file = TempFile::new("html");
        let text = "x".repeat(1100);
        let (id, record) = deliver(&fake, Some(&text), std::slice::from_ref(&file.0)).await;

        assert_eq!(fake.methods(), ["sendMessage", "sendDocument"]);
        let seen = fake.seen();
        assert!(seen[0].body.contains("fu:0"));
        assert!(!seen[1].body.contains("fu:0"));
        assert_eq!(
            statuses(&record),
            [DeliveryStatus::Sent, DeliveryStatus::Sent]
        );
        assert_eq!(id, record.items[0].message_id);
    }

    #[tokio::test]
    async fn an_unabsorbed_flood_refusal_stops_the_remaining_items() {
        let fake = FakeTelegram::start().await;
        fake.script(
            "sendMessage",
            &[Reply::Error(
                429,
                "Too Many Requests: retry after 500",
                Some(500),
            )],
        );
        let a = TempFile::new("html");
        let b = TempFile::new("csv");
        let (id, record) = deliver(&fake, Some("done"), &[a.0.clone(), b.0.clone()]).await;

        assert_eq!(id, None);
        assert_eq!(fake.methods(), ["sendMessage"]);
        assert_eq!(
            statuses(&record),
            [
                DeliveryStatus::Failed,
                DeliveryStatus::Failed,
                DeliveryStatus::Failed
            ]
        );
        assert!(
            record.items[1]
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("not attempted after a flood refusal"))
        );
    }

    #[tokio::test]
    async fn a_refused_captioned_file_still_delivers_the_text() {
        let fake = FakeTelegram::start().await;
        fake.script(
            "sendDocument",
            &[Reply::Error(400, "Bad Request: wrong file type", None)],
        );
        let file = TempFile::new("html");
        let (id, record) = deliver(&fake, Some("done"), std::slice::from_ref(&file.0)).await;

        assert_eq!(fake.methods(), ["sendDocument", "sendMessage"]);
        assert_eq!(
            statuses(&record),
            [DeliveryStatus::Sent, DeliveryStatus::Failed]
        );
        assert_eq!(id, record.items[0].message_id);
    }

    #[tokio::test]
    async fn an_ambiguous_captioned_send_is_unknown_and_not_resent() {
        let fake = FakeTelegram::start().await;
        fake.script("sendDocument", &[Reply::Garbage]);
        let file = TempFile::new("html");
        let (id, record) = deliver(&fake, Some("done"), std::slice::from_ref(&file.0)).await;

        assert_eq!(id, None);
        assert_eq!(fake.methods(), ["sendDocument"]);
        assert_eq!(
            statuses(&record),
            [DeliveryStatus::Unknown, DeliveryStatus::Unknown]
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_recorded_and_the_text_still_goes_out() {
        let fake = FakeTelegram::start().await;
        let missing = std::env::temp_dir().join(format!("zdx-missing-{}", uuid::Uuid::new_v4()));
        let (_, record) = deliver(&fake, Some("done"), &[missing]).await;

        assert_eq!(fake.methods(), ["sendMessage"]);
        assert_eq!(
            statuses(&record),
            [DeliveryStatus::Sent, DeliveryStatus::Failed]
        );
    }

    #[tokio::test]
    async fn only_an_invalid_reply_target_is_dropped_and_resent() {
        let fake = FakeTelegram::start().await;
        fake.script(
            "sendMessage",
            &[Reply::Error(
                400,
                "Bad Request: message to be replied not found",
                None,
            )],
        );
        let (_, record) = deliver(&fake, Some(&"y".repeat(1100)), &[]).await;
        assert_eq!(fake.methods(), ["sendMessage", "sendMessage"]);
        assert!(fake.seen()[0].body.contains("\"reply_to_message_id\":42"));
        assert!(!fake.seen()[1].body.contains("reply_to_message_id"));
        assert_eq!(statuses(&record), [DeliveryStatus::Sent]);

        let fake = FakeTelegram::start().await;
        fake.script(
            "sendMessage",
            &[Reply::Error(400, "Bad Request: chat not found", None)],
        );
        let (_, record) = deliver(&fake, Some("z"), &[]).await;
        assert_eq!(fake.methods(), ["sendMessage"]);
        assert_eq!(statuses(&record), [DeliveryStatus::Failed]);
    }

    #[test]
    fn appends_a_deep_link_only_when_the_mini_app_is_configured() {
        assert_eq!(
            append_thread_link(
                Some("https://t.me/zdx_2026_bot/app"),
                "telegram--1001234567890-topic-17771",
                "done",
            ),
            "done\n\n<a href=\"https://t.me/zdx_2026_bot/app?startapp=telegram--1001234567890-topic-17771\">↗ Open thread</a>"
        );

        // Mini App disabled or unset: the answer must go out untouched.
        assert_eq!(
            append_thread_link(None, "telegram--100-topic-1", "done"),
            "done"
        );
    }

    #[test]
    fn appends_one_escaped_link_per_touched_worker() {
        let links = vec![
            (
                "dub · fix <PR> comments".to_string(),
                "https://t.me/c/1/2".to_string(),
            ),
            ("zdx".to_string(), "https://t.me/c/1/3".to_string()),
        ];
        assert_eq!(
            append_worker_links(&links, "sent"),
            "sent\n\n🛠 <a href=\"https://t.me/c/1/2\">dub · fix &lt;PR&gt; comments</a>\n🛠 <a href=\"https://t.me/c/1/3\">zdx</a>"
        );
        assert_eq!(append_worker_links(&[], "sent"), "sent");
    }
}
