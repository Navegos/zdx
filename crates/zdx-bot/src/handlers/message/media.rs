use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::telegram::markdown::to_telegram_html;
use crate::telegram::{MediaSend, Message, TelegramClient};

const MEDIA_BLOCK_OPEN: &str = "<medias>";

const MEDIA_BLOCK_CLOSE: &str = "</medias>";

const MEDIA_TAG_OPEN: &str = "<media";

const MEDIA_TAG_CLOSE: &str = "</media>";

/// Sends one local file with the Telegram method that fits its type
/// (photo, voice note, audio, or document).
pub(super) async fn send_media_file(
    client: &TelegramClient,
    chat_id: i64,
    path: &Path,
    options: &MediaSend<'_>,
) -> Result<Message> {
    if is_image_path(path) {
        client.send_photo_from_path(chat_id, path, options).await
    } else if is_voice_note_path(path) {
        client.send_voice_from_path(chat_id, path, options).await
    } else if is_audio_path(path) {
        client.send_audio_from_path(chat_id, path, options).await
    } else {
        client.send_document_from_path(chat_id, path, options).await
    }
}

#[derive(Default)]
pub(super) struct ParsedFinalResponse {
    pub(super) text: String,
    pub(super) media_paths: Vec<PathBuf>,
    pub(super) followups: Vec<String>,
}

pub(super) fn parse_final_response(final_text: &str) -> ParsedFinalResponse {
    let (text_without_followups, followups) = zdx_engine::followups::extract_followups(final_text);
    let text_without_wrappers = strip_media_wrappers(&text_without_followups);
    let (text_without_media_tags, raw_media_values) = extract_media_tags(&text_without_wrappers);
    let mut media_paths = Vec::new();
    let mut seen = HashSet::new();

    for raw in raw_media_values {
        if let Some(path) = parse_media_path(&raw)
            && seen.insert(path.clone())
        {
            media_paths.push(path);
        }
    }

    ParsedFinalResponse {
        text: to_telegram_html(&text_without_media_tags),
        media_paths,
        followups,
    }
}

fn strip_media_wrappers(input: &str) -> String {
    input
        .replace(MEDIA_BLOCK_OPEN, "")
        .replace(MEDIA_BLOCK_CLOSE, "")
}

fn extract_media_tags(input: &str) -> (String, Vec<String>) {
    let mut cleaned = String::new();
    let mut media_values = Vec::new();
    let mut cursor = 0;

    while let Some(start_rel) = input[cursor..].find(MEDIA_TAG_OPEN) {
        let start = cursor + start_rel;
        let Some(after_tag_name) = input.as_bytes().get(start + MEDIA_TAG_OPEN.len()) else {
            break;
        };
        // Skip accidental matches such as "<medias>".
        if *after_tag_name == b's' {
            let skip_to = start + MEDIA_TAG_OPEN.len();
            cleaned.push_str(&input[cursor..skip_to]);
            cursor = skip_to;
            continue;
        }

        cleaned.push_str(&input[cursor..start]);

        let Some(open_end_rel) = input[start..].find('>') else {
            cleaned.push_str(&input[start..]);
            cursor = input.len();
            break;
        };
        let open_end = start + open_end_rel;
        let open_tag = &input[start..=open_end];

        if open_tag.ends_with("/>") {
            cursor = open_end + 1;
            continue;
        }

        let content_start = open_end + 1;
        let Some(close_rel) = input[content_start..].find(MEDIA_TAG_CLOSE) else {
            cleaned.push_str(&input[start..]);
            cursor = input.len();
            break;
        };
        let content_end = content_start + close_rel;
        let inner = input[content_start..content_end].trim();
        if !inner.is_empty() {
            media_values.push(inner.to_string());
        }

        cursor = content_end + MEDIA_TAG_CLOSE.len();
    }

    if cursor < input.len() {
        cleaned.push_str(&input[cursor..]);
    }

    (cleaned, media_values)
}

fn parse_media_path(raw: &str) -> Option<PathBuf> {
    let candidate = raw
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim_end_matches([',', ';']);

    if candidate.starts_with('/') {
        Some(PathBuf::from(candidate))
    } else {
        None
    }
}

pub(super) fn is_valid_media_path(path: &Path) -> bool {
    path.is_absolute() && std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

pub(super) fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp"
            )
        })
}

pub(super) fn is_voice_note_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "ogg" | "oga" | "opus"))
}

pub(super) fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp3" | "m4a" | "wav" | "aac" | "flac"
            )
        })
}
