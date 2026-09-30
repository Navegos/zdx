use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio_util::sync::CancellationToken;
use zdx_engine::config::{Config, paths};

use crate::telegram::{Audio, Document, Message, PhotoSize, TelegramClient, Video, Voice};
use crate::transcribe;
use crate::types::{
    IncomingAudio, IncomingDocument, IncomingImage, IncomingMessage, IncomingVideo, VideoStatus,
};

const MAX_IMAGE_BYTES: u64 = 3_932_160; // 3.75MB
const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024; // 25MB
const MAX_DOCUMENT_BYTES: u64 = 25 * 1024 * 1024; // 25MB
const MAX_VIDEO_BYTES: u64 = zdx_engine::media::MAX_INLINE_BYTES as u64;
const SUPPORTED_IMAGE_MIMES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

pub(crate) struct AllowlistConfig<'a> {
    pub user_ids: &'a HashSet<i64>,
    pub chat_ids: &'a HashSet<i64>,
}

struct MessageTarget {
    chat: i64,
    message: i64,
    thread: Option<i64>,
}

struct AttachmentCollection {
    images: Vec<IncomingImage>,
    audios: Vec<IncomingAudio>,
    documents: Vec<IncomingDocument>,
    videos: Vec<IncomingVideo>,
    had_attachments: bool,
}

struct VideoRef<'a> {
    id: &'a str,
    size: Option<u64>,
    name: Option<&'a str>,
}

impl<'a> From<&'a Video> for VideoRef<'a> {
    fn from(video: &'a Video) -> Self {
        Self {
            id: &video.file_id,
            size: video.file_size,
            name: video.file_name.as_deref(),
        }
    }
}

impl<'a> From<&'a Document> for VideoRef<'a> {
    fn from(document: &'a Document) -> Self {
        Self {
            id: &document.file_id,
            size: document.file_size,
            name: document.file_name.as_deref(),
        }
    }
}

struct AudioSource<'a> {
    file_id: &'a str,
    file_size: Option<u64>,
    mime_type: Option<&'a str>,
    file_name_hint: Option<&'a str>,
}

///
/// # Errors
/// Returns an error if the operation fails.
pub(crate) async fn parse_incoming_message(
    client: &TelegramClient,
    allowlist: AllowlistConfig<'_>,
    config: &Config,
    message: Message,
    cancel_token: Option<&CancellationToken>,
) -> Result<Option<IncomingMessage>> {
    let target = MessageTarget {
        chat: message.chat.id,
        message: message.id,
        thread: message.effective_thread_id(),
    };

    let Some(user_id) = validate_access(client, allowlist, &message, &target).await? else {
        return Ok(None);
    };

    let mut text = extract_text(&message);
    let attachments = collect_attachments(client, config, &message, &target, cancel_token).await?;
    let AttachmentCollection {
        images,
        audios,
        documents,
        videos,
        had_attachments,
    } = attachments;

    if text.is_none()
        && images.is_empty()
        && audios.is_empty()
        && documents.is_empty()
        && videos.is_empty()
    {
        return handle_empty_message(client, &target, had_attachments).await;
    }

    if text.as_deref().is_some_and(|value| value.trim().is_empty()) {
        text = None;
    }

    Ok(Some(IncomingMessage {
        chat_id: target.chat,
        message_id: target.message,
        user_id,
        text,
        images,
        audios,
        documents,
        videos,
        message_thread_id: target.thread,
        is_forum: message.chat.is_forum_enabled(),
    }))
}

