//! File cards: how a reply delivers an HTML file without uploading it, and
//! the `⬇ Send file` button that uploads it on request.
//!
//! When the Mini App is configured for the chat, an HTML file in a reply goes
//! out as a plain text message (the card) that names the file, with
//! `↗ Open artifact` (Mini App link) and `⬇ Send file` (`sf:<key>`). Every
//! reply posts new cards. Old cards are never edited, deleted, or refreshed,
//! and nothing keeps them up to date: Open and Send file read the file at its
//! path when they are used, so an old card opens and sends the current bytes,
//! not a snapshot. The card text says so.
//!
//! A Send file tap is resolved from the card alone: its chat and topic name
//! the receiving thread, and the key is looked up in that thread's artifact
//! list with the same resolution the Mini App uses. No state is kept between
//! the card and the tap, so it works after a restart. The document goes out
//! as a reply to the card; the card and its buttons stay as they are.
//!
//! Stale taps: a callback update carries no time, so the bot cannot measure
//! how old a tap is. It acknowledges first. When Telegram refuses that answer
//! as too old (observed at about 10-15 s after the tap, not documented), the
//! tap waited somewhere (for example while the Mac slept), and nothing is
//! uploaded: one notice per card within [`STALE_NOTICE_WINDOW`] asks the user
//! to tap again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zdx_engine::core::artifacts;
use zdx_engine::core::delivery::{DeliveryItem, DeliveryRecord};
use zdx_engine::core::thread_persistence::{Thread, ThreadEvent};
use zdx_engine::telegram::flood::{self, FailureClass};

use crate::bot::context::BotContext;
use crate::handlers::message::{escape_html, resolve_effective_thread_id, thread_id_for_chat};
use crate::telegram::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MediaSend, TelegramClient,
    api_error_description, is_invalid_reply_error,
};

pub(crate) const CALLBACK_PREFIX: &str = "sf:";
pub(crate) const OPEN_ARTIFACT_LABEL: &str = "↗ Open artifact";
const SEND_FILE_LABEL: &str = "⬇ Send file";
/// At most one stale-tap notice per card in this window, so several taps
/// queued while the Mac slept produce one message.
pub(crate) const STALE_NOTICE_WINDOW: Duration = Duration::from_mins(10);
const MAX_REASON_CHARS: usize = 200;

/// Per-process Send file bookkeeping: cards with an upload running (a second
/// tap does not upload twice) and the last stale notice per card.
#[derive(Default)]
pub(crate) struct SendFileState {
    in_flight: Mutex<HashSet<(i64, i64)>>,
    stale_notices: Mutex<HashMap<(i64, i64), Instant>>,
}

impl SendFileState {
    fn begin(&self, card: (i64, i64)) -> bool {
        self.in_flight
            .lock()
            .expect("send file lock poisoned")
            .insert(card)
    }

    fn finish(&self, card: (i64, i64)) {
        self.in_flight
            .lock()
            .expect("send file lock poisoned")
            .remove(&card);
    }

    fn should_notify_stale(&self, card: (i64, i64), now: Instant) -> bool {
        let mut notices = self.stale_notices.lock().expect("send file lock poisoned");
        notices.retain(|_, at| now.duration_since(*at) < STALE_NOTICE_WINDOW);
        if notices.contains_key(&card) {
            return false;
        }
        notices.insert(card, now);
        true
    }
}

/// Whether a reply file goes out as a card: HTML, when it has an
/// `Open artifact` link. Without the Mini App a card could not open
/// anything, so the file is uploaded as before.
pub(crate) fn is_card_file(path: &Path, open_url: Option<&str>) -> bool {
    open_url.is_some() && artifacts::kind_for_path(path) == artifacts::ArtifactKind::Html
}

/// Card text: name, size when sent, and that both buttons use the file as
/// it is when tapped.
pub(crate) fn card_text(path: &Path) -> String {
    let name = file_name(path);
    let size = std::fs::metadata(path).map_or(0, |meta| meta.len());
    format!(
        "📄 <b>{}</b>\n{} · HTML · not uploaded\n<i>Open artifact and Send file use the file as it is now on the Mac.</i>",
        escape_html(&name),
        format_bytes(size)
    )
}

