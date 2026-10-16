//! Media-album grouping for incoming Telegram messages.
//!
//! Messages that share a `media_group_id` (same chat, topic, and sender) are
//! held for `MEDIA_GROUP_DEBOUNCE` after the first member arrives, then
//! dispatched as one message whose `grouped_messages` holds the rest. A member
//! that arrives after its group flushed opens a new group; the buffer remembers
//! recent flushes only so that case can be logged.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::intake_log::Grouping;
use crate::telegram::Message;

pub(crate) const MEDIA_GROUP_DEBOUNCE: Duration = Duration::from_millis(1500);

/// How long a flushed key is remembered for late-member detection.
const FLUSH_MEMORY: Duration = Duration::from_mins(2);

/// Most flushed keys remembered at once; the oldest is dropped past this.
const FLUSH_MEMORY_CAP: usize = 64;

/// (`chat_id`, topic, sender, `media_group_id`).
pub(crate) type MediaGroupKey = (i64, Option<i64>, i64, String);

pub(crate) fn media_group_key(message: &Message) -> Option<MediaGroupKey> {
    Some((
        message.chat.id,
        message.effective_thread_id(),
        message.from.as_ref()?.id,
        message.media_group_id.clone()?,
    ))
}

struct PendingGroup {
    message: Message,
    opened_at: Instant,
    last_member_at: Instant,
}

/// Result of offering a received message to the buffer.
pub(crate) enum Offer {
    /// Not in an album: dispatch it now.
    Ungrouped(Box<Message>),
    /// First member of a group: schedule `flush` for this key.
    Opened {
        key: MediaGroupKey,
        grouping: Grouping,
    },
    /// Added to a group that is still held.
    Joined { grouping: Grouping },
}

/// A group taken out of the buffer for dispatch.
pub(crate) struct Flushed {
    pub message: Message,
    /// Members this process saw, the head included.
    pub observed: usize,
    /// From the first member's arrival to the flush.
    pub held: Duration,
    /// From the first member's arrival to the last one's.
    pub span: Duration,
}

#[derive(Default)]
pub(crate) struct MediaGroups {
    pending: HashMap<MediaGroupKey, PendingGroup>,
    flushed: HashMap<MediaGroupKey, Instant>,
}

impl MediaGroups {
    pub(crate) fn offer(&mut self, message: Message, now: Instant) -> Offer {
        let Some(key) = media_group_key(&message) else {
            return Offer::Ungrouped(Box::new(message));
        };
        if let Some(group) = self.pending.get_mut(&key) {
            group.message.grouped_messages.push(message);
            group.last_member_at = now;
            return Offer::Joined {
                grouping: Grouping::Joined {
                    observed: group.message.grouped_messages.len() + 1,
                },
            };
        }
        let late_after_flush_ms = self
            .flushed
            .get(&key)
            .map(|flushed_at| now.saturating_duration_since(*flushed_at))
            .filter(|since| *since < FLUSH_MEMORY)
            .map(|since| since.as_millis() as u64);
        self.pending.insert(
            key.clone(),
            PendingGroup {
                message,
                opened_at: now,
                last_member_at: now,
            },
        );
        Offer::Opened {
            key,
            grouping: Grouping::Opened {
                late_after_flush_ms,
            },
        }
    }

    pub(crate) fn flush(&mut self, key: &MediaGroupKey, now: Instant) -> Option<Flushed> {
        let group = self.pending.remove(key)?;
        self.remember_flush(key, now);
        Some(Flushed {
            observed: group.message.grouped_messages.len() + 1,
            held: now.saturating_duration_since(group.opened_at),
            span: group
                .last_member_at
                .saturating_duration_since(group.opened_at),
            message: group.message,
        })
    }

