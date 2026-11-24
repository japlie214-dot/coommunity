use anyhow::Result;
use tokio::sync::broadcast;
use tracing::warn;

use crate::config::Config;
use crate::db::Database;
use crate::models::{
    mention_matches_agent, HubEvent, Message, Subscription, SubscriptionMode, Thread, ThreadKind,
};

/// Hook-first inbox router and delivery audit logger.
///
/// Messages are persisted in SQLite with per-agent `last_read_seq` cursors and surfaced to
/// agents via lifecycle hooks (`pre-turn` / `session-start`) and MCP `check_inbox` / `read_thread`.
#[derive(Clone)]
pub struct Dispatcher {
    db: Database,
    _cfg: Config,
    events: broadcast::Sender<HubEvent>,
}

impl Dispatcher {
    pub fn new(db: Database, cfg: Config, events: broadcast::Sender<HubEvent>) -> Self {
        Self {
            db,
            _cfg: cfg,
            events,
        }
    }

    pub fn dispatch_message(&self, thread: Thread, message: Message) {
        let this = self.clone();
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || this.dispatch_inner(thread, message)).await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!(error = %err, "dispatch_message failed"),
                Err(err) => warn!(error = %err, "dispatch_message task panicked"),
            }
        });
    }

    fn dispatch_inner(&self, thread: Thread, message: Message) -> Result<()> {
        // Global kill-switch
        if self.db.is_paused()? {
            self.record_and_emit(
                &message.id,
                &thread.id,
                "*",
                "guardrail",
                "skipped_paused",
                Some("hub is globally paused"),
            );
            return Ok(());
        }

        // Thread mute kill-switch
        if thread.muted {
            self.record_and_emit(
                &message.id,
                &thread.id,
                "*",
                "guardrail",
                "skipped_muted",
                Some("thread is muted"),
            );
            return Ok(());
        }

        // Anti-loop hop budget check
        if thread.hop_count > thread.max_hops {
            self.record_and_emit(
                &message.id,
                &thread.id,
                "*",
                "guardrail",
                "skipped_hop_cap",
                Some(&format!(
                    "hop cap reached ({}/{}); run `coom reset-hops {}` to resume",
                    thread.hop_count, thread.max_hops, thread.id
                )),
            );
            return Ok(());
        }

        let subs = self.db.list_subscriptions(Some(&thread.id))?;
        for sub in subs {
            // Echo suppression: never notify the sender of their own post
            if sub.agent_address == message.sender {
                continue;
            }
            if !should_notify(&thread, &sub, &message) {
                continue;
            }

            let is_registered = self.db.get_agent(&sub.agent_address)?.is_some();
            let (status, detail) = if is_registered {
                (
                    "queued_unread",
                    "tracked in unread inbox; surfaced on next hook turn or check_inbox",
                )
            } else {
                (
                    "queued_unregistered",
                    "agent not yet registered; tracked in unread inbox",
                )
            };

            self.record_and_emit(
                &message.id,
                &thread.id,
                &sub.agent_address,
                "hook_inbox",
                status,
                Some(detail),
            );
        }

        Ok(())
    }

    fn record_and_emit(
        &self,
        message_id: &str,
        thread_id: &str,
        target_agent: &str,
        tier: &str,
        status: &str,
        detail: Option<&str>,
    ) {
        if let Ok(rec) = self
            .db
            .log_delivery(message_id, thread_id, target_agent, tier, status, detail)
        {
            let _ = self.events.send(HubEvent::DeliveryLogged { delivery: rec });
        }
    }
}

pub fn should_notify(thread: &Thread, sub: &Subscription, msg: &Message) -> bool {
    if sub.mode == SubscriptionMode::Muted {
        return false;
    }
    if thread.kind == ThreadKind::Dm {
        return true;
    }
    match sub.mode {
        SubscriptionMode::All | SubscriptionMode::Digest => true,
        SubscriptionMode::Mentions => msg
            .mentions
            .iter()
            .any(|m| mention_matches_agent(m, &sub.agent_address)),
        SubscriptionMode::Muted => false,
    }
}