pub(crate) fn card_keyboard(open_url: &str, path: &Path) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![
            InlineKeyboardButton::url(OPEN_ARTIFACT_LABEL, open_url),
            InlineKeyboardButton::callback(SEND_FILE_LABEL, callback_data(path)),
        ]],
    }
}

/// `sf:<key>`: 19 bytes, within Telegram's 64-byte `callback_data` limit.
pub(crate) fn callback_data(path: &Path) -> String {
    format!("{CALLBACK_PREFIX}{}", artifacts::artifact_key(path))
}

fn parse_key(rest: &str) -> Option<&str> {
    (rest.len() == artifacts::ARTIFACT_KEY_LEN
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(rest)
}

fn is_stale_answer(err: &anyhow::Error) -> bool {
    api_error_description(err).is_some_and(|description| description.contains("query is too old"))
}

/// Handles a `sf:<key>` tap. Answers the tap on the polling loop, then
/// uploads (or posts a notice) on its own task, returned for tests.
pub(crate) async fn handle_callback(
    context: &Arc<BotContext>,
    client: &TelegramClient,
    callback: &CallbackQuery,
    rest: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(message) = callback.message.as_ref() else {
        let _ = client
            .answer_callback_query(&callback.id, Some("No message context"))
            .await;
        return None;
    };
    let Some(key) = parse_key(rest) else {
        let _ = client
            .answer_callback_query(&callback.id, Some("This button no longer works"))
            .await;
        return None;
    };
    let chat_id = message.chat.id;
    let card_id = message.id;
    let topic = message.effective_thread_id();
    let card = (chat_id, card_id);

    if !context.send_files().begin(card) {
        let _ = client
            .answer_callback_query(&callback.id, Some("Already sending this file"))
            .await;
        return None;
    }

    if let Err(err) = client
        .answer_callback_query(&callback.id, Some("Sending the file…"))
        .await
    {
        if is_stale_answer(&err) {
            context.send_files().finish(card);
            tracing::info!(chat_id, card_id, "Send file tap arrived too late; not sent");
            if !context
                .send_files()
                .should_notify_stale(card, Instant::now())
            {
                return None;
            }
            let context = Arc::clone(context);
            return Some(tokio::spawn(async move {
                post_notice(
                    &context,
                    chat_id,
                    topic,
                    card_id,
                    "⏱ That ⬇ Send file tap reached the bot late (the Mac was probably asleep), so nothing was sent. Tap ⬇ Send file again.",
                )
                .await;
            }));
        }
        tracing::warn!(err = %format!("{err:#}"), "Failed to answer Send file callback");
    }

    let context = Arc::clone(context);
    let key = key.to_string();
    Some(tokio::spawn(async move {
        send_file(&context, chat_id, topic, card_id, key).await;
        context.send_files().finish(card);
    }))
}

async fn send_file(
    context: &BotContext,
    chat_id: i64,
    topic: Option<i64>,
    card_id: i64,
    key: String,
) {
    let thread_id = resolve_effective_thread_id(&thread_id_for_chat(chat_id, topic));
    let lookup_thread = thread_id.clone();
    let resolved = tokio::task::spawn_blocking(move || resolve(&lookup_thread, &key))
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "Send file lookup task failed");
            Err("the file lookup failed".to_string())
        });
    let (path, name) = match resolved {
        Ok(found) => found,
        Err(reason) => {
            post_notice(
                context,
                chat_id,
                topic,
                card_id,
                &format!("⚠️ Not sent: {}", escape_html(&reason)),
            )
            .await;
            return;
        }
    };

    let client = context.client().reliable();
    let mut options = MediaSend {
        reply_to_message_id: Some(card_id),
        message_thread_id: topic,
        ..MediaSend::default()
    };
    let sent = match client
        .send_document_from_path(chat_id, &path, &options)
        .await
    {
        Err(err) if is_invalid_reply_error(&err) => {
            options.reply_to_message_id = None;
            client
                .send_document_from_path(chat_id, &path, &options)
                .await
        }
        other => other,
    };
    match sent {
        Ok(message) => {
            tracing::info!(chat_id, card_id, message_id = message.id, file = %name, "Send file delivered");
            record_sent(&thread_id, chat_id, topic, &path, message.id);
        }
        Err(err) => {
            let reason = truncate(
                &api_error_description(&err).map_or_else(|| format!("{err:#}"), str::to_string),
            );
            tracing::warn!(chat_id, card_id, file = %name, err = %format!("{err:#}"), "Send file failed");
            let text = match flood::classify(&err) {
                FailureClass::Unknown => format!(
                    "⚠️ {} may not have arrived ({}). Check above before you tap ⬇ Send file again.",
                    escape_html(&name),
                    escape_html(&reason)
                ),
                _ => format!(
                    "⚠️ {} was not sent ({}). Tap ⬇ Send file to try again.",
                    escape_html(&name),
                    escape_html(&reason)
                ),
            };
            post_notice(context, chat_id, topic, card_id, &text).await;
        }
    }
}

