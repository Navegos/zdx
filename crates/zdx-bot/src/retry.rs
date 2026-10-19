//! Error-recovery "Try again" buttons.
//!
//! Two failures get one:
//! - A turn that fails without producing a reply (for example an
//!   `overloaded_error`). Tapping re-runs the same turn from the
//!   already-persisted thread state — no new user message is appended, so the
//!   failed request is retried verbatim.
//! - A message whose Telegram attachment could not be downloaded after every
//!   try. No turn ran for it. Tapping dispatches the same message again, so
//!   the user does not have to resend it: Telegram still has the file.
//!
//! Entries sit unanswered without blocking anything.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::bot::context::BotContext;
use crate::bot::queue::{ChatQueueMap, dispatch_message};
use crate::telegram::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, TelegramClient,
};

/// What a pending "Try again" button needs to re-run the failed turn.
#[allow(clippy::struct_field_names)] // these are distinct ids, not a shared prefix
pub(crate) struct RetryRequest {
    pub thread_id: String,
    pub topic_id: Option<i64>,
    pub reply_to_message_id: Option<i64>,
    /// The user's original message id, reused to key the new turn's status.
    pub user_message_id: i64,
}

/// What a tap on "Try again" does.
pub(crate) enum RetryAction {
    /// Re-run a failed agent turn.
    Turn(RetryRequest),
    /// Dispatch a message again whose attachment download failed.
    Download(Box<Message>),
}

/// Pending retries keyed by (`chat_id`, `message_id`) of the buttons message.
/// Entries stay until tapped, dismissed, or restart.
pub(crate) type RetryMap = Arc<Mutex<HashMap<(i64, i64), RetryAction>>>;

pub(crate) fn new_retry_map() -> RetryMap {
    Arc::new(Mutex::new(HashMap::new()))
}

fn retry_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![InlineKeyboardButton::callback("🔄 Try again", "retry:go")],
            vec![InlineKeyboardButton::callback("✕ Dismiss", "retry:x")],
        ],
    }
}

/// Sends the "Try again" button and registers the retry request for lookup.
pub(crate) async fn send_retry_buttons(context: &BotContext, chat_id: i64, request: RetryRequest) {
    let topic_id = request.topic_id;
    send_and_register(
        context,
        chat_id,
        "🔁 <i>Retry the failed request?</i>",
        None,
        topic_id,
        RetryAction::Turn(request),
    )
    .await;
}

/// Tells the user an attachment of `message` could not be downloaded and
/// offers to load the same message again.
pub(crate) async fn offer_download_retry(context: &BotContext, message: Message, label: &str) {
    let chat_id = message.chat.id;
    let topic_id = message.effective_thread_id();
    // A message routed out of General stays there, so it cannot be quoted in
    // its new topic.
    let reply_to = (!message.synthetic_topic_routed_from_general).then_some(message.id);
    let text = format!(
        "⚠️ I couldn't download your {label} from Telegram. Telegram still has it, so you don't need to send it again."
    );
    send_and_register(
        context,
        chat_id,
        &text,
        reply_to,
        topic_id,
        RetryAction::Download(Box::new(message)),
    )
    .await;
}

async fn send_and_register(
    context: &BotContext,
    chat_id: i64,
    text: &str,
    reply_to: Option<i64>,
    topic_id: Option<i64>,
    action: RetryAction,
) {
    match context
        .client()
        .send_message_with_markup(chat_id, text, reply_to, topic_id, &retry_keyboard())
        .await
    {
        Ok(message) => {
            let mut map = context.retry_map().lock().expect("retry lock poisoned");
            map.insert((chat_id, message.id), action);
        }
        Err(err) => {
            tracing::warn!(chat_id, err = %format!("{err:#}"), "Failed to send retry button");
        }
    }
}

