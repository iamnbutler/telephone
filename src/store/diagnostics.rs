//! Bounded journal metadata. Queue acceptance never implies pending or read.
use super::Store;
use crate::{address::Address, runtime::Runtime};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use uuid::Uuid;

pub const HISTORY_LIMIT: usize = 20;

#[derive(Debug, Serialize)]
pub struct DeliveryRecord {
    pub id: Uuid,
    pub from: Address,
    pub sent_at: u64,
    pub outcome: String,
    pub channel: &'static str,
    /// Only a Telephone inbox receipt, never a native read receipt.
    pub inbox_read: Option<bool>,
}

impl Store {
    pub fn recent_deliveries(&self, recipient: &Address) -> Result<Vec<DeliveryRecord>> {
        // The expression matches messages_recipient exactly. SQLite traverses
        // that recipient's rowids backwards; unrelated journals aren't scanned.
        let mut stmt = self.conn.prepare(
            "SELECT m.id, json_extract(m.envelope,'$.from'),
                    json_extract(m.envelope,'$.sent_at'), m.outcome, i.read
             FROM messages m LEFT JOIN inbox i ON i.id=m.id
             WHERE (CASE WHEN json_valid(m.envelope) THEN json_extract(m.envelope,'$.to') END)=?1
             ORDER BY m.rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![recipient.as_str(), HISTORY_LIMIT as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<bool>>(4)?,
            ))
        })?;
        let mut history = Vec::new();
        for row in rows {
            let (id, from, sent_at, outcome, inbox_read) = row?;
            // These are outcome-based observations, not today's route settings.
            let channel = match (inbox_read, recipient.runtime().ok(), outcome.as_str()) {
                (Some(_), _, _) => "telephone inbox",
                (None, Some(Runtime::Codex), "accepted") => "codex native queue",
                (None, Some(Runtime::Claude), "unconfirmed") => "claude native socket",
                _ => "unknown",
            };
            history.push(DeliveryRecord {
                id: id.parse()?,
                from: from.parse()?,
                sent_at: sent_at.try_into()?,
                outcome,
                channel,
                inbox_read,
            });
        }
        Ok(history)
    }

    /// Called inside the inbox transaction: cancellation or failed output must
    /// not consume the notice. Peek leaves it available for the next real read.
    pub(super) fn native_queue_notice(
        &self,
        recipient: &Address,
        peek: bool,
    ) -> Result<Option<String>> {
        if recipient.runtime().ok() != Some(Runtime::Codex) {
            return Ok(None);
        }
        let history = self.recent_deliveries(recipient)?;
        let native: Vec<_> = history
            .iter()
            .filter(|item| item.channel == "codex native queue")
            .collect();
        let Some(latest) = native.first() else {
            return Ok(None);
        };
        let id = latest.id.to_string();
        let seen: Option<String> = self
            .conn
            .query_row(
                "SELECT last_id FROM native_queue_notices WHERE recipient=?1",
                [recipient.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if !peek && seen.as_deref() == Some(&id) {
            return Ok(None);
        }
        if !peek {
            self.conn.execute(
                "INSERT INTO native_queue_notices(recipient,last_id) VALUES (?1,?2)
                 ON CONFLICT(recipient) DO UPDATE SET last_id=excluded.last_id",
                params![recipient.as_str(), id],
            )?;
        }
        Ok(Some(format!(
            "Native queue history: {} of the last {} journaled messages to this address were accepted by Codex's native queue and are not shown in telephone inbox/check_inbox (latest {}). Native read status is unknown; this is not a pending count. For future polling exchanges, ask senders to use --delivery inbox (MCP: delivery=\"inbox\"). Do not resend accepted messages blindly.",
            native.len(), history.len(), latest.id,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Envelope, Kind};

    fn accepted(store: &mut Store, recipient: &str) -> Envelope {
        let env = store
            .prepare(
                Envelope::new(
                    "claude:sender",
                    recipient,
                    Kind::Inform,
                    "private message body".into(),
                )
                .unwrap(),
                None,
            )
            .unwrap();
        store.outcome(env.id, "accepted").unwrap();
        env
    }

    #[test]
    fn notices_survive_cancelled_output_and_peeks_without_repeating_on_polls() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let recipient: Address = "codex:recipient".parse().unwrap();
        let env = accepted(&mut store, recipient.as_str());
        accepted(&mut store, "codex:other");
        drop(store);

        let interrupted = Store::open(root.path())
            .unwrap()
            .inbox(&recipient, false)
            .unwrap();
        assert!(interrupted.messages.is_empty());
        assert!(interrupted.notices[0].contains(&env.id.to_string()));
        assert!(interrupted.notices[0].contains("1 of the last 1"));
        drop(interrupted); // response was never flushed

        let peek = Store::open(root.path())
            .unwrap()
            .inbox(&recipient, true)
            .unwrap();
        assert_eq!(peek.notices.len(), 1);
        peek.acknowledge().unwrap();
        let received = Store::open(root.path())
            .unwrap()
            .inbox(&recipient, false)
            .unwrap();
        assert_eq!(received.notices.len(), 1);
        received.acknowledge().unwrap();
        let quiet = Store::open(root.path())
            .unwrap()
            .inbox(&recipient, false)
            .unwrap();
        assert!(quiet.notices.is_empty());
        quiet.acknowledge().unwrap();
        let peek = Store::open(root.path())
            .unwrap()
            .inbox(&recipient, true)
            .unwrap();
        assert_eq!(
            peek.notices.len(),
            1,
            "explicit peeks may repeat diagnostic history"
        );
        peek.acknowledge().unwrap();

        let mut store = Store::open(root.path()).unwrap();
        let newer = accepted(&mut store, recipient.as_str());
        let batch = store.inbox(&recipient, false).unwrap();
        assert!(batch.notices[0].contains(&newer.id.to_string()));
        assert!(batch.notices[0].contains("2 of the last 2"));
    }

    #[test]
    fn history_is_bounded_by_recipient_and_does_not_claim_uncertain_sends_were_accepted() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let recipient: Address = "codex:recipient".parse().unwrap();
        for _ in 0..HISTORY_LIMIT + 5 {
            accepted(&mut store, recipient.as_str());
        }
        let uncertain = accepted(&mut store, recipient.as_str());
        store.outcome(uncertain.id, "failed-or-uncertain").unwrap();
        for _ in 0..HISTORY_LIMIT + 5 {
            accepted(&mut store, "codex:other");
        }
        let history = store.recent_deliveries(&recipient).unwrap();
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].id, uncertain.id);
        assert_eq!(history[0].channel, "unknown");
        assert!(history[0].inbox_read.is_none());
        let batch = store.inbox(&recipient, false).unwrap();
        assert!(batch.notices[0].contains("19 of the last 20"));
        assert!(!batch.notices[0].contains("private message body"));
    }

    #[test]
    fn existing_journals_gain_history_without_replaying_native_messages() {
        let root = tempfile::tempdir().unwrap();
        let mut env = Envelope::new(
            "claude:sender",
            "codex:recipient",
            Kind::Inform,
            "old message".into(),
        )
        .unwrap();
        env.add_hop(env.from.clone()).unwrap();
        let db = rusqlite::Connection::open(root.path().join("messages.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE messages(id TEXT PRIMARY KEY,envelope TEXT NOT NULL,outcome TEXT NOT NULL);").unwrap();
        db.execute(
            "INSERT INTO messages VALUES (?1,?2,'accepted')",
            params![env.id.to_string(), serde_json::to_string(&env).unwrap()],
        )
        .unwrap();
        // A damaged unrelated record must not prevent creating the index.
        db.execute(
            "INSERT INTO messages VALUES ('broken','not JSON','unknown')",
            [],
        )
        .unwrap();
        drop(db);
        let store = Store::open(root.path()).unwrap();
        let history = store.recent_deliveries(&env.to).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].channel, "codex native queue");
        let batch = store.inbox(&env.to, false).unwrap();
        assert!(batch.messages.is_empty());
        assert_eq!(batch.notices.len(), 1);
    }

    #[test]
    fn damaged_history_does_not_break_an_empty_inbox() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let recipient: Address = "codex:recipient".parse().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO messages VALUES (?1,?2,'accepted')",
                params![Uuid::new_v4().to_string(), "{\"to\":\"codex:recipient\"}"],
            )
            .unwrap();
        let batch = store.inbox(&recipient, false).unwrap();
        assert!(batch.messages.is_empty());
        assert!(batch.notices.is_empty());
        assert!(batch.warnings[0].contains("Native queue history unavailable"));
        batch.acknowledge().unwrap();
    }
}
