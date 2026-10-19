//! Delivery of Mini App artifact feedback into the conversation that owns the
//! thread.
//!
//! The engine (`zdx_engine::core::artifact_feedback`) validates and saves the
//! bundle and writes the prompt; this module only decides where it goes and
//! puts it on that chat's normal per-topic queue as a synthetic prompt, so it
//! waits behind a running turn like any other message and is answered with the
//! thread's own profile, model, and alias routing. A legacy worker mirror topic
//! is a Telegram topic like any other: the queued message takes the same path a
//! typed one does there (`route_worker_topic_message`), which acknowledges in
//! the topic and whose results post back to it.
//!
//! Threads that are not a Telegram chat or topic — TUI/CLI threads, and worker
//! threads opened directly — are refused, never rerouted to another chat.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use zdx_engine::core::artifact_feedback::mark_queued;
use zdx_engine::core::thread_persistence;

use crate::bot::context::BotContext;
use crate::bot::queue::{ChatQueueMap, dispatch_message};
use crate::bot::synthetic::{SyntheticKind, build_synthetic_message};
use crate::handlers::message::{escape_html, parse_topic_thread_id};
use crate::telegram::Message;
use crate::types::LocalAttachment;

/// The Telegram chat (and topic) a thread id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FeedbackRoute {
    pub chat_id: i64,
    pub topic_id: Option<i64>,
}

/// Maps a thread id minted by the bot back to its chat.
pub(crate) fn route_for_thread(thread_id: &str) -> Option<FeedbackRoute> {
    if let Some((chat_id, topic_id)) = parse_topic_thread_id(thread_id) {
        return Some(FeedbackRoute {
            chat_id,
            topic_id: Some(topic_id),
        });
    }
    let chat_id = thread_id.strip_prefix("telegram-")?.parse().ok()?;
    Some(FeedbackRoute {
        chat_id,
        topic_id: None,
    })
}

/// Decides where feedback for `requested` (the id the user opened) goes. Reads
/// persisted thread metadata for the refusal message, so call it off the async
/// workers.
///
/// # Errors
/// Returns the message to show when the thread has no supported destination.
pub(crate) fn resolve_destination(requested: &str) -> Result<FeedbackRoute, &'static str> {
    if let Some(route) = route_for_thread(requested) {
        return Ok(route);
    }
    let is_worker = thread_persistence::read_thread_summary_cached(requested)
        .ok()
        .flatten()
        .is_some_and(|summary| summary.is_worker());
    Err(if is_worker {
        "Feedback on a worker thread cannot be sent from the Mini App yet. Your notes are kept; describe them in the orchestrator's topic instead."
    } else {
        "This thread does not belong to a Telegram chat, so feedback cannot be sent from here yet. Your notes are kept."
    })
}

/// A saved bundle ready to queue.
pub(crate) struct FeedbackDelivery {
    pub route: FeedbackRoute,
    pub user_id: i64,
    pub prompt: String,
    pub artifact_name: String,
    pub annotation_count: usize,
    pub attachments: Vec<LocalAttachment>,
    /// The saved bundle; marked queued right before the message is queued.
    pub bundle_dir: PathBuf,
}

pub(crate) type DeliverFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Where the Mini App server hands feedback. The bot implementation is
/// [`BotFeedbackSink`]; route tests record deliveries instead.
pub(crate) trait FeedbackSink: Send + Sync {
    /// Whether `user_id` may send into `route`. Checked before anything is
    /// saved, so a refused submission leaves nothing behind.
    fn allows(&self, user_id: i64, route: FeedbackRoute) -> bool;
    /// Marks the bundle queued and queues it. `Err` means nothing was queued,
    /// so a retry must queue it again.
    fn deliver(&self, delivery: FeedbackDelivery) -> DeliverFuture<'_>;
}

type Dispatch = Arc<dyn Fn(Message) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub(crate) struct BotFeedbackSink {
    context: Arc<BotContext>,
    dispatch: Dispatch,
}