/// The card's key in its thread's artifact list, checked by the same rules as
/// the Mini App download route. Errors are user-facing reasons.
fn resolve(thread_id: &str, key: &str) -> Result<(PathBuf, String), String> {
    let rows = artifacts::list_thread_artifacts(thread_id).map_err(|err| {
        tracing::warn!(thread_id, err = %format!("{err:#}"), "Send file could not list artifacts");
        "the file list of this chat could not be read".to_string()
    })?;
    let row = rows
        .into_iter()
        .find(|row| row.key == key)
        .ok_or_else(|| "this file is no longer part of this chat".to_string())?;
    if !row.exists {
        return Err(format!("{} is no longer on the Mac", row.name));
    }
    let (path, _mime) = artifacts::resolve_artifact_for_download(thread_id, &row.path)
        .map_err(|err| {
            tracing::warn!(thread_id, err = %format!("{err:#}"), "Send file could not resolve the file");
            format!("{} is no longer on the Mac", row.name)
        })?;
    Ok((path, row.name))
}

/// Records the upload in the thread like any delivered file. Only a sent
/// file is recorded: a failure is reported to the user in the chat, and a
/// failed record would tell the model its last reply was not delivered.
fn record_sent(thread_id: &str, chat_id: i64, topic: Option<i64>, path: &Path, message_id: i64) {
    let mut item = DeliveryItem::file(path.display().to_string());
    item.mark_sent(Some(message_id));
    let record = DeliveryRecord::new("telegram", Some(chat_id), topic, vec![item]);
    let result = Thread::with_id(thread_id.to_string())
        .and_then(|mut thread| thread.append(&ThreadEvent::delivery(record)));
    if let Err(err) = result {
        tracing::warn!(thread_id, err = %format!("{err:#}"), "Failed to record Send file delivery");
    }
}

