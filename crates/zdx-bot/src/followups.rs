//! Suggested-reply buttons using the internal `<followups>` protocol.
//!
//! The model appends a `<followups>` block to its final reply; the bot strips
//! it and attaches one inline button per suggestion to the reply message
//! itself (a separate "Suggested replies" message only when the reply has no
//! text). A tap dispatches that suggestion as the user's next message (a
//! normal new turn). Taps change only the keyboard, never the message text,
//! because the buttons usually sit on the answer. Nothing blocks while the
//! buttons sit unanswered.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::bot::context::BotContext;
use crate::bot::queue::{ChatQueueMap, dispatch_message};
use crate::telegram::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient};

const MAX_BUTTON_CHARS: usize = 64;

/// Suggestions awaiting a tap, keyed by (`chat_id`, `message_id`) of the
/// buttons message. Entries for ignored suggestions stay until restart;
/// they are tiny.
pub(crate) type FollowupMap = Arc<Mutex<HashMap<(i64, i64), Vec<String>>>>;

pub(crate) fn new_followup_map() -> FollowupMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Inline keyboard for `items`: one button per suggestion plus Dismiss.
/// `None` when there are no suggestions.
pub(crate) fn keyboard(items: &[String]) -> Option<InlineKeyboardMarkup> {
    if items.is_empty() {
        return None;
    }
    let mut rows: Vec<Vec<InlineKeyboardButton>> = items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            vec![InlineKeyboardButton::callback(
                truncate_chars(&strip_markdown_markers(item), MAX_BUTTON_CHARS),
                format!("fu:{idx}"),
            )]
        })
        .collect();
    rows.push(vec![InlineKeyboardButton::callback("✕ Dismiss", "fu:x")]);
    Some(InlineKeyboardMarkup {
        inline_keyboard: rows,
    })
}

/// Registers the suggestions shown on message `message_id` for tap lookup.
pub(crate) fn register(context: &BotContext, chat_id: i64, message_id: i64, items: Vec<String>) {
    if items.is_empty() {
        return;
    }
    context
        .followup_map()
        .lock()
        .expect("followup lock poisoned")
        .insert((chat_id, message_id), items);
}

/// Sends the suggestions as their own message, for replies without text.
pub(crate) async fn send_followups(
    context: &BotContext,
    chat_id: i64,
    topic_id: Option<i64>,
    items: Vec<String>,
) {
    let Some(markup) = keyboard(&items) else {
        return;
    };
    match context
        .client()
        .paced()
        .send_message_with_markup(
            chat_id,
            "💡 <i>Suggested replies</i>",
            None,
            topic_id,
            &markup,
        )
        .await
    {
        Ok(message) => register(context, chat_id, message.id, items),
        Err(err) => {
            tracing::warn!(chat_id, err = %format!("{err:#}"), "Failed to send follow-up buttons");
        }
    }
}

/// Keyboard left after a tap: the chosen suggestion, inert.
fn chosen_keyboard(item: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![InlineKeyboardButton::callback(
            truncate_chars(
                &format!("▶️ {}", strip_markdown_markers(item)),
                MAX_BUTTON_CHARS,
            ),
            "fu:-",
        )]],
    }
}

/// Handles a `fu:{idx}` callback: dispatches the tapped suggestion as the
/// user's next message.
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

    // Dismiss: remove the buttons without starting a turn. The message text
    // stays, since it is usually the reply itself.
    if data == "x" {
        {
            let mut map = context
                .followup_map()
                .lock()
                .expect("followup lock poisoned");
            map.remove(&(chat_id, message.id));
        }
        let _ = client
            .edit_message_reply_markup(chat_id, message.id, &InlineKeyboardMarkup::empty())
            .await;
        let _ = client.answer_callback_query(&callback.id, None).await;
        return;
    }

    let Some(idx) = data.parse::<usize>().ok() else {
        let _ = client.answer_callback_query(&callback.id, None).await;
        return;
    };

    let item = {
        let mut map = context
            .followup_map()
            .lock()
            .expect("followup lock poisoned");
        map.remove(&(chat_id, message.id))
            .and_then(|mut items| (idx < items.len()).then(|| items.swap_remove(idx)))
    };
    let Some(item) = item else {
        let _ = client
            .answer_callback_query(&callback.id, Some("These suggestions are no longer active"))
            .await;
        return;
    };

    let _ = client
        .edit_message_reply_markup(chat_id, message.id, &chosen_keyboard(&item))
        .await;
    let _ = client.answer_callback_query(&callback.id, None).await;

    let chat_kind = if message.chat.is_private() {
        "private"
    } else {
        "supergroup"
    };
    let synthetic: Result<crate::telegram::Message, _> = serde_json::from_value(json!({
        "message_id": message.id,
        "chat": {
            "id": chat_id,
            "type": chat_kind,
            "is_forum": message.chat.is_forum_enabled(),
        },
        "from": { "id": callback.from.id, "is_bot": false },
        "text": item,
        "message_thread_id": message.effective_thread_id(),
    }));
    match synthetic {
        Ok(synthetic) => dispatch_message(queues, context, synthetic).await,
        Err(err) => {
            tracing::error!(chat_id, %err, "Failed to synthesize follow-up message");
        }
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Button labels are plain text, so Markdown markers from the reply would show
/// up literally. Underscores are left alone because identifiers use them.
fn strip_markdown_markers(text: &str) -> String {
    text.replace("__", "")
        .replace(['`', '*'], "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_button_labels() {
        let long = "x".repeat(80);
        let label = truncate_chars(&long, 64);
        assert_eq!(label.chars().count(), 64);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn keyboard_has_one_row_per_item_plus_dismiss() {
        assert!(keyboard(&[]).is_none());
        let markup = keyboard(&["One".to_string(), "Two".to_string()]).expect("keyboard");
        assert_eq!(markup.inline_keyboard.len(), 3);
        let chosen = chosen_keyboard("Two");
        assert_eq!(chosen.inline_keyboard.len(), 1);
    }

    #[test]
    fn strips_markdown_markers_from_button_labels() {
        assert_eq!(
            strip_markdown_markers("Run `just ci-fast` on **zdx_bot**"),
            "Run just ci-fast on zdx_bot"
        );
    }
}