impl BotFeedbackSink {
    pub(crate) fn new(context: Arc<BotContext>, queues: ChatQueueMap) -> Self {
        let queue_context = Arc::clone(&context);
        let dispatch: Dispatch = Arc::new(move |message| {
            let context = Arc::clone(&queue_context);
            let queues = Arc::clone(&queues);
            Box::pin(async move { dispatch_message(&queues, &context, message).await })
        });
        Self { context, dispatch }
    }

    /// A sink whose queue step is `dispatch`, for tests that check what would
    /// be queued without running an agent turn.
    #[cfg(test)]
    fn with_dispatch(context: Arc<BotContext>, dispatch: Dispatch) -> Self {
        Self { context, dispatch }
    }
}

/// The same gate the queue applies to a real message (`should_process_message`),
/// checked up front because the queue drops a refused message silently. A
/// private chat must be the sender's own chat with the bot.
pub(crate) fn route_allowed(
    allowlist_user_ids: &std::collections::HashSet<i64>,
    allowlist_chat_ids: &std::collections::HashSet<i64>,
    user_id: i64,
    route: FeedbackRoute,
) -> bool {
    if !allowlist_user_ids.contains(&user_id) {
        return false;
    }
    if route.chat_id < 0 {
        allowlist_chat_ids.contains(&route.chat_id)
    } else {
        route.chat_id == user_id
    }
}

impl FeedbackSink for BotFeedbackSink {
    fn allows(&self, user_id: i64, route: FeedbackRoute) -> bool {
        route_allowed(
            self.context.allowlist_user_ids(),
            self.context.allowlist_chat_ids(),
            user_id,
            route,
        )
    }