    fn remember_flush(&mut self, key: &MediaGroupKey, now: Instant) {
        self.flushed
            .retain(|_, flushed_at| now.saturating_duration_since(*flushed_at) < FLUSH_MEMORY);
        if !self.flushed.contains_key(key) && self.flushed.len() >= FLUSH_MEMORY_CAP {
            let oldest = self
                .flushed
                .iter()
                .min_by_key(|(_, flushed_at)| **flushed_at)
                .map(|(oldest, _)| oldest.clone());
            if let Some(oldest) = oldest {
                self.flushed.remove(&oldest);
            }
        }
        self.flushed.insert(key.clone(), now);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message(id: i64, media_group_id: Option<&str>) -> Message {
        serde_json::from_value(json!({
            "message_id": id,
            "chat": { "id": 42, "type": "private", "is_forum": false },
            "from": { "id": 7, "is_bot": false },
            "media_group_id": media_group_id,
            "text": "x",
        }))
        .expect("valid test message")
    }

    fn ids(message: &Message) -> Vec<i64> {
        std::iter::once(message.id)
            .chain(message.grouped_messages.iter().map(|m| m.id))
            .collect()
    }

    #[test]
    fn derives_media_group_key_from_message() {
        assert_eq!(
            media_group_key(&message(100, Some("album-1"))),
            Some((42, None, 7, "album-1".to_string()))
        );
        assert_eq!(media_group_key(&message(101, None)), None);
    }

    #[test]
    fn message_without_group_is_dispatched_directly() {
        let mut groups = MediaGroups::default();
        assert!(matches!(
            groups.offer(message(1, None), Instant::now()),
            Offer::Ungrouped(m) if m.id == 1
        ));
    }

    #[test]
    fn album_members_flush_as_one_message() {
        let mut groups = MediaGroups::default();
        let start = Instant::now();
        let Offer::Opened { key, grouping } = groups.offer(message(1, Some("g")), start) else {
            panic!("first member opens the group");
        };
        assert_eq!(
            grouping,
            Grouping::Opened {
                late_after_flush_ms: None
            }
        );
        let Offer::Joined { grouping } =
            groups.offer(message(2, Some("g")), start + Duration::from_millis(200))
        else {
            panic!("second member joins");
        };
        assert_eq!(grouping, Grouping::Joined { observed: 2 });

        let flushed = groups
            .flush(&key, start + MEDIA_GROUP_DEBOUNCE)
            .expect("group is pending");
        assert_eq!(ids(&flushed.message), [1, 2]);
        assert_eq!(flushed.observed, 2);
        assert_eq!(flushed.held, MEDIA_GROUP_DEBOUNCE);
        assert_eq!(flushed.span, Duration::from_millis(200));
        assert!(groups.flush(&key, start + MEDIA_GROUP_DEBOUNCE).is_none());
    }

    #[test]
    fn late_member_opens_a_new_group_and_is_flagged() {
        let mut groups = MediaGroups::default();
        let start = Instant::now();
        let Offer::Opened { key, .. } = groups.offer(message(1, Some("g")), start) else {
            panic!("first member opens the group");
        };
        let flushed_at = start + MEDIA_GROUP_DEBOUNCE;
        groups.flush(&key, flushed_at).expect("group is pending");

        let late = flushed_at + Duration::from_millis(700);
        let Offer::Opened {
            key: late_key,
            grouping,
        } = groups.offer(message(2, Some("g")), late)
        else {
            panic!("late member opens a new group");
        };
        assert_eq!(late_key, key);
        assert_eq!(
            grouping,
            Grouping::Opened {
                late_after_flush_ms: Some(700)
            }
        );
        let second = groups
            .flush(&key, late + MEDIA_GROUP_DEBOUNCE)
            .expect("late group is pending");
        assert_eq!(ids(&second.message), [2]);
    }

    #[test]
    fn different_groups_stay_apart() {
        let mut groups = MediaGroups::default();
        let now = Instant::now();
        assert!(matches!(
            groups.offer(message(1, Some("a")), now),
            Offer::Opened { .. }
        ));
        assert!(matches!(
            groups.offer(message(2, Some("b")), now),
            Offer::Opened { .. }
        ));
    }

    fn open_and_flush(groups: &mut MediaGroups, id: i64, group: &str, at: Instant) {
        let Offer::Opened { key, .. } = groups.offer(message(id, Some(group)), at) else {
            panic!("opens");
        };
        groups.flush(&key, at).expect("pending");
    }

    fn late_ms(groups: &mut MediaGroups, id: i64, group: &str, at: Instant) -> Option<u64> {
        let Offer::Opened {
            grouping: Grouping::Opened {
                late_after_flush_ms,
            },
            ..
        } = groups.offer(message(id, Some(group)), at)
        else {
            panic!("opens");
        };
        late_after_flush_ms
    }

    #[test]
    fn idle_flush_memory_expires_at_lookup() {
        let mut groups = MediaGroups::default();
        let start = Instant::now();
        open_and_flush(&mut groups, 1, "g", start);
        // No flush in between prunes the entry; the lookup itself must age it out.
        assert_eq!(late_ms(&mut groups, 2, "g", start + FLUSH_MEMORY), None);
    }

    #[test]
    fn flush_memory_is_capped() {
        let mut groups = MediaGroups::default();
        let start = Instant::now();
        for n in 0..=FLUSH_MEMORY_CAP {
            open_and_flush(
                &mut groups,
                n as i64,
                &format!("g{n}"),
                start + Duration::from_millis(n as u64),
            );
        }
        assert_eq!(groups.flushed.len(), FLUSH_MEMORY_CAP);
        let later = start + Duration::from_secs(1);
        // The oldest key was dropped; the newest is still remembered.
        assert_eq!(late_ms(&mut groups, 1000, "g0", later), None);
        assert!(late_ms(&mut groups, 1001, &format!("g{FLUSH_MEMORY_CAP}"), later).is_some());
    }

    #[test]
    fn flush_memory_expires() {
        let mut groups = MediaGroups::default();
        let start = Instant::now();
        let Offer::Opened { key, .. } = groups.offer(message(1, Some("g")), start) else {
            panic!("opens");
        };
        groups.flush(&key, start).expect("pending");
        let Offer::Opened { key: other, .. } = groups.offer(message(2, Some("h")), start) else {
            panic!("opens");
        };
        groups.flush(&other, start + FLUSH_MEMORY).expect("pending");
        let Offer::Opened { grouping, .. } =
            groups.offer(message(3, Some("g")), start + FLUSH_MEMORY)
        else {
            panic!("opens");
        };
        assert_eq!(
            grouping,
            Grouping::Opened {
                late_after_flush_ms: None
            }
        );
    }
}
