//! Privacy-safe intake metadata for incoming Telegram messages.
//!
//! Each allowlisted message gets one log line, stamped on receipt before
//! grouping, queueing, or downloads. With the media-group flush line and the
//! queue lines, it shows how Telegram delivered a request (separate messages,
//! one album, a late album member) and how ZDX turned it into turns.
//!
//! Only identifiers, timestamps, fixed-vocabulary kind labels, presence flags,
//! and counts are recorded: never text, captions, file names, file ids, MIME
//! strings, or nested payload bodies. Member counts are what this process
//! observed; Telegram does not announce an album's size.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::bot::media_group::Flushed;
use crate::telegram::Message;

/// Message ids listed per log line; the count field still has the full total.
const MAX_LOGGED_IDS: usize = 10;

/// Longest all-digit `media_group_id` logged verbatim.
const MAX_GROUP_ID_LEN: usize = 32;

/// Hex characters kept from a fingerprint (64 bits).
const FINGERPRINT_HEX_LEN: usize = 16;

/// Logging authorization: an allowlisted human sender, and for groups an
/// allowlisted chat. This is `queue::should_process_message` without its
/// `/whereami` exception (which serves unbound groups), so intake metadata is
/// only ever logged for messages ZDX runs turns for. Keep the two aligned.
pub(crate) fn is_allowlisted(
    users: &HashSet<i64>,
    chats: &HashSet<i64>,
    message: &Message,
) -> bool {
    let Some(user) = message.from.as_ref() else {
        return false;
    };
    !user.is_bot
        && users.contains(&user.id)
        && (!message.chat.is_group() || chats.contains(&message.chat.id))
}

/// Where a message sat in one `getUpdates` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PollPosition {
    pub index: usize,
    pub size: usize,
}

/// What the media-group buffer did with a received message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grouping {
    /// No `media_group_id`: dispatched on its own right away.
    None,
    /// First member seen for its group; held until the group flushes.
    /// `late_after_flush_ms` is set when a group with the same key flushed
    /// recently, so this member missed that batch.
    Opened { late_after_flush_ms: Option<u64> },
    /// Joined a group that is still held; `observed` counts members so far.
    Joined { observed: usize },
}

impl Grouping {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Opened { .. } => "opened",
            Self::Joined { .. } => "joined",
        }
    }
}

/// Logged facts about one received message. Every field is an id, a time, a
/// fixed label, a flag, or a count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntakeMeta {
    pub update_id: i64,
    pub message_id: i64,
    pub chat_id: i64,
    pub topic_id: Option<i64>,
    pub sender_id: Option<i64>,
    pub reply_to: Option<i64>,
    /// Telegram's message time, unix seconds.
    pub tg_date: i64,
    /// Local receipt time, unix milliseconds.
    pub received_ms: i64,
    pub poll: PollPosition,
    /// Telegram's `media_group_id` (an opaque string) as `safe_group_id`
    /// renders it; `None` when absent.
    pub media_group: Option<String>,
    /// Comma-separated media kinds, from a fixed vocabulary.
    pub kinds: String,
    /// Sizes offered for the message's one photo (all the same image).
    pub photo_sizes: usize,
    /// MIME class of `document`, when the message has one.
    pub document_class: Option<&'static str>,
    /// The `document` is Telegram's compatibility copy of an `animation`.
    pub animation_document_twin: bool,
    pub has_text: bool,
    pub has_caption: bool,
    /// Rich message: (top-level blocks, top-level media blocks).
    pub rich_blocks: Option<(usize, usize)>,
    pub paid_media_items: Option<usize>,
}