async fn validate_access(
    client: &TelegramClient,
    allowlist: AllowlistConfig<'_>,
    message: &Message,
    target: &MessageTarget,
) -> Result<Option<i64>> {
    let Some(user) = message.from.as_ref() else {
        tracing::debug!(chat_id = target.chat, "Ignoring message without sender");
        return Ok(None);
    };

    if user.is_bot {
        return Ok(None);
    }

    let user_allowed = allowlist.user_ids.contains(&user.id);

    if message.chat.is_group() {
        if !allowlist.chat_ids.contains(&target.chat) {
            // Mirror the queue-level gate: allow /whereami in non-allowlisted
            // groups only when the sender is allowlisted, so the operator can
            // discover the chat ID before binding it.
            let is_whereami = message
                .text
                .as_deref()
                .and_then(crate::commands::parse_command)
                .is_some_and(|cmd| matches!(cmd, crate::commands::BotCommand::WhereAmI));
            if user_allowed && is_whereami {
                return Ok(Some(user.id));
            }
            tracing::debug!(chat_id = target.chat, "Ignoring non-allowlisted group chat");
            return Ok(None);
        }
    } else if !message.chat.is_private() {
        tracing::debug!(chat_id = target.chat, "Ignoring unsupported chat type");
        return Ok(None);
    }

    if user_allowed {
        return Ok(Some(user.id));
    }

    tracing::warn!(user_id = user.id, chat_id = target.chat, "Denied user");
    let _ = client
        .send_message(
            target.chat,
            "Access denied.",
            Some(target.message),
            target.thread,
        )
        .await;
    Ok(None)
}

#[allow(clippy::too_many_lines)]
async fn collect_attachments(
    client: &TelegramClient,
    config: &Config,
    message: &Message,
    target: &MessageTarget,
    cancel_token: Option<&CancellationToken>,
) -> Result<AttachmentCollection> {
    let mut images = Vec::new();
    let mut audios = Vec::new();
    let mut documents = Vec::new();
    let mut videos = Vec::new();
    let mut had_attachments = false;

    for current in std::iter::once(message).chain(message.grouped_messages.iter()) {
        let current_message_id = current.id;

        if let Some(photos) = current.photo.as_deref() {
            had_attachments = true;
            if let Some(photo) = select_best_photo(photos) {
                push_attachment(
                    &mut images,
                    load_photo_attachment(client, target.chat, current_message_id, photo),
                    "photo attachment",
                )
                .await?;
            }
        }

        if let Some(document) = current.document.as_ref()
            && let Some(mime) = document.mime_type.as_deref()
            && current.animation.is_none()
            && !mime.starts_with("video/")
        {
            had_attachments = true;
            if mime.starts_with("image/") {
                push_attachment(
                    &mut images,
                    load_document_image(client, target.chat, current_message_id, document),
                    "document image",
                )
                .await?;
            } else if mime.starts_with("audio/") {
                push_attachment(
                    &mut audios,
                    load_audio_attachment(
                        client,
                        config,
                        target.chat,
                        current_message_id,
                        document,
                        cancel_token,
                    ),
                    "document audio",
                )
                .await?;
            } else {
                push_attachment(
                    &mut documents,
                    load_generic_document(client, target.chat, current_message_id, document),
                    "generic document",
                )
                .await?;
            }
        }

        if let Some(voice) = current.voice.as_ref() {
            had_attachments = true;
            push_attachment(
                &mut audios,
                load_voice_attachment(
                    client,
                    config,
                    target.chat,
                    current_message_id,
                    voice,
                    cancel_token,
                ),
                "voice attachment",
            )
            .await?;
        }

        if let Some(audio) = current.audio.as_ref() {
            had_attachments = true;
            push_attachment(
                &mut audios,
                load_audio_message(
                    client,
                    config,
                    target.chat,
                    current_message_id,
                    audio,
                    cancel_token,
                ),
                "audio message",
            )
            .await?;
        }

        for (kind, video) in video_attachments(current) {
            had_attachments = true;
            push_attachment(
                &mut videos,
                load_video(client, target.chat, current_message_id, kind, &video),
                "video attachment",
            )
            .await?;
        }
    }

    Ok(AttachmentCollection {
        images,
        audios,
        documents,
        videos,
        had_attachments,
    })
}