/// Handles a `retry:{go|x}` callback.
pub(crate) async fn handle_callback(
    context: &Arc<BotContext>,
    queues: &ChatQueueMap,
    client: &TelegramClient,
    callback: &CallbackQuery,
    data: &str,
) {
    let Some(message) = callback.message.as_ref() else {
        let _ = client
            .answer_callback_query(&callback.id, Some("No message context"))
            .await;
        return;
    };
    let chat_id = message.chat.id;

    // Dismiss: drop the request and delete the buttons message.
    if data == "x" {
        {
            let mut map = context.retry_map().lock().expect("retry lock poisoned");
            map.remove(&(chat_id, message.id));
        }
        let _ = client.delete_message(chat_id, message.id).await;
        let _ = client.answer_callback_query(&callback.id, None).await;
        return;
    }

    if data != "go" {
        let _ = client.answer_callback_query(&callback.id, None).await;
        return;
    }

    let action = {
        let mut map = context.retry_map().lock().expect("retry lock poisoned");
        map.remove(&(chat_id, message.id))
    };
    match action {
        None => {
            let _ = client
                .answer_callback_query(&callback.id, Some("This retry is no longer active"))
                .await;
        }
        Some(RetryAction::Turn(request)) => {
            retry_turn(context, client, callback, chat_id, message.id, request).await;
        }
        Some(RetryAction::Download(original)) => {
            let _ = client
                .edit_message_text(
                    chat_id,
                    message.id,
                    "🔄 Loading it again…",
                    Some(&InlineKeyboardMarkup::empty()),
                )
                .await;
            let _ = client
                .answer_callback_query(&callback.id, Some("Loading it again…"))
                .await;
            dispatch_message(queues, context, *original).await;
        }
    }
}

async fn retry_turn(
    context: &Arc<BotContext>,
    client: &TelegramClient,
    callback: &CallbackQuery,
    chat_id: i64,
    buttons_message_id: i64,
    request: RetryRequest,
) {
    // Defense in depth: never re-run a turn inside a worker mirror topic — the
    // worker's child process owns that thread's JSONL (normally unreachable,
    // since mirror topics don't post retry buttons).
    let topic_thread_id = crate::handlers::message::thread_id_for_chat(chat_id, request.topic_id);
    if zdx_engine::core::thread_persistence::read_thread_worker_topic(&topic_thread_id)
        .unwrap_or(false)
    {
        let _ = client
            .answer_callback_query(&callback.id, Some("Not available in a worker topic"))
            .await;
        return;
    }

    let _ = client
        .edit_message_text(chat_id, buttons_message_id, "🔄 Retrying…", None)
        .await;
    let _ = client
        .answer_callback_query(&callback.id, Some("Retrying…"))
        .await;

    let context = Arc::clone(context);
    tokio::spawn(async move {
        if let Err(err) = crate::handlers::message::retry_agent_turn(
            &context,
            chat_id,
            request.user_message_id,
            &request.thread_id,
            request.topic_id,
            request.reply_to_message_id,
        )
        .await
        {
            tracing::error!(chat_id, err = %format!("{err:#}"), "Retry agent turn failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::telegram::fake::{FakeTelegram, TOKEN};

    /// The voice message stays with the button, and a tap dispatches that same
    /// message again instead of asking the user to resend it.
    #[tokio::test]
    async fn download_retry_keeps_the_message_and_dispatches_it_on_tap() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let client = TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone());
        let context = Arc::new(BotContext::for_test(
            client.clone(),
            zdx_engine::config::Config::default(),
            std::env::temp_dir(),
        ));
        let queues = crate::bot::queue::new_chat_queues();
        let voice: Message = serde_json::from_value(json!({
            "message_id": 55,
            "chat": { "id": -1001, "type": "supergroup", "is_forum": true },
            "from": { "id": 7, "is_bot": false },
            "message_thread_id": 9,
            "voice": { "file_id": "voice-file" },
        }))
        .expect("message");

        offer_download_retry(&context, voice, "voice message").await;

        let sent = fake.seen();
        let body: serde_json::Value = serde_json::from_str(&sent[0].body).expect("json");
        assert!(
            body["text"]
                .as_str()
                .is_some_and(|text| text.contains("download your voice message")),
            "{body}"
        );
        assert_eq!(body["reply_to_message_id"], 55, "{body}");
        assert_eq!(body["message_thread_id"], 9, "{body}");
        assert_eq!(
            body["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "retry:go"
        );
        let buttons_id = 101;
        let held = {
            let map = context.retry_map().lock().expect("retry lock");
            matches!(map.get(&(-1001, buttons_id)), Some(RetryAction::Download(message)) if message.id == 55 && message.voice.is_some())
        };
        assert!(held, "message held for retry");

        let tap: CallbackQuery = serde_json::from_value(json!({
            "id": "cb1",
            "from": { "id": 7, "is_bot": false },
            "data": "retry:go",
            "message": {
                "message_id": buttons_id,
                "chat": { "id": -1001, "type": "supergroup", "is_forum": true },
                "message_thread_id": 9,
            },
        }))
        .expect("callback");
        handle_callback(&context, &queues, &client, &tap, "go").await;

        assert!(
            context.retry_map().lock().expect("retry lock").is_empty(),
            "a tap uses the retry once"
        );
        assert!(
            fake.methods().contains(&"editMessageText".to_string()),
            "{:?}",
            fake.methods()
        );
    }
}