    fn deliver(&self, delivery: FeedbackDelivery) -> DeliverFuture<'_> {
        Box::pin(async move {
            let FeedbackDelivery {
                route,
                user_id,
                prompt,
                artifact_name,
                annotation_count,
                attachments,
                bundle_dir,
            } = delivery;
            let mut message = build_synthetic_message(
                route.chat_id,
                route.topic_id,
                user_id,
                &prompt,
                SyntheticKind::ArtifactFeedback,
            )
            .ok_or_else(|| anyhow::anyhow!("could not build the feedback message"))?;
            message.local_attachments = attachments;

            // Marker first: if it cannot be written nothing is queued and a
            // retry is safe. Queueing itself cannot fail once the sender passed
            // `allows`, so a marker never describes something left unqueued.
            mark_queued(&bundle_dir)?;

            // The prompt itself never appears in chat, so say what was sent.
            // Best effort: the turn runs either way.
            let notice = format!(
                "📝 Feedback on <b>{}</b> from the Mini App ({} {}).",
                escape_html(&artifact_name),
                annotation_count,
                if annotation_count == 1 {
                    "note"
                } else {
                    "notes"
                }
            );
            if let Err(err) = self
                .context
                .client()
                .send_message(route.chat_id, &notice, None, route.topic_id)
                .await
            {
                tracing::warn!(chat_id = route.chat_id, err = %format!("{err:#}"), "Failed to post artifact feedback notice");
            }

            tracing::info!(
                chat_id = route.chat_id,
                topic_id = ?route.topic_id,
                annotation_count,
                "Artifact feedback queued"
            );
            (self.dispatch)(message).await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::ingest::{AllowlistConfig, parse_incoming_message};
    use crate::telegram::TelegramClient;
    use crate::telegram::fake::{FakeTelegram, TOKEN};

    #[test]
    fn routes_only_bot_minted_thread_ids() {
        assert_eq!(
            route_for_thread("telegram--1001234-topic-77"),
            Some(FeedbackRoute {
                chat_id: -1_001_234,
                topic_id: Some(77)
            })
        );
        assert_eq!(
            route_for_thread("telegram-4242"),
            Some(FeedbackRoute {
                chat_id: 4242,
                topic_id: None
            })
        );
        assert_eq!(route_for_thread("20261018-cli-thread"), None);
        assert_eq!(route_for_thread("telegram-abc"), None);
    }

    /// Worker threads opened directly are refused with their own message, not
    /// rerouted to the orchestrator's chat.
    #[test]
    fn worker_and_cli_threads_are_refused() {
        let _home = zdx_engine::test_support::temp_zdx_home();
        let worker = format!("worker-{}", uuid::Uuid::new_v4());
        let mut thread = thread_persistence::Thread::with_id(worker.clone()).unwrap();
        thread.set_origin(None, Some("telegram--100555-topic-3".to_string()), None);
        thread
            .append(&thread_persistence::ThreadEvent::assistant_message("done"))
            .unwrap();
        let refused = resolve_destination(&worker).unwrap_err();
        assert!(refused.contains("worker thread"), "{refused}");

        let cli = format!("cli-{}", uuid::Uuid::new_v4());
        thread_persistence::Thread::with_id(cli.clone())
            .unwrap()
            .append(&thread_persistence::ThreadEvent::assistant_message("hi"))
            .unwrap();
        assert!(
            resolve_destination(&cli)
                .unwrap_err()
                .contains("Telegram chat")
        );
    }

    #[test]
    fn private_chats_must_be_the_senders_own() {
        let users = HashSet::from([7, 8]);
        let chats = HashSet::from([-100]);
        let dm = |chat_id| FeedbackRoute {
            chat_id,
            topic_id: None,
        };
        assert!(route_allowed(&users, &chats, 7, dm(7)));
        assert!(!route_allowed(&users, &chats, 7, dm(8)));
        assert!(route_allowed(&users, &chats, 7, dm(-100)));
        assert!(!route_allowed(&users, &chats, 7, dm(-200)));
        assert!(!route_allowed(&users, &chats, 9, dm(9)));
    }

    /// The production sink against a fake Bot API: the bundle is marked queued,
    /// the chat gets a notice, and the queued message passes the queue's
    /// sender gate and ingests (through the same code a Telegram message
    /// takes) into the prompt plus the labelled attachments. The agent turn
    /// itself is not run.
    #[tokio::test]
    async fn production_sink_queues_a_message_that_ingests_with_labels() {
        let home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let client = TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone());
        let users = HashSet::from([7]);
        let chats = HashSet::from([-100_777]);
        let context = Arc::new(BotContext::for_test_allowing(
            client.clone(),
            zdx_engine::config::Config::default(),
            std::env::temp_dir(),
            users.clone(),
            chats.clone(),
        ));
        let captured: Arc<std::sync::Mutex<Vec<Message>>> = Arc::default();
        let sink_captured = Arc::clone(&captured);
        let sink = BotFeedbackSink::with_dispatch(
            Arc::clone(&context),
            Arc::new(move |message| {
                sink_captured.lock().unwrap().push(message);
                Box::pin(async {})
            }),
        );

        let bundle = home.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let sketch = bundle.join("note-1-drawing.png");
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(4, 4)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&sketch, png).unwrap();
        let route = FeedbackRoute {
            chat_id: -100_777,
            topic_id: Some(9),
        };
        assert!(sink.allows(7, route));
        assert!(!sink.allows(8, route));

        sink.deliver(FeedbackDelivery {
            route,
            user_id: 7,
            prompt: "[artifact feedback]\nNote 1. Change".to_string(),
            artifact_name: "report <v2>.html".to_string(),
            annotation_count: 1,
            attachments: vec![LocalAttachment::Image {
                path: sketch.clone(),
                label: "Drawing sketch for note 1".to_string(),
            }],
            bundle_dir: bundle.clone(),
        })
        .await
        .expect("deliver");

        assert!(bundle.join("queued").is_file(), "marked queued");
        let notice = fake
            .seen()
            .into_iter()
            .find(|seen| seen.method == "sendMessage")
            .expect("notice posted");
        assert!(
            notice.body.contains("report &lt;v2&gt;.html"),
            "{}",
            notice.body
        );
        assert!(
            notice.body.contains("\"message_thread_id\":9"),
            "{}",
            notice.body
        );

        let message = captured.lock().unwrap().pop().expect("message queued");
        assert_eq!(message.synthetic, Some(SyntheticKind::ArtifactFeedback));
        assert_eq!(message.effective_thread_id(), Some(9));
        assert!(crate::bot::queue::should_process_message(
            &context, &message
        ));

        let incoming = parse_incoming_message(
            &client,
            AllowlistConfig {
                user_ids: &users,
                chat_ids: &chats,
            },
            &zdx_engine::config::Config::default(),
            &message,
            None,
        )
        .await
        .expect("ingest")
        .expect("message accepted");
        let prompt = crate::agent::build_user_text(&incoming);
        assert!(prompt.starts_with("[artifact feedback]"), "{prompt}");
        assert!(
            prompt.contains(&format!(
                "Drawing sketch for note 1: image attachment saved at {}",
                sketch.display()
            )),
            "{prompt}"
        );
        assert!(incoming.images[0].inline().is_some());
    }