/// Telegram video media on one message, labelled by kind, including a file
/// sent with a `video/*` MIME type. A GIF-style `animation` also sets
/// `document` for backward compatibility, so that document is not counted.
fn video_attachments(message: &Message) -> Vec<(&'static str, VideoRef<'_>)> {
    let video_document = message.document.as_ref().filter(|document| {
        message.animation.is_none()
            && document
                .mime_type
                .as_deref()
                .is_some_and(|mime| mime.starts_with("video/"))
    });
    [
        ("Video", message.video.as_ref().map(VideoRef::from)),
        (
            "Video note",
            message.video_note.as_ref().map(VideoRef::from),
        ),
        ("Animation", message.animation.as_ref().map(VideoRef::from)),
        ("Video file", video_document.map(VideoRef::from)),
    ]
    .into_iter()
    .filter_map(|(kind, video)| video.map(|video| (kind, video)))
    .collect()
}

async fn push_attachment<T>(
    output: &mut Vec<T>,
    load_future: impl std::future::Future<Output = Result<Option<T>>>,
    label: &str,
) -> Result<()> {
    match load_future.await {
        Ok(Some(item)) => output.push(item),
        Ok(None) => {}
        Err(err) if transcribe::is_operation_cancelled(&err) => return Err(err),
        Err(err) => tracing::error!(err = %format!("{err:#}"), label, "Failed to load attachment"),
    }
    Ok(())
}

async fn handle_empty_message(
    client: &TelegramClient,
    target: &MessageTarget,
    had_attachments: bool,
) -> Result<Option<IncomingMessage>> {
    if had_attachments {
        tracing::warn!(chat_id = target.chat, "Unsupported attachment");
        let _ = client
            .send_message(
                target.chat,
                "Sorry, I couldn't read that attachment.",
                Some(target.message),
                target.thread,
            )
            .await;
    } else {
        tracing::debug!(chat_id = target.chat, "Ignoring empty message");
    }
    Ok(None)
}

fn extract_text(message: &Message) -> Option<String> {
    let mut parts = Vec::new();
    for current in std::iter::once(message).chain(message.grouped_messages.iter()) {
        if let Some(text) = current.plain_text() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
        if let Some(caption) = current.caption.as_deref() {
            let trimmed = caption.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn select_best_photo(photos: &[PhotoSize]) -> Option<&PhotoSize> {
    photos.iter().max_by_key(|photo| {
        let size = photo.file_size.unwrap_or(0);
        let width = u64::try_from(photo.width.max(0)).unwrap_or(0);
        let height = u64::try_from(photo.height.max(0)).unwrap_or(0);
        let area = width * height;
        (size, area)
    })
}

async fn load_photo_attachment(
    client: &TelegramClient,
    chat_id: i64,
    message_id: i64,
    photo: &PhotoSize,
) -> Result<Option<IncomingImage>> {
    if photo.file_size.unwrap_or(0) > MAX_IMAGE_BYTES {
        tracing::debug!(chat_id, "Skipping photo exceeding max image size");
        return Ok(None);
    }

    let (file_path, bytes) = download_file_bytes(client, &photo.file_id).await?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        tracing::debug!(chat_id, "Downloaded photo exceeds max image size");
        return Ok(None);
    }

    let Some(mime_type) = detect_image_mime(&bytes) else {
        tracing::debug!(chat_id, "Unsupported image type");
        return Ok(None);
    };

    let filename =
        file_name_from_path(&file_path).unwrap_or_else(|| format!("photo_{message_id}.bin"));
    let local_path = save_media_bytes(chat_id, message_id, &filename, &bytes)?;
    // The saved file keeps full resolution; only the provider-bound copy is
    // clamped, since vision APIs reject images over their pixel cap.
    let (bytes, mime_type) = match zdx_engine::images::downscale_for_provider(&bytes) {
        Some((resized, resized_mime)) => (resized, resized_mime.to_string()),
        None => (bytes, mime_type),
    };
    let data = BASE64.encode(&bytes);

    Ok(Some(IncomingImage {
        local_path,
        mime_type,
        data,
    }))
}

async fn load_document_image(
    client: &TelegramClient,
    chat_id: i64,
    message_id: i64,
    document: &Document,
) -> Result<Option<IncomingImage>> {
    if document.file_size.unwrap_or(0) > MAX_IMAGE_BYTES {
        tracing::debug!(chat_id, "Skipping document image exceeding max size");
        return Ok(None);
    }

    let (file_path, bytes) = download_file_bytes(client, &document.file_id).await?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        tracing::debug!(chat_id, "Downloaded document image exceeds max size");
        return Ok(None);
    }

    let Some(mime_type) = select_image_mime(document.mime_type.as_deref(), &bytes) else {
        tracing::debug!(chat_id, "Unsupported document image type");
        return Ok(None);
    };

    let filename = document
        .file_name
        .as_deref()
        .and_then(file_name_from_path)
        .or_else(|| file_name_from_path(&file_path))
        .unwrap_or_else(|| format!("image_{message_id}.bin"));
    let local_path = save_media_bytes(chat_id, message_id, &filename, &bytes)?;
    // The saved file keeps full resolution; only the provider-bound copy is
    // clamped, since vision APIs reject images over their pixel cap.
    let (bytes, mime_type) = match zdx_engine::images::downscale_for_provider(&bytes) {
        Some((resized, resized_mime)) => (resized, resized_mime.to_string()),
        None => (bytes, mime_type),
    };
    let data = BASE64.encode(&bytes);

    Ok(Some(IncomingImage {
        local_path,
        mime_type,
        data,
    }))
}

async fn load_generic_document(
    client: &TelegramClient,
    chat_id: i64,
    message_id: i64,
    document: &Document,
) -> Result<Option<IncomingDocument>> {
    if document.file_size.unwrap_or(0) > MAX_DOCUMENT_BYTES {
        tracing::debug!(chat_id, "Skipping document exceeding max size");
        return Ok(None);
    }

    let (file_path, bytes) = download_file_bytes(client, &document.file_id).await?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        tracing::debug!(chat_id, "Downloaded document exceeds max size");
        return Ok(None);
    }

    let file_name = document
        .file_name
        .as_deref()
        .and_then(file_name_from_path)
        .or_else(|| file_name_from_path(&file_path))
        .unwrap_or_else(|| format!("document_{message_id}.bin"));
    let local_path = save_media_bytes(chat_id, message_id, &file_name, &bytes)?;

    Ok(Some(IncomingDocument {
        local_path,
        file_name,
    }))
}

