//! Performs the restart queued in `zdx_engine::service` (by Telegram
//! `/restart q` or `zdx service restart --queue`) once the machine is idle.
//!
//! The bot owns this because a queued restart must outlive the agent run that
//! asked for it, and the bot is the process that restarts by exiting. The
//! worker bridge drives it, because only that task can tell that no worker
//! completion is still waiting to reach its orchestrator.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use zdx_engine::agent_activity;
use zdx_engine::service::{self, QueuedRestart, Service};

use crate::bot::context::BotContext;
use crate::handlers::message::{escape_html, parse_topic_thread_id};

/// How often the worker bridge checks for a queued restart.
pub(crate) const POLL: Duration = Duration::from_secs(3);

pub(crate) enum RestartAttempt {
    /// Daemon restarted (status text, HTML-escaped); the bot should exit.
    Restarting(String),
    /// Refused because this many agent runs are active.
    Blocked(usize),
    /// Daemon restart failed (error text, HTML-escaped); the bot stays up.
    Failed(String),
}

/// Restarts the daemon (gated unless `force`) without exiting the bot.
pub(crate) async fn attempt_restart(force: bool) -> Result<RestartAttempt> {
    let outcome = tokio::task::spawn_blocking(move || service::restart(Service::Daemon, force))
        .await
        .context("join daemon restart task")?;
    Ok(match outcome {
        Ok(status) => RestartAttempt::Restarting(escape_html(&status)),
        Err(err) => match err.downcast_ref::<service::RestartBlocked>() {
            Some(blocked) => RestartAttempt::Blocked(blocked.active_runs()),
            None => RestartAttempt::Failed(escape_html(&format!("{err:#}"))),
        },
    })
}

enum Pending {
    None,
    Waiting,
    Ready(QueuedRestart),
}

/// Reads the queue and decides whether to act. Blocking: file and process checks.
fn check_pending() -> Pending {
    let request = match service::queued_restart() {
        Ok(Some(request)) => request,
        Ok(None) => return Pending::None,
        Err(err) => {
            tracing::warn!(err = %format!("{err:#}"), "Dropping unreadable queued restart");
            service::clear_queued_restart();
            return Pending::None;
        }
    };
    if request.is_fulfilled() {
        tracing::info!("Queued restart already fulfilled by a newer process; dropping it");
        service::clear_queued_restart();
        return Pending::None;
    }
    if agent_activity::list_active().is_empty() {
        Pending::Ready(request)
    } else {
        Pending::Waiting
    }
}

/// Performs a pending queued restart when no agent run is active.
///
/// Called only by the worker bridge (`orchestrator.rs`) once it has seen no
/// running or queued worker, no undelivered completion, and no queued or
/// running bot turn; this adds the cross-process `agent_activity` check.
pub(crate) async fn tick(context: &Arc<BotContext>) {
    if let Err(err) = try_tick(context).await {
        tracing::warn!(err = %format!("{err:#}"), "Queued restart check failed");
    }
}

async fn try_tick(context: &Arc<BotContext>) -> Result<()> {
    let Pending::Ready(request) = tokio::task::spawn_blocking(check_pending)
        .await
        .context("join queued restart check")?
    else {
        return Ok(());
    };
    let target = announce_target(
        request.thread_id.as_deref(),
        context.allowlist_chat_ids(),
        context.allowlist_user_ids(),
    );

    if request.includes(Service::Bot) && !zdx_engine::pidfile::is_supervised("bot") {
        service::clear_queued_restart();
        announce(
            context,
            target,
            "⚠️ Queued restart dropped: the bot has no active supervisor, so it cannot exit and come back.",
        )
        .await;
        return Ok(());
    }

    let mut status = None;
    if request.includes(Service::Daemon) {
        match attempt_restart(false).await? {
            RestartAttempt::Restarting(text) => status = Some(text),
            // A run started after the idle check; the gate caught it, so wait again.
            RestartAttempt::Blocked(_) => return Ok(()),
            RestartAttempt::Failed(error) => {
                service::clear_queued_restart();
                announce(
                    context,
                    target,
                    &format!(
                        "⚠️ Queued restart failed, so I left the bot running.\n<code>{error}</code>"
                    ),
                )
                .await;
                return Ok(());
            }
        }
    }

    service::clear_queued_restart();
    let prefix = status.map(|s| format!("🔄 {s}.\n")).unwrap_or_default();
    if request.includes(Service::Bot) {
        announce(
            context,
            target,
            &format!(
                "{prefix}👋 Queued restart: no agent run is active, exiting bot… launchd will restart it shortly."
            ),
        )
        .await;
        context.request_exit();
    } else {
        announce(
            context,
            target,
            &format!("{prefix}✅ Queued restart done: no agent run was active."),
        )
        .await;
    }
    Ok(())
}

/// Chat and topic to announce in, from the requesting thread id. Only
/// Telegram threads of allowlisted chats qualify; anything else stays silent.
fn announce_target(
    thread_id: Option<&str>,
    allowed_chats: &HashSet<i64>,
    allowed_users: &HashSet<i64>,
) -> Option<(i64, Option<i64>)> {
    let thread_id = thread_id?;
    let (chat_id, topic_id) = match parse_topic_thread_id(thread_id) {
        Some((chat_id, topic_id)) => (chat_id, Some(topic_id)),
        None => (thread_id.strip_prefix("telegram-")?.parse().ok()?, None),
    };
    let allowed = allowed_chats.contains(&chat_id) || allowed_users.contains(&chat_id);
    allowed.then_some((chat_id, topic_id))
}

async fn announce(context: &BotContext, target: Option<(i64, Option<i64>)>, text: &str) {
    let Some((chat_id, topic_id)) = target else {
        tracing::info!(message = text, "Queued restart (no thread to announce in)");
        return;
    };
    if let Err(err) = context
        .client()
        .send_message(chat_id, text, None, topic_id)
        .await
    {
        tracing::warn!(err = %format!("{err:#}"), "Failed to announce queued restart");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_only_in_allowlisted_telegram_threads() {
        let chats = HashSet::from([-100]);
        let users = HashSet::from([42]);
        let target = |id| announce_target(Some(id), &chats, &users);

        assert_eq!(target("telegram--100-topic-7"), Some((-100, Some(7))));
        assert_eq!(target("telegram-42"), Some((42, None)));
        assert_eq!(target("telegram--200-topic-7"), None);
        assert_eq!(target("a1b2c3-worker"), None);
        assert_eq!(announce_target(None, &chats, &users), None);
    }
}