impl IntakeMeta {
    pub(crate) fn of(
        update_id: i64,
        message: &Message,
        received_ms: i64,
        poll: PollPosition,
    ) -> Self {
        let mut kinds = Vec::new();
        let flags = [
            ("photo", message.photo.is_some()),
            ("video", message.video.is_some()),
            ("video_note", message.video_note.is_some()),
            ("animation", message.animation.is_some()),
            ("voice", message.voice.is_some()),
            ("audio", message.audio.is_some()),
            (
                "document",
                message.document.is_some() && message.animation.is_none(),
            ),
            ("paid_media", message.paid_media.is_some()),
            ("rich", message.rich_message.is_some()),
        ];
        for (kind, present) in flags {
            if present {
                kinds.push(kind);
            }
        }
        Self {
            update_id,
            message_id: message.id,
            chat_id: message.chat.id,
            topic_id: message.effective_thread_id(),
            sender_id: message.from.as_ref().map(|user| user.id),
            reply_to: message.reply_to.as_ref().map(|reply| reply.id),
            tg_date: message.date,
            received_ms,
            poll,
            media_group: message.media_group_id.as_deref().map(safe_group_id),
            kinds: kinds.join(","),
            photo_sizes: message.photo.as_ref().map_or(0, Vec::len),
            document_class: message
                .document
                .as_ref()
                .map(|document| document_class(document.mime_type.as_deref())),
            animation_document_twin: message.animation.is_some() && message.document.is_some(),
            has_text: has_content(message.text.as_deref()),
            has_caption: has_content(message.caption.as_deref()),
            rich_blocks: message
                .rich_message
                .as_ref()
                .map(crate::telegram::RichMessage::block_counts),
            paid_media_items: message
                .paid_media
                .as_ref()
                .map(|paid| paid.paid_media.len()),
        }
    }

    /// Local wall clock at receipt minus Telegram's message time. Telegram's
    /// time has one-second resolution and comes from Telegram's clock, so this
    /// is a coarse difference between two clocks, not a guaranteed bound on
    /// delivery lag; it can be off by clock skew or even negative.
    fn receipt_lag_ms(&self) -> Option<i64> {
        (self.tg_date > 0).then(|| self.received_ms - self.tg_date * 1000)
    }
}

/// Writes the receipt line. Call before the message is grouped or queued.
pub(crate) fn log_received(meta: &IntakeMeta, grouping: Grouping) {
    let (observed_members, late_after_flush_ms) = match grouping {
        Grouping::None => (None, None),
        Grouping::Opened {
            late_after_flush_ms,
        } => (Some(1), late_after_flush_ms),
        Grouping::Joined { observed } => (Some(observed), None),
    };
    tracing::info!(
        update_id = meta.update_id,
        message_id = meta.message_id,
        chat_id = meta.chat_id,
        topic_id = ?meta.topic_id,
        sender_id = ?meta.sender_id,
        reply_to = ?meta.reply_to,
        tg_date = meta.tg_date,
        received_ms = meta.received_ms,
        receipt_lag_ms = ?meta.receipt_lag_ms(),
        poll_index = meta.poll.index,
        poll_size = meta.poll.size,
        media_group = ?meta.media_group,
        kinds = %meta.kinds,
        photo_sizes = meta.photo_sizes,
        document_class = ?meta.document_class,
        animation_document_twin = meta.animation_document_twin,
        has_text = meta.has_text,
        has_caption = meta.has_caption,
        rich_blocks = ?meta.rich_blocks,
        paid_media_items = ?meta.paid_media_items,
        grouping = grouping.label(),
        observed_members = ?observed_members,
        late_after_flush_ms = ?late_after_flush_ms,
        "Telegram message received"
    );
}

/// Writes the flush line for a media group taken out of the buffer.
pub(crate) fn log_flushed(flushed: &Flushed) {
    tracing::info!(
        chat_id = flushed.message.chat.id,
        topic_id = ?flushed.message.effective_thread_id(),
        message_id = flushed.message.id,
        media_group = ?flushed.message.media_group_id.as_deref().map(safe_group_id),
        member_ids = %member_ids(&flushed.message),
        observed_members = flushed.observed,
        held_ms = flushed.held.as_millis() as u64,
        arrival_span_ms = flushed.span.as_millis() as u64,
        "Media group flushed"
    );
}

/// Current time as unix milliseconds.
pub(crate) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// The message's own id followed by its grouped members' ids, at most
/// `MAX_LOGGED_IDS` of them, e.g. `"24483,24484"` or `"1,…,10,+3"`.
pub(crate) fn member_ids(message: &Message) -> String {
    let total = message.grouped_messages.len() + 1;
    let mut listed: Vec<String> = std::iter::once(message.id)
        .chain(message.grouped_messages.iter().map(|member| member.id))
        .take(MAX_LOGGED_IDS)
        .map(|id| id.to_string())
        .collect();
    if total > MAX_LOGGED_IDS {
        listed.push(format!("+{}", total - MAX_LOGGED_IDS));
    }
    listed.join(",")
}