/// Downloads a video for `ask_media`. The limit is `ask_media`'s inline cap, so
/// a saved video is always one the agent can inspect; a larger one is reported
/// to the agent instead of dropped.
async fn load_video(
    client: &TelegramClient,
    chat_id: i64,
    message_id: i64,
    kind: &'static str,
    video: &VideoRef<'_>,
) -> Result<Option<IncomingVideo>> {
    let too_large = |size_bytes| {
        tracing::debug!(
            chat_id,
            size_bytes,
            "Skipping video exceeding ask_media limit"
        );
        Ok(Some(IncomingVideo {
            kind,
            status: VideoStatus::TooLarge { size_bytes },
        }))
    };
    if let Some(size) = video.size.filter(|size| *size > MAX_VIDEO_BYTES) {
        return too_large(size);
    }

    let (file_path, bytes) = download_file_bytes(client, video.id).await?;
    if bytes.len() as u64 > MAX_VIDEO_BYTES {
        return too_large(bytes.len() as u64);
    }

    let file_name = video
        .name
        .and_then(file_name_from_path)
        .or_else(|| file_name_from_path(&file_path))
        .unwrap_or_else(|| format!("video_{message_id}.mp4"));
    let local_path = save_media_bytes(chat_id, message_id, &file_name, &bytes)?;

    Ok(Some(IncomingVideo {
        kind,
        status: VideoStatus::Saved(local_path),
    }))
}

async fn load_voice_attachment(
    client: &TelegramClient,
    config: &Config,
    chat_id: i64,
    message_id: i64,
    voice: &Voice,
    cancel_token: Option<&CancellationToken>,
) -> Result<Option<IncomingAudio>> {
    load_audio_by_id(
        client,
        config,
        chat_id,
        message_id,
        AudioSource {
            file_id: &voice.file_id,
            file_size: voice.file_size,
            mime_type: voice.mime_type.as_deref(),
            file_name_hint: None,
        },
        cancel_token,
    )
    .await
}