    /// Image artifact feedback, end to end short of a model call: the engine
    /// saves the bundle, the production sink queues it (captured), ingest
    /// loads it like a Telegram message, and the user message the agent would
    /// get carries the marked image itself as an image block, in the order
    /// its label appears in the text.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one pipeline checked end to end
    async fn image_feedback_reaches_the_model_as_the_marked_image() {
        use base64::Engine as _;
        use zdx_engine::core::artifact_feedback::canonical::canonical_for_artifact;
        use zdx_engine::core::artifact_feedback::{SaveOutcome, save_feedback};
        use zdx_engine::providers::{ChatContentBlock, MessageContent};

        let _home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let client = TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone());
        let users = HashSet::from([7]);
        let chats = HashSet::from([-100_777]);
        let context = Arc::new(BotContext::for_test_allowing(
            client.clone(),
            zdx_engine::config::Config::default(),
            std::env::temp_dir(),
            users.clone(),
            chats.clone(),
        ));
        let captured: Arc<std::sync::Mutex<Vec<Message>>> = Arc::default();
        let sink_captured = Arc::clone(&captured);
        let sink = BotFeedbackSink::with_dispatch(
            Arc::clone(&context),
            Arc::new(move |message| {
                sink_captured.lock().unwrap().push(message);
                Box::pin(async {})
            }),
        );

