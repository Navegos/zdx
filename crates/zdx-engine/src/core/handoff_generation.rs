//! Handoff context generation for an existing thread using an LLM subagent.
//!
//! Loads the thread's persisted events, formats them as a transcript, and asks
//! a subagent (configured via `Config::handoff_model`) to produce a condensed
//! context prompt for continuing the work in a fresh thread. Shared by the TUI
//! `/handoff` command and the Telegram bot.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio_util::sync::CancellationToken;

use crate::config::ThinkingLevel;
use crate::core::subagent::{ExecSubagentOptions, run_exec_subagent_with_cancel};
use crate::core::thread_persistence as tp;
use crate::prompts::HANDOFF_PROMPT_TEMPLATE;
use crate::zdx_context::build_zdx_context;

/// Timeout for handoff generation subagent (2 minutes).
const HANDOFF_TIMEOUT_SECS: u64 = 120;

/// One ancestor in a handoff lineage: `(thread_id, display_title)`.
type LineageEntry = (String, String);

/// Walks `handoff_from` from `source_thread_id` up to the root, returning the
/// chain as `(id, display_title)` starting at the source thread.
///
/// Each step reads one thread's meta line by ID, so the walk costs one file
/// open per ancestor rather than a scan of the whole threads directory. The
/// walk stops when a thread has no `handoff_from` or its parent isn't found.
/// Falls back to a single source entry when thread metadata can't be read.
fn collect_lineage(source_thread_id: &str) -> Vec<LineageEntry> {
    let mut chain: Vec<LineageEntry> = Vec::new();
    let mut current = Some(source_thread_id.to_string());
    while let Some(id) = current {
        let Ok(Some(summary)) = tp::read_thread_summary(&id) else {
            if chain.is_empty() {
                chain.push((id.clone(), id));
            }
            break;
        };
        chain.push((summary.id.clone(), summary.display_title()));
        current = summary.handoff_from;
    }
    chain
}

const HANDOFF_KIND: &str = "Handoff";
const SIDE_QUESTION_KIND: &str = "Side question";
const USER_MESSAGE_LABEL: &str = "User message:\n";

/// Label line that starts the helper's generated background in a handoff seed.
fn generated_background_label(source_thread_id: &str) -> String {
    format!(
        "Generated background from thread {source_thread_id} (written by a helper model; it can be incomplete or wrong):\n"
    )
}

/// Formats one lineage entry as `id "title"`.
fn format_lineage_entry(entry: &LineageEntry) -> String {
    let (id, title) = entry;
    format!("{id} \"{title}\"")
}

/// Header line that starts a seeded thread: what kind of transition it is,
/// the source thread, and its ancestors, so the new assistant can
/// `read_thread` any of them.
fn build_source_header(kind: &str, lineage: &[LineageEntry]) -> String {
    let Some(source) = lineage.first() else {
        return format!("[{kind}]");
    };
    if lineage.len() <= 1 {
        return format!(
            "[{kind} from thread {}. Use read_thread on it for details.]",
            source.0
        );
    }
    let chain = lineage
        .iter()
        .map(format_lineage_entry)
        .collect::<Vec<_>>()
        .join(" ← ");
    format!(
        "[{kind} from thread {}. Lineage: {chain}. Use read_thread on any of these threads for details.]",
        format_lineage_entry(source)
    )
}

/// The user's own words, labelled so they stay apart from generated text.
/// `None` when the user sent nothing.
fn user_message_block(message: &str) -> Option<String> {
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| format!("{USER_MESSAGE_LABEL}{trimmed}"))
}