async fn load_audio_message(
    client: &TelegramClient,
    config: &Config,
    chat_id: i64,
    message_id: i64,
    audio: &Audio,
    cancel_token: Option<&CancellationToken>,
) -> Result<Option<IncomingAudio>> {
    load_audio_by_id(
        client,
        config,
        chat_id,
        message_id,
        AudioSource {
            file_id: &audio.file_id,
            file_size: audio.file_size,
            mime_type: audio.mime_type.as_deref(),
            file_name_hint: audio.file_name.as_deref(),
        },
        cancel_token,
    )
    .await
}

async fn load_audio_attachment(
    client: &TelegramClient,
    config: &Config,
    chat_id: i64,
    message_id: i64,
    document: &Document,
    cancel_token: Option<&CancellationToken>,
) -> Result<Option<IncomingAudio>> {
    load_audio_by_id(
        client,
        config,
        chat_id,
        message_id,
        AudioSource {
            file_id: &document.file_id,
            file_size: document.file_size,
            mime_type: document.mime_type.as_deref(),
            file_name_hint: document.file_name.as_deref(),
        },
        cancel_token,
    )
    .await
}

async fn load_audio_by_id(
    client: &TelegramClient,
    config: &Config,
    chat_id: i64,
    message_id: i64,
    source: AudioSource<'_>,
    cancel_token: Option<&CancellationToken>,
) -> Result<Option<IncomingAudio>> {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return Err(transcribe::OperationCancelled.into());
    }

    if source.file_size.unwrap_or(0) > MAX_AUDIO_BYTES {
        tracing::debug!(chat_id, "Skipping audio exceeding max size");
        return Ok(None);
    }

    let (file_path, bytes) = download_file_bytes(client, source.file_id).await?;
    if bytes.len() as u64 > MAX_AUDIO_BYTES {
        tracing::debug!(chat_id, "Downloaded audio exceeds max size");
        return Ok(None);
    }

    let filename = source
        .file_name_hint
        .and_then(file_name_from_path)
        .or_else(|| file_name_from_path(&file_path))
        .unwrap_or_else(|| format!("audio_{message_id}.bin"));
    let local_path = save_media_bytes(chat_id, message_id, &filename, &bytes)?;

    let transcript = match transcribe::transcribe_audio_if_configured(
        config,
        bytes,
        &filename,
        source.mime_type,
        cancel_token,
    )
    .await
    {
        Ok(transcript) => transcript,
        Err(err) if transcribe::is_operation_cancelled(&err) => return Err(err),
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "Audio transcription failed");
            None
        }
    };

    Ok(Some(IncomingAudio {
        local_path,
        transcript,
    }))
}

fn detect_image_mime(bytes: &[u8]) -> Option<String> {
    let kind = infer::get(bytes)?;
    let mime = kind.mime_type();
    if SUPPORTED_IMAGE_MIMES.contains(&mime) {
        Some(mime.to_string())
    } else {
        None
    }
}

fn select_image_mime(declared: Option<&str>, bytes: &[u8]) -> Option<String> {
    if let Some(mime) = declared
        && SUPPORTED_IMAGE_MIMES.contains(&mime)
    {
        return Some(mime.to_string());
    }
    detect_image_mime(bytes)
}

async fn download_file_bytes(client: &TelegramClient, file_id: &str) -> Result<(String, Vec<u8>)> {
    let file = client.get_file(file_id).await?;
    let file_path = file
        .file_path
        .ok_or_else(|| anyhow!("Telegram file missing file_path"))?;
    let bytes = client.download_file(&file_path).await?;
    Ok((file_path, bytes))
}

fn file_name_from_path(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(std::string::ToString::to_string)
}