        let thread_id = "telegram--100777-topic-9";
        let artifact_dir = zdx_engine::config::paths::artifact_dir_for_thread(Some(thread_id));
        std::fs::create_dir_all(&artifact_dir).unwrap();
        // A 3000×1500 original: the agent gets its 1568×784 canonical copy.
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(3000, 1500, |x, y| {
            image::Rgb([
                30,
                u8::try_from(x / 24).unwrap(),
                u8::try_from(y / 12).unwrap(),
            ])
        }))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
        std::fs::write(artifact_dir.join("chart.png"), &png).unwrap();
        thread_persistence::Thread::with_id(thread_id.to_string())
            .unwrap()
            .append(&thread_persistence::ThreadEvent::assistant_message("chart"))
            .unwrap();
        let sha = {
            use sha2::Digest as _;
            sha2::Sha256::digest(&png)
                .iter()
                .fold(String::new(), |mut out, byte| {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{byte:02x}");
                    out
                })
        };
        // What the Mini App gets from `/artifacts/image` and sends back.
        let copy = canonical_for_artifact(thread_id, "chart.png").unwrap();
        assert_eq!(copy.meta.source_sha256, sha);
        assert_eq!((copy.meta.width, copy.meta.height), (1568, 784));
        let submission = serde_json::from_value(serde_json::json!({
            "submission_id": format!("sub-{}", uuid::Uuid::new_v4()),
            "artifact_path": "chart.png",
            "artifact_sha256": sha,
            "image": {
                "canonical_sha256": copy.meta.sha256,
                "width": copy.meta.width,
                "height": copy.meta.height,
            },
            "annotations": [
                { "intent": "change", "comment": "Move the legend here", "target": { "kind": "image_pin", "point": { "x": 0.5, "y": 0.5 } } },
            ],
        }))
        .unwrap();
        let SaveOutcome::Saved(saved) = save_feedback(thread_id, "user=7", &submission).unwrap()
        else {
            panic!("expected a saved bundle");
        };

        sink.deliver(FeedbackDelivery {
            route: route_for_thread(thread_id).unwrap(),
            user_id: 7,
            attachments: crate::server::feedback_attachments(&saved),
            prompt: saved.prompt.clone(),
            artifact_name: saved.artifact_name.clone(),
            annotation_count: saved.annotation_count,
            bundle_dir: saved.dir.clone(),
        })
        .await
        .expect("deliver");
        let message = captured.lock().unwrap().pop().expect("message queued");
        let incoming = parse_incoming_message(
            &client,
            AllowlistConfig {
                user_ids: &users,
                chat_ids: &chats,
            },
            &zdx_engine::config::Config::default(),
            &message,
            None,
        )
        .await
        .expect("ingest")
        .expect("message accepted");

        let mut thread = thread_persistence::Thread::with_id(thread_id.to_string()).unwrap();
        let mut messages = Vec::new();
        crate::agent::record_user_message(&mut thread, &mut messages, &incoming, None, None)
            .unwrap();
        let MessageContent::Blocks(blocks) = &messages[0].content else {
            panic!("expected text plus image blocks");
        };
        let ChatContentBlock::Text { text, .. } = &blocks[0] else {
            panic!("text first");
        };
        assert!(
            text.contains("Note 1. Change: pin at x 784, y 392 px"),
            "{text}"
        );
        assert!(text.contains("Comment: Move the legend here"), "{text}");
        let marked_at = text
            .find("Marked image (every note's mark, numbered): image attachment saved at")
            .expect("marked image label");
        let closeup_at = text
            .find("Close-up of note 1: image attachment saved at")
            .expect("close-up label");
        assert!(marked_at < closeup_at, "{text}");

        let images: Vec<_> = blocks
            .iter()
            .filter_map(|block| match block {
                ChatContentBlock::Image { mime_type, data } => Some((mime_type.as_str(), data)),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 2, "marked image and close-up");
        // The model gets the marked copy's exact bytes: no resize or
        // re-encode on the way, so its pixels are the copy the user marked.
        let block = base64::engine::general_purpose::STANDARD
            .decode(images[0].1)
            .unwrap();
        assert_eq!(block, std::fs::read(&saved.images[0].path).unwrap());
        assert_eq!(images[0].0, "image/jpeg");
        let marked = image::load_from_memory(&block).unwrap().to_rgb8();
        assert_eq!(marked.dimensions(), (copy.meta.width, copy.meta.height));
        // unit 3.92 px; the red ring runs 17.6 to 27.4 px from the pin.
        let ring = marked.get_pixel(784 - 22, 392);
        assert!(ring[0] > 150 && ring[1] < 120, "{ring:?}");
        let clean = image::load_from_memory(&copy.bytes).unwrap().to_rgb8();
        for (x, y) in [(784, 392), (100, 100), (1500, 700)] {
            let (a, b) = (marked.get_pixel(x, y), clean.get_pixel(x, y));
            assert!((0..3).all(|c| a[c].abs_diff(b[c]) <= 4), "{a:?} vs {b:?}");
        }
    }

    /// A marker that cannot be written fails the delivery before anything is
    /// queued or posted, so a retry is safe.
    #[tokio::test]
    async fn marker_failure_queues_nothing() {
        let home = zdx_engine::test_support::temp_zdx_home();
        let fake = FakeTelegram::start().await;
        let client = TelegramClient::with_base_url(TOKEN.to_string(), fake.base_url.clone());
        let context = Arc::new(BotContext::for_test(
            client,
            zdx_engine::config::Config::default(),
            std::env::temp_dir(),
        ));
        let captured: Arc<std::sync::Mutex<Vec<Message>>> = Arc::default();
        let sink_captured = Arc::clone(&captured);
        let sink = BotFeedbackSink::with_dispatch(
            context,
            Arc::new(move |message| {
                sink_captured.lock().unwrap().push(message);
                Box::pin(async {})
            }),
        );
        let result = sink
            .deliver(FeedbackDelivery {
                route: FeedbackRoute {
                    chat_id: 7,
                    topic_id: None,
                },
                user_id: 7,
                prompt: "x".to_string(),
                artifact_name: "a.html".to_string(),
                annotation_count: 1,
                attachments: Vec::new(),
                bundle_dir: home.path().join("missing-bundle"),
            })
            .await;
        assert!(result.is_err());
        assert!(captured.lock().unwrap().is_empty());
        assert!(fake.seen().is_empty());
    }
}