async fn post_notice(
    context: &BotContext,
    chat_id: i64,
    topic: Option<i64>,
    card_id: i64,
    text: &str,
) {
    if let Err(err) = context
        .client()
        .paced()
        .send_message(chat_id, text, Some(card_id), topic)
        .await
    {
        tracing::warn!(chat_id, card_id, err = %format!("{err:#}"), "Failed to post Send file notice");
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = "KB";
    for next in ["MB", "GB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value >= 100.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

fn truncate(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_REASON_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_REASON_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zdx_engine::core::artifacts::{artifact_dir_for_thread, artifact_key};
    use zdx_engine::core::thread_persistence::{Thread, ThreadEvent, load_thread_events};

    use super::*;
    use crate::telegram::fake::{FakeTelegram, Reply, TOKEN};

    const CHAT: i64 = -1001;
    const CARD: i64 = 500;

    fn context(fake: &FakeTelegram) -> Arc<BotContext> {
        Arc::new(BotContext::for_test(
            TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone()),
            zdx_engine::config::Config::default(),
            std::env::temp_dir(),
        ))
    }

    fn tap(data: &str, card_id: i64) -> CallbackQuery {
        serde_json::from_value(serde_json::json!({
            "id": format!("cb-{}", uuid::Uuid::new_v4()),
            "from": { "id": 1 },
            "data": data,
            "message": {
                "message_id": card_id,
                "date": 1_700_000_000,
                "chat": { "id": CHAT, "type": "supergroup", "is_forum": true },
                "message_thread_id": 7,
                "text": "📄 report.html",
            },
        }))
        .expect("callback")
    }

    /// A topic thread whose reply sent `name` from its artifact dir.
    fn sent_file(name: &str, body: &str) -> (String, PathBuf) {
        let thread_id = thread_id_for_chat(CHAT, Some(7));
        let path = artifact_dir_for_thread(Some(&thread_id)).join(name);
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        std::fs::write(&path, body).expect("write");
        Thread::with_id(thread_id.clone())
            .expect("thread")
            .append(&ThreadEvent::assistant_message(format!(
                "Done.\n<media>{}</media>",
                path.display()
            )))
            .expect("append");
        (thread_id, path)
    }

    fn assert_no_edits(fake: &FakeTelegram) {
        for method in fake.methods() {
            assert!(
                !method.starts_with("edit") && !method.starts_with("delete"),
                "old messages are never touched: {method}"
            );
        }
    }

    #[test]
    fn card_names_the_file_and_says_it_is_live() {
        let dir = std::env::temp_dir().join(format!("zdx-card-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("a<b>&c.html");
        std::fs::write(&path, vec![b'x'; 2048]).expect("write");

        let text = card_text(&path);
        assert!(text.contains("<b>a&lt;b&gt;&amp;c.html</b>"), "{text}");
        assert!(text.contains("2.0 KB · HTML · not uploaded"), "{text}");
        assert!(text.contains("as it is now"), "{text}");

        let keyboard = card_keyboard("https://t.me/x/app?startapp=a_1", &path);
        let row = &keyboard.inline_keyboard[0];
        assert_eq!(
            row[0].url.as_deref(),
            Some("https://t.me/x/app?startapp=a_1")
        );
        let data = row[1].callback_data.as_deref().expect("callback");
        assert_eq!(data, format!("sf:{}", artifact_key(&path)));
        assert!(data.len() <= 64);
        assert_eq!(parse_key(&data[3..]), Some(&data[3..]));
        assert_eq!(parse_key("../etc/passwd"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_linked_html_becomes_a_card() {
        let html = Path::new("/tmp/x/report.html");
        assert!(is_card_file(html, Some("https://t.me/x/app")));
        assert!(!is_card_file(html, None), "no Mini App: upload as before");
        for other in [
            "/tmp/x/a.png",
            "/tmp/x/a.ogg",
            "/tmp/x/a.mp3",
            "/tmp/x/a.pdf",
            "/tmp/x/a.zip",
            "/tmp/x/a.csv",
        ] {
            assert!(
                !is_card_file(Path::new(other), Some("https://t.me/x/app")),
                "{other}"
            );
        }
    }

    #[test]
    fn one_stale_notice_per_card_and_window() {
        let state = SendFileState::default();
        let now = Instant::now();
        assert!(state.should_notify_stale((1, 2), now));
        assert!(!state.should_notify_stale((1, 2), now + Duration::from_secs(30)));
        assert!(state.should_notify_stale((1, 3), now), "another card");
        assert!(state.should_notify_stale((1, 2), now + STALE_NOTICE_WINDOW));

        assert!(state.begin((1, 2)));
        assert!(!state.begin((1, 2)), "no second upload while one runs");
        state.finish((1, 2));
        assert!(state.begin((1, 2)));
    }

    /// The tap uploads the current bytes as a reply to the card, in its
    /// topic, records the delivery, and leaves the card untouched.
    #[tokio::test]
    async fn send_file_uploads_the_current_file_as_a_reply_to_the_card() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let context = context(&fake);
        let (thread_id, path) = sent_file("report.html", "<p>first</p>");
        std::fs::write(&path, "<p>edited later</p>").expect("edit");

        let data = callback_data(&path);
        let handle = handle_callback(&context, context.client(), &tap(&data, CARD), &data[3..])
            .await
            .expect("task");
        handle.await.expect("join");

        assert_eq!(fake.methods(), ["answerCallbackQuery", "sendDocument"]);
        let seen = fake.seen();
        assert!(seen[0].body.contains("Sending the file"));
        let upload = &seen[1].body;
        assert!(upload.contains("<p>edited later</p>"), "current bytes");
        assert!(upload.contains("name=\"reply_to_message_id\"\r\n\r\n500"));
        assert!(upload.contains("name=\"message_thread_id\"\r\n\r\n7"));
        assert!(!upload.contains("reply_markup"), "no buttons on the upload");
        assert_no_edits(&fake);

        let events = load_thread_events(&thread_id).expect("events");
        let Some(ThreadEvent::Delivery { record, .. }) = events.last() else {
            panic!("delivery recorded");
        };
        assert_eq!(record.items.len(), 1);
        assert_eq!(
            record.items[0].path.as_deref(),
            Some(path.to_str().expect("utf8"))
        );
        assert!(record.items[0].message_id.is_some());
        assert!(!record.items[0].link_only);
    }

    /// Telegram refuses the late acknowledgement: nothing is uploaded, one
    /// notice asks to tap again, and a second late tap adds no notice.
    #[tokio::test]
    async fn a_stale_tap_uploads_nothing_and_asks_once_to_tap_again() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let too_old = Reply::Error(
            400,
            "Bad Request: query is too old and response timeout expired or query ID is invalid",
            None,
        );
        fake.script("answerCallbackQuery", &[too_old.clone(), too_old]);
        let context = context(&fake);
        let (_, path) = sent_file("report.html", "<p>x</p>");
        let data = callback_data(&path);

        let first = handle_callback(&context, context.client(), &tap(&data, CARD), &data[3..])
            .await
            .expect("notice task");
        first.await.expect("join");
        let second =
            handle_callback(&context, context.client(), &tap(&data, CARD), &data[3..]).await;
        assert!(second.is_none(), "one notice per card");

        assert_eq!(
            fake.methods(),
            ["answerCallbackQuery", "sendMessage", "answerCallbackQuery"]
        );
        let notice = &fake.seen()[1].body;
        assert!(notice.contains("Tap ⬇ Send file again"), "{notice}");
        assert!(notice.contains("\"reply_to_message_id\":500"), "{notice}");
        assert_no_edits(&fake);
    }

    /// A key that is not in the topic's artifact list never reads a file.
    #[tokio::test]
    async fn an_unknown_key_sends_nothing() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let context = context(&fake);
        let _ = sent_file("report.html", "<p>x</p>");
        let outside = std::env::temp_dir().join("zdx-not-sent.html");
        let data = callback_data(&outside);

        let handle = handle_callback(&context, context.client(), &tap(&data, CARD), &data[3..])
            .await
            .expect("task");
        handle.await.expect("join");

        assert_eq!(fake.methods(), ["answerCallbackQuery", "sendMessage"]);
        assert!(fake.seen()[1].body.contains("no longer part of this chat"));
    }

    #[tokio::test]
    async fn a_failed_upload_is_reported_and_not_recorded() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        fake.script(
            "sendDocument",
            &[Reply::Error(400, "Bad Request: file is empty", None)],
        );
        let context = context(&fake);
        let (thread_id, path) = sent_file("report.html", "<p>x</p>");
        let before = load_thread_events(&thread_id).expect("events").len();
        let data = callback_data(&path);

        let handle = handle_callback(&context, context.client(), &tap(&data, CARD), &data[3..])
            .await
            .expect("task");
        handle.await.expect("join");

        assert_eq!(
            fake.methods(),
            ["answerCallbackQuery", "sendDocument", "sendMessage"]
        );
        assert!(
            fake.seen()[2]
                .body
                .contains("was not sent (Bad Request: file is empty)")
        );
        assert_eq!(
            load_thread_events(&thread_id).expect("events").len(),
            before
        );
    }
}
