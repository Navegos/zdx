use std::path::PathBuf;

pub struct IncomingMessage {
    pub chat_id: i64,
    pub message_id: i64,
    pub user_id: i64,
    pub text: Option<String>,
    pub images: Vec<IncomingImage>,
    pub audios: Vec<IncomingAudio>,
    pub documents: Vec<IncomingDocument>,
    pub videos: Vec<IncomingVideo>,
    /// Attachments that could not be loaded, as prompt sentences, so the agent
    /// is told instead of the attachment silently vanishing.
    pub notices: Vec<String>,
    /// Forum topic ID (for supergroups with topics enabled).
    pub message_thread_id: Option<i64>,
    /// Whether the group is a forum-enabled supergroup.
    pub is_forum: bool,
}

pub struct IncomingImage {
    /// The original file as received, kept at full resolution.
    pub local_path: PathBuf,
    pub delivery: ImageDelivery,
}

pub enum ImageDelivery {
    /// Sent to the model as an image block.
    Inline {
        mime_type: String,
        data: String,
        /// The model gets a smaller copy than the saved original.
        downscaled: bool,
    },
    /// Saved only; `reason` says why the model cannot see it inline.
    SavedOnly {
        reason: String,
        /// The saved file fits `ask_media`'s type and size limits.
        ask_media: bool,
    },
}

impl IncomingImage {
    pub fn inline(&self) -> Option<(&str, &str)> {
        match &self.delivery {
            ImageDelivery::Inline {
                mime_type, data, ..
            } => Some((mime_type, data)),
            ImageDelivery::SavedOnly { .. } => None,
        }
    }
}

pub struct IncomingAudio {
    pub local_path: PathBuf,
    pub transcript: Option<String>,
}

pub struct IncomingDocument {
    pub local_path: PathBuf,
    pub file_name: String,
}

pub struct IncomingVideo {
    /// Human label for the Telegram media kind ("Video", "Video note", ...).
    pub kind: &'static str,
    pub status: VideoStatus,
}

pub enum VideoStatus {
    Saved(PathBuf),
    /// Not downloaded because it is over the media-understanding limit.
    TooLarge {
        size_bytes: u64,
    },
}