fn has_content(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

/// A short all-digit id (what Telegram sends in practice) verbatim; any other
/// value as `h:` plus a truncated SHA-256, which is stable for correlating
/// album members but never echoes the raw string. Grouping keys on the raw id.
fn safe_group_id(raw: &str) -> String {
    if !raw.is_empty() && raw.len() <= MAX_GROUP_ID_LEN && raw.bytes().all(|b| b.is_ascii_digit()) {
        return raw.to_string();
    }
    let digest = Sha256::digest(raw.as_bytes());
    let mut fingerprint = String::from("h:");
    for byte in digest.iter().take(FINGERPRINT_HEX_LEN / 2) {
        let _ = write!(fingerprint, "{byte:02x}");
    }
    fingerprint
}

fn document_class(mime: Option<&str>) -> &'static str {
    match mime.and_then(|mime| mime.split('/').next()) {
        Some("image") => "image",
        Some("video") => "video",
        Some("audio") => "audio",
        Some(_) => "other",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const POLL: PollPosition = PollPosition { index: 0, size: 1 };

    fn message(fields: &Value) -> Message {
        let mut value = json!({
            "message_id": 50,
            "date": 1_700_000_000,
            "chat": { "id": -100, "type": "supergroup", "is_forum": true },
            "from": { "id": 7, "is_bot": false, "username": "private_handle" },
            "message_thread_id": 3,
        });
        for (key, field) in fields.as_object().expect("object") {
            value[key] = field.clone();
        }
        serde_json::from_value(value).expect("valid test message")
    }

    fn meta(fields: &Value) -> IntakeMeta {
        IntakeMeta::of(9, &message(fields), 1_700_000_000_400, POLL)
    }

    #[test]
    fn plain_text_message() {
        let meta = meta(&json!({ "text": "hello" }));
        assert!(meta.has_text);
        assert!(!meta.has_caption);
        assert_eq!(meta.kinds, "");
        assert_eq!(meta.media_group, None);
        assert_eq!(meta.receipt_lag_ms(), Some(400));
        assert_eq!(
            (
                meta.update_id,
                meta.message_id,
                meta.chat_id,
                meta.topic_id,
                meta.sender_id
            ),
            (9, 50, -100, Some(3), Some(7))
        );
    }

    #[test]
    fn captioned_photo_counts_sizes_of_one_photo() {
        let meta = meta(&json!({
            "caption": "look",
            "media_group_id": "1234567890",
            "photo": [
                { "file_id": "small", "width": 90, "height": 90 },
                { "file_id": "big", "width": 1280, "height": 1280 },
            ],
        }));
        assert_eq!(meta.kinds, "photo");
        assert_eq!(meta.photo_sizes, 2);
        assert!(meta.has_caption);
        assert!(!meta.has_text);
        assert_eq!(meta.media_group.as_deref(), Some("1234567890"));
    }

    #[test]
    fn animation_document_twin_is_one_animation() {
        let meta = meta(&json!({
            "animation": { "file_id": "anim", "file_name": "clip.mp4", "mime_type": "video/mp4" },
            "document": { "file_id": "anim", "file_name": "clip.mp4", "mime_type": "video/mp4" },
        }));
        assert_eq!(meta.kinds, "animation");
        assert!(meta.animation_document_twin);
        assert_eq!(meta.document_class, Some("video"));
    }

    #[test]
    fn document_reports_mime_class_only() {
        let meta = meta(&json!({
            "document": { "file_id": "doc", "mime_type": "application/x-secret-type" },
        }));
        assert_eq!(meta.kinds, "document");
        assert_eq!(meta.document_class, Some("other"));
        assert!(!meta.animation_document_twin);
    }

    #[test]
    fn rich_and_paid_payloads_are_counted() {
        let meta = meta(&json!({
            "rich_message": { "blocks": [
                { "type": "paragraph", "text": "words" },
                { "type": "photo", "caption": "inside" },
                { "type": "collage", "blocks": [] },
            ]},
            "paid_media": { "star_count": 5, "paid_media": [
                { "type": "photo", "photo": [] },
                { "type": "preview" },
            ]},
        }));
        assert_eq!(meta.kinds, "paid_media,rich");
        assert_eq!(meta.rich_blocks, Some((3, 2)));
        assert_eq!(meta.paid_media_items, Some(2));
    }

    #[test]
    fn logged_fields_carry_no_content() {
        let message = message(&json!({
            "text": "SECRET_TEXT",
            "caption": "SECRET_CAPTION",
            "media_group_id": "SECRET_GROUP\"\n",
            "video": { "file_id": "SECRET_FILE_ID", "file_name": "SECRET_NAME.mp4", "mime_type": "video/SECRET_MIME" },
            "document": { "file_id": "SECRET_DOC_ID", "file_name": "SECRET_DOC.pdf", "mime_type": "application/SECRET" },
            "rich_message": { "blocks": [{ "type": "SECRET_BLOCK", "text": "SECRET_RICH" }] },
            "paid_media": { "paid_media": [{ "type": "SECRET_PAID" }] },
            "reply_to_message": {
                "message_id": 49,
                "chat": { "id": -100, "type": "supergroup" },
                "text": "SECRET_REPLY",
            },
        }));
        let rendered = format!("{:?}", IntakeMeta::of(9, &message, 0, POLL));
        assert!(!rendered.contains("SECRET"), "{rendered}");
        assert!(!rendered.contains("private_handle"), "{rendered}");
        assert!(rendered.contains("h:"));
    }

    #[test]
    fn member_ids_are_bounded() {
        let mut head = message(&json!({ "message_id": 1 }));
        for id in 2..=13 {
            head.grouped_messages
                .push(message(&json!({ "message_id": id })));
        }
        assert_eq!(member_ids(&head), "1,2,3,4,5,6,7,8,9,10,+3");
        head.grouped_messages.truncate(1);
        assert_eq!(member_ids(&head), "1,2");
    }

    #[test]
    fn opaque_group_ids_get_a_stable_fingerprint() {
        let a = safe_group_id("album/one");
        assert_eq!(a, safe_group_id("album/one"));
        assert_ne!(a, safe_group_id("album/two"));
        assert!(a.starts_with("h:"));
        assert_eq!(a.len(), 2 + FINGERPRINT_HEX_LEN);
        assert!(!a.contains("album"));
        assert_eq!(safe_group_id("13897512736"), "13897512736");
        let long_digits = "1".repeat(MAX_GROUP_ID_LEN + 1);
        assert!(safe_group_id(&long_digits).starts_with("h:"));
        assert!(safe_group_id("").starts_with("h:"));
    }

    #[test]
    fn whereami_in_unbound_group_is_not_logged() {
        let users = HashSet::from([7]);
        let whereami = message(&json!({ "text": "/whereami" }));
        assert!(!is_allowlisted(&users, &HashSet::new(), &whereami));
    }

    /// Captures what the receipt and flush lines actually emit.
    #[test]
    fn emitted_log_lines_carry_no_content() {
        use std::io;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("capture lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let secret_fields = json!({
            "text": "SECRET_TEXT https://api.telegram.org/botSECRET_TOKEN/getFile",
            "caption": "SECRET_CAPTION",
            "media_group_id": "SECRET_GROUP",
            "video": { "file_id": "SECRET_FILE_ID", "file_name": "SECRET_NAME.mp4", "mime_type": "video/mp4" },
            "photo": [{ "file_id": "SECRET_PHOTO_ID", "file_unique_id": "SECRET_UNIQUE", "width": 1, "height": 1 }],
        });
        let mut head = message(&secret_fields);
        head.grouped_messages.push(message(
            &json!({ "message_id": 51, "caption": "SECRET_MEMBER" }),
        ));
        let meta = IntakeMeta::of(9, &head, 1_700_000_000_400, POLL);
        let flushed = Flushed {
            message: head,
            observed: 2,
            held: Duration::from_millis(1500),
            span: Duration::from_millis(200),
        };

        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_received(&meta, Grouping::Joined { observed: 2 });
            log_flushed(&flushed);
        });

        let output = String::from_utf8(capture.0.lock().expect("capture lock").clone())
            .expect("utf8 log output");
        assert!(output.contains("Telegram message received"), "{output}");
        assert!(output.contains("Media group flushed"), "{output}");
        assert!(output.contains("member_ids=50,51"), "{output}");
        for leaked in [
            "SECRET",
            "private_handle",
            "https://",
            "api.telegram.org",
            "mp4",
        ] {
            assert!(!output.contains(leaked), "{leaked} leaked: {output}");
        }
    }

    #[test]
    fn allowlist_gate() {
        let users = HashSet::from([7]);
        let chats = HashSet::from([-100]);
        let allowed = message(&json!({ "text": "x" }));
        assert!(is_allowlisted(&users, &chats, &allowed));
        assert!(!is_allowlisted(&users, &HashSet::new(), &allowed));
        assert!(!is_allowlisted(&HashSet::from([8]), &chats, &allowed));
        let bot = message(&json!({ "from": { "id": 7, "is_bot": true } }));
        assert!(!is_allowlisted(&users, &chats, &bot));
    }
}