/// Seed for `/handoff`: header, the user's message verbatim, then the
/// helper's generated background, each part labelled with its origin.
fn build_handoff_seed(
    source_thread_id: &str,
    lineage: &[LineageEntry],
    next_message: &str,
    generated: &str,
) -> String {
    let background = format!(
        "{}{}",
        generated_background_label(source_thread_id),
        generated.trim()
    );
    [
        Some(build_source_header(HANDOFF_KIND, lineage)),
        user_message_block(next_message),
        Some(background),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// Builds the seed message for a side thread (`/btw`): a header pointing at
/// `source_thread_id` and its ancestors, then the user's message verbatim.
/// No generated text is added.
pub fn build_side_thread_seed(source_thread_id: &str, message: &str) -> String {
    let header = build_source_header(SIDE_QUESTION_KIND, &collect_lineage(source_thread_id));
    match user_message_block(message) {
        Some(block) => format!("{header}\n\n{block}"),
        None => header,
    }
}

/// The text a title should be generated from, for a message that may be a
/// `/handoff` or `/btw` seed built above.
///
/// For a seed, returns the user's message, or the generated background when
/// the user sent no message. Any other text, including a seed whose header or
/// labels were edited, is returned unchanged.
#[must_use]
pub fn seed_title_text(message: &str) -> &str {
    let Some((header, rest)) = message.split_once("\n\n") else {
        return message;
    };
    let Some(source_id) = [HANDOFF_KIND, SIDE_QUESTION_KIND].iter().find_map(|kind| {
        header
            .strip_prefix(&format!("[{kind} from thread "))
            .and_then(|tail| tail.split([' ', '.']).next())
            .filter(|id| !id.is_empty())
    }) else {
        return message;
    };
    let background_label = generated_background_label(source_id);
    if let Some(user) = rest.strip_prefix(USER_MESSAGE_LABEL) {
        // The user's words are verbatim and may contain anything, so only the
        // exact label this seed was built with ends them.
        let end = user
            .find(&format!("\n\n{background_label}"))
            .unwrap_or(user.len());
        return &user[..end];
    }
    rest.strip_prefix(background_label.as_str())
        .unwrap_or(message)
}

/// Builds the prompt for handoff generation.
fn build_handoff_prompt(thread_content: &str, next_message: &str, zdx_context: &str) -> String {
    HANDOFF_PROMPT_TEMPLATE
        .replace("{{ZDX_CONTEXT}}", zdx_context)
        .replace("{{THREAD_CONTENT}}", thread_content)
        .replace("{{NEXT_MESSAGE}}", next_message)
}

/// Generates a handoff prompt for `thread_id`, ready to seed a new thread.
///
/// The result starts with a header naming the source thread and its lineage,
/// then the user's `next_message` verbatim (when non-empty), then the
/// LLM-generated background, each part labelled with where it came from.
///
/// # Errors
/// Returns an error when the thread cannot be loaded, is empty, or the
/// subagent fails / times out / is cancelled.
pub async fn generate_handoff(
    thread_id: &str,
    next_message: &str,
    handoff_model: &str,
    root: &Path,
    cancel: Option<CancellationToken>,
) -> Result<String> {
    let events = tp::load_thread_events(thread_id)
        .with_context(|| format!("load thread '{thread_id}' for handoff"))?;
    ensure!(!events.is_empty(), "Thread '{thread_id}' is empty");
    let thread_content = tp::format_handoff_transcript(&events);

    let generation_prompt =
        build_handoff_prompt(&thread_content, next_message, &build_zdx_context(root));
    let lineage = collect_lineage(thread_id);

    let spec = crate::models::ModelSpec::parse(handoff_model);
    let options = ExecSubagentOptions {
        model: Some(spec.without_thinking()),
        system_prompt: None,
        thinking_level: Some(spec.thinking.unwrap_or(ThinkingLevel::Low)),
        no_tools: true,
        no_system_prompt: true,
        tools_override: None,
        event_filter: Some(vec!["turn_finished".to_string()]),
        timeout: Some(Duration::from_secs(HANDOFF_TIMEOUT_SECS)),
        activity_kind: Some("helper:handoff".to_string()),
        activity_parent_thread_id: Some(thread_id.to_string()),
        thread_origin_kind: Some("helper:handoff".to_string()),
        ..Default::default()
    };

    let generated_prompt =
        run_exec_subagent_with_cancel(root, &generation_prompt, &options, cancel, None).await?;
    Ok(build_handoff_seed(
        thread_id,
        &lineage,
        next_message,
        &generated_prompt,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        LineageEntry, build_handoff_seed, build_source_header, seed_title_text, user_message_block,
    };

    fn entry(id: &str, title: &str) -> LineageEntry {
        (id.to_string(), title.to_string())
    }

    #[test]
    fn side_thread_seed_starts_with_header_then_user_message() {
        let header = build_source_header("Side question", &[entry("thread-side", "Parent work")]);
        let block = user_message_block("what files did we touch?").unwrap();
        let seed = format!("{header}\n\n{block}");
        assert!(seed.starts_with("[Side question from thread thread-side."));
        assert!(seed.contains("read_thread"));
        assert!(seed.ends_with("User message:\nwhat files did we touch?"));
    }

    #[test]
    fn handoff_seed_separates_user_words_from_generated_background() {
        let lineage = vec![entry("thread-xyz", "Streamline comments")];
        let msg = "now lets streamline the comments";
        let seed = build_handoff_seed("thread-xyz", &lineage, msg, "Open question: keep or drop?");

        assert!(seed.starts_with("[Handoff from thread thread-xyz."));
        let user = seed
            .find(&format!("User message:\n{msg}"))
            .expect("user block");
        let generated = seed
            .find("Generated background from thread thread-xyz")
            .expect("generated block");
        assert!(
            user < generated,
            "the user's words come before generated text"
        );
        assert!(seed.ends_with("Open question: keep or drop?"));
        assert!(!seed.contains("My goal:"));
    }

    #[test]
    fn handoff_seed_without_a_message_has_no_user_block() {
        let lineage = vec![entry("thread-abc", "Some work")];
        let seed = build_handoff_seed("thread-abc", &lineage, "   ", "Background");
        assert!(seed.starts_with("[Handoff from thread thread-abc."));
        assert!(!seed.contains("User message:"));
        assert!(seed.contains("Generated background from thread thread-abc"));
    }

    #[test]
    fn title_text_is_the_user_message_of_a_seed() {
        let lineage = vec![entry("thread-xyz", "Streamline comments")];
        let msg = "first line\n\nUser message:\nnot a label\nGenerated background from thread thread-xyz\n\n[Handoff from thread other.]";
        let handoff = build_handoff_seed("thread-xyz", &lineage, msg, "Background text");
        assert_eq!(seed_title_text(&handoff), msg);

        let header = build_source_header("Side question", &lineage);
        let side = format!("{header}\n\n{}", user_message_block(msg).unwrap());
        assert_eq!(seed_title_text(&side), msg);

        let chain = vec![entry("thread-b", "B title. Part 2"), entry("thread-a", "A")];
        let nested = build_handoff_seed("thread-b", &chain, "keep going", "Background");
        assert_eq!(seed_title_text(&nested), "keep going");
    }

    #[test]
    fn title_text_falls_back_to_background_then_to_the_whole_text() {
        let lineage = vec![entry("thread-abc", "Some work")];
        let seed = build_handoff_seed("thread-abc", &lineage, "", "Fix the flaky test\nin CI");
        assert_eq!(seed_title_text(&seed), "Fix the flaky test\nin CI");

        for text in [
            "Fix the login bug",
            "two\n\nparagraphs",
            "[Handoff from thread x.]",
            "[Handoff from thread x.]\n\nedited by the user",
            "[Note from thread x.]\n\nUser message:\nhi",
        ] {
            assert_eq!(seed_title_text(text), text);
        }
    }

    #[test]
    fn single_thread_header_has_no_lineage_chain() {
        let header = build_source_header("Handoff", &[entry("thread-solo", "Solo work")]);
        assert!(!header.contains("Lineage:"));
        assert!(!header.contains('←'));
    }

    #[test]
    fn header_renders_full_lineage_in_order() {
        // Source first, then ancestors up to the root: D ← C ← B ← A.
        let lineage = vec![
            entry("thread-d", "Deploy"),
            entry("thread-c", "Cleanup"),
            entry("thread-b", "Build"),
            entry("thread-a", "Analyze"),
        ];
        let header = build_source_header("Handoff", &lineage);

        assert!(header.starts_with("[Handoff from thread thread-d \"Deploy\"."));
        assert!(header.contains("Lineage:"));
        assert!(header.contains("read_thread"));
        for (id, title) in [
            ("thread-d", "Deploy"),
            ("thread-c", "Cleanup"),
            ("thread-b", "Build"),
            ("thread-a", "Analyze"),
        ] {
            assert!(header.contains(id), "missing ancestor id {id}");
            assert!(header.contains(title), "missing ancestor title {title}");
        }
        let chain = &header[header.find("Lineage:").unwrap()..];
        let pos = |needle: &str| chain.find(needle).unwrap();
        assert!(
            pos("thread-d") < pos("thread-c")
                && pos("thread-c") < pos("thread-b")
                && pos("thread-b") < pos("thread-a"),
            "lineage must render in order D ← C ← B ← A"
        );
    }
}
