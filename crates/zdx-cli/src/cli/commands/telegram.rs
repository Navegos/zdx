//! Telegram command handlers.
//!
//! Thin CLI shim over `zdx_engine::telegram`, the shared outbound core also
//! used by the native `telegram` tool.

use anyhow::Result;
use zdx_engine::config::Config;
use zdx_engine::telegram::{ParseMode, SentMessage, TelegramOutbound, resolve_bot_token};

fn client(config: &Config, bot_token: Option<&str>) -> Result<TelegramOutbound> {
    let token = resolve_bot_token(config, bot_token)?;
    Ok(TelegramOutbound::new(token))
}

pub async fn create_topic(
    config: &Config,
    bot_token: Option<String>,
    chat_id: i64,
    name: &str,
) -> Result<()> {
    let message_thread_id = client(config, bot_token.as_deref())?
        .create_forum_topic(chat_id, name)
        .await?;
    println!("{message_thread_id}");
    Ok(())
}

pub async fn send_message(
    config: &Config,
    bot_token: Option<String>,
    chat_id: i64,
    message_thread_id: Option<i64>,
    text: &str,
    parse_mode: &str,
    json: bool,
) -> Result<()> {
    let sent = client(config, bot_token.as_deref())?
        .send_message(
            chat_id,
            text,
            message_thread_id,
            ParseMode::parse(parse_mode)?,
        )
        .await?;
    print_receipt("message", &sent, json)
}

pub async fn send_document(
    config: &Config,
    bot_token: Option<String>,
    chat_id: i64,
    message_thread_id: Option<i64>,
    path: &str,
    caption: Option<&str>,
    json: bool,
) -> Result<()> {
    let sent = client(config, bot_token.as_deref())?
        .send_document(
            chat_id,
            std::path::Path::new(path),
            caption,
            message_thread_id,
        )
        .await?;
    print_receipt("document", &sent, json)
}

fn print_receipt(kind: &str, sent: &SentMessage, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(sent)?);
    } else {
        println!("{}", receipt_line(kind, sent));
    }
    Ok(())
}

fn receipt_line(kind: &str, sent: &SentMessage) -> String {
    match &sent.url {
        Some(url) => format!(
            "Sent {kind} to Telegram (message_id {}): {url}",
            sent.message_id
        ),
        None => format!("Sent {kind} to Telegram (message_id {}).", sent.message_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_line_reports_message_id_and_link() {
        let mut sent = SentMessage {
            chat_id: -1_001_234_567_890,
            message_thread_id: Some(17),
            message_id: 4242,
            url: Some("https://t.me/c/1234567890/17/4242".to_string()),
        };
        assert_eq!(
            receipt_line("message", &sent),
            "Sent message to Telegram (message_id 4242): https://t.me/c/1234567890/17/4242"
        );
        sent.url = None;
        assert_eq!(
            receipt_line("document", &sent),
            "Sent document to Telegram (message_id 4242)."
        );
    }

    #[test]
    fn json_receipt_has_stable_fields() {
        let sent = SentMessage {
            chat_id: -1_001_234_567_890,
            message_thread_id: None,
            message_id: 99,
            url: Some("https://t.me/c/1234567890/99".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&sent).unwrap(),
            serde_json::json!({
                "chat_id": -1_001_234_567_890_i64,
                "message_thread_id": null,
                "message_id": 99,
                "url": "https://t.me/c/1234567890/99"
            })
        );
    }
}