fn save_media_bytes(
    chat_id: i64,
    message_id: i64,
    filename: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    let dir = paths::zdx_home().join("telegram").join(chat_id.to_string());
    fs::create_dir_all(&dir).context("create media directory")?;
    let safe_name = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(filename);
    let path = dir.join(format!("{message_id}_{safe_name}"));
    fs::write(&path, bytes).context("write media file")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::json;
    use zdx_engine::config::Config;

    use super::{AllowlistConfig, MAX_VIDEO_BYTES, extract_text, parse_incoming_message};
    use crate::agent::build_user_text;
    use crate::telegram::{Message, TelegramClient};
    use crate::types::{IncomingMessage, VideoStatus};

    fn message(value: serde_json::Value) -> Message {
        serde_json::from_value(value).expect("valid message")
    }

    fn base(id: i64) -> serde_json::Value {
        json!({
            "message_id": id,
            "chat": { "id": 42, "type": "supergroup", "is_forum": true },
            "from": { "id": 7, "is_bot": false },
        })
    }

    fn with(id: i64, fields: &serde_json::Value) -> Message {
        let mut value = base(id);
        let object = value.as_object_mut().expect("object");
        for (key, field) in fields.as_object().expect("object") {
            object.insert(key.clone(), field.clone());
        }
        message(value)
    }

    /// Shape Telegram delivers when the sender pastes a formatted list: no
    /// `text` at all, everything under `rich_message`.
    #[test]
    fn reads_the_text_of_a_rich_message() {
        let message = with(
            1,
            &json!({
                "rich_message": { "blocks": [
                    { "type": "paragraph", "text": "August:" },
                    { "type": "list", "items": [
                        { "label": "•", "blocks": [{ "type": "paragraph", "text": "rent: 460.00" }] },
                        { "label": "•", "blocks": [{ "type": "paragraph", "text": "lessons: 296.00" }] },
                    ]},
                ]},
            }),
        );

        assert_eq!(
            extract_text(&message).as_deref(),
            Some("August:\n\n• rent: 460.00\n• lessons: 296.00")
        );
    }

    #[test]
    fn plain_text_and_captions_are_unchanged() {
        let mut album = with(2, &json!({ "caption": "first photo" }));
        album
            .grouped_messages
            .push(with(3, &json!({ "text": "note" })));

        assert_eq!(extract_text(&album).as_deref(), Some("first photo\n\nnote"));
    }

    #[test]
    fn a_message_without_any_text_stays_empty() {
        assert!(extract_text(&with(4, &json!({ "rich_message": { "blocks": [] } }))).is_none());
    }

    const TOKEN: &str = "test-token";
    const VIDEO_BYTES: &[u8] = b"synthetic-mp4-bytes";

    /// Fake Bot API: `getFile` maps a file id to `videos/<id>.mp4`, and the file
    /// endpoint serves fixed bytes. Returns the base URL and the `getFile` count.
    async fn fake_telegram() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let get_file_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&get_file_calls);
        let app = Router::new()
            .route(
                &format!("/bot{TOKEN}/getFile"),
                post(move |Json(body): Json<serde_json::Value>| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let file_id = body["file_id"].as_str().unwrap_or_default().to_string();
                        Json(json!({
                            "ok": true,
                            "result": { "file_id": file_id, "file_path": format!("videos/{file_id}.mp4") },
                        }))
                    }
                }),
            )
            .route(
                &format!("/file/bot{TOKEN}/videos/{{name}}"),
                get(|| async { VIDEO_BYTES }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Telegram");
        let addr = listener.local_addr().expect("fake Telegram address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve fake Telegram");
        });
        (format!("http://{addr}"), get_file_calls, server)
    }

    async fn ingest(base_url: String, message: Message) -> Option<IncomingMessage> {
        let client = TelegramClient::with_base_url(TOKEN.to_string(), base_url);
        let user_ids = HashSet::from([7]);
        let chat_ids = HashSet::from([42]);
        parse_incoming_message(
            &client,
            AllowlistConfig {
                user_ids: &user_ids,
                chat_ids: &chat_ids,
            },
            &Config::default(),
            message,
            None,
        )
        .await
        .expect("ingest message")
    }

    fn saved_video_path(incoming: &IncomingMessage) -> PathBuf {
        assert_eq!(incoming.videos.len(), 1, "exactly one video attachment");
        let VideoStatus::Saved(path) = &incoming.videos[0].status else {
            panic!("video was not saved");
        };
        path.clone()
    }

    /// The reported regression: a captioned video used to reach the agent as
    /// its caption alone.
    #[tokio::test]
    async fn captioned_video_reaches_the_agent_as_a_saved_file() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let (base_url, _, server) = fake_telegram().await;
        let message = with(
            10,
            &json!({
                "caption": "check the keyboard animation",
                "video": { "file_id": "vid1", "file_size": 19, "mime_type": "video/mp4" },
            }),
        );

        let incoming = ingest(base_url, message).await.expect("turn runs");
        let path = saved_video_path(&incoming);
        assert_eq!(fs::read(&path).expect("saved video"), VIDEO_BYTES);
        assert!(path.to_string_lossy().ends_with("10_vid1.mp4"));

        let prompt = build_user_text(&incoming);
        assert!(prompt.starts_with("check the keyboard animation"));
        assert!(prompt.contains(&format!(
            "Video attachment saved at {} — use the `ask_media` tool on it",
            path.display()
        )));
        server.abort();
    }

    /// A video with no caption used to be dropped as an empty message.
    #[tokio::test]
    async fn video_note_without_caption_runs_a_turn() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let (base_url, _, server) = fake_telegram().await;
        let message = with(11, &json!({ "video_note": { "file_id": "note1" } }));

        let incoming = ingest(base_url, message).await.expect("turn runs");
        assert!(incoming.text.is_none());
        let path = saved_video_path(&incoming);
        assert!(build_user_text(&incoming).starts_with(&format!(
            "Video note attachment saved at {}",
            path.display()
        )));
        server.abort();
    }

    /// Telegram also sets `document` on an animation; it must load once.
    #[tokio::test]
    async fn animation_is_loaded_once_despite_its_document_twin() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let (base_url, get_file_calls, server) = fake_telegram().await;
        let message = with(
            12,
            &json!({
                "animation": { "file_id": "gif1", "file_name": "loop.mp4", "mime_type": "video/mp4" },
                "document": { "file_id": "gif1", "file_name": "loop.mp4", "mime_type": "video/mp4" },
            }),
        );

        let incoming = ingest(base_url, message).await.expect("turn runs");
        assert!(incoming.documents.is_empty());
        assert_eq!(incoming.videos[0].kind, "Animation");
        assert!(saved_video_path(&incoming).ends_with("12_loop.mp4"));
        assert_eq!(get_file_calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn video_sent_as_a_file_uses_the_video_path() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let (base_url, _, server) = fake_telegram().await;
        let message = with(
            13,
            &json!({
                "document": { "file_id": "doc1", "file_name": "screen.mov", "mime_type": "video/quicktime" },
            }),
        );

        let incoming = ingest(base_url, message).await.expect("turn runs");
        assert!(incoming.documents.is_empty());
        assert_eq!(incoming.videos[0].kind, "Video file");
        assert!(saved_video_path(&incoming).ends_with("13_screen.mov"));
        server.abort();
    }

    /// A video over the `ask_media` limit is not downloaded, but the agent is
    /// told why instead of the video vanishing.
    #[tokio::test]
    async fn oversized_video_is_reported_without_download() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let (base_url, get_file_calls, server) = fake_telegram().await;
        let message = with(
            14,
            &json!({ "video": { "file_id": "big1", "file_size": MAX_VIDEO_BYTES + 1 } }),
        );

        let incoming = ingest(base_url, message).await.expect("turn runs");
        assert!(matches!(
            incoming.videos[0].status,
            VideoStatus::TooLarge { .. }
        ));
        assert_eq!(get_file_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            build_user_text(&incoming),
            "Video attachment was not downloaded: it is 15.0 MiB and the limit is 15 MiB. \
             Ask the user for a shorter or compressed clip."
        );
        server.abort();
    }
}
