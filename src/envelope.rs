//! The canonical message format every adapter translates to and from.
//!
//! The envelope is deliberately small. Anything a specific runtime needs that
//! doesn't fit here belongs in that runtime's adapter, not in the wire format.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many times a message may be relayed before we assume it's a loop.
///
/// Two agents that each reply to messages will ping-pong forever. Claude Code
/// carries a `hop-chain` for the same reason; we carry the full chain rather
/// than a bare counter so the path is visible when something does go wrong.
pub const MAX_HOPS: usize = 8;

/// What the sender wants the receiver to do about this message.
///
/// This is the FIPA-ACL "performative" idea, trimmed to the four that earn
/// their keep. The distinction matters because it's the difference between
/// "here's a fact, do what you like with it" and "do this and report back".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A statement. No response expected.
    Inform,
    /// A task or question. A reply is expected.
    Request,
    /// Answers an earlier `Request`; `reply_to` names it.
    Reply,
    /// Something happened (a build finished, a session went idle).
    Event,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "inform" => Some(Kind::Inform),
            "request" => Some(Kind::Request),
            "reply" => Some(Kind::Reply),
            "event" => Some(Kind::Event),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Inform => "inform",
            Kind::Request => "request",
            Kind::Reply => "reply",
            Kind::Event => "event",
        }
    }
}

/// How much the receiver should trust the body.
///
/// This exists because a peer message is untrusted text entering a loop that
/// holds a shell and a filesystem. The receiving adapter is responsible for
/// rendering anything below `Peer` inside a clearly-fenced untrusted block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Same uid, same machine, proved it could read our key material.
    Peer,
    /// Everything else.
    Untrusted,
}

/// When delivery is acceptable to the receiver.
///
/// An agent is not a server: it is a turn-based loop that is often busy for
/// minutes. "Send a message" has to answer what happens when the target is
/// mid-turn, and the honest answer differs per message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    /// Leave it in the inbox; the agent picks it up on its own schedule.
    Queue,
    /// Ask the runtime to surface it as soon as the current turn ends.
    WhenIdle,
    /// Interrupt the current turn. Runtimes that can't will fall back to
    /// `WhenIdle` and say so.
    Interrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    /// Threads messages together. A fresh `Request` starts a new conversation;
    /// replies inherit it.
    pub conversation: String,
    /// The `id` of the message this answers, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,

    /// Canonical address of the sender, e.g. `claude:83487`.
    pub from: String,
    /// Human-facing name, for display only. Never route on this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    pub to: String,

    pub kind: Kind,
    pub body: String,

    #[serde(default)]
    pub delivery: Option<Delivery>,
    pub trust: Trust,
    /// Every address this message has passed through, oldest first.
    #[serde(default)]
    pub hop_chain: Vec<String>,
    pub sent_at: u64,
}

impl Envelope {
    pub fn new(from: String, to: String, kind: Kind, body: String) -> Self {
        let id = new_id();
        Envelope {
            conversation: id.clone(),
            id,
            reply_to: None,
            from,
            from_name: None,
            to,
            kind,
            body,
            delivery: None,
            trust: Trust::Peer,
            hop_chain: Vec::new(),
            sent_at: now_millis(),
        }
    }

    /// Records that this message passed through `addr`.
    ///
    /// Returns an error rather than silently dropping, so a relay loop shows up
    /// as a visible failure instead of messages quietly vanishing.
    pub fn add_hop(&mut self, addr: &str) -> anyhow::Result<()> {
        if self.hop_chain.len() >= MAX_HOPS {
            anyhow::bail!(
                "hop limit ({}) exceeded, likely a relay loop: {}",
                MAX_HOPS,
                self.hop_chain.join(" -> ")
            );
        }
        self.hop_chain.push(addr.to_string());
        Ok(())
    }

    /// Renders the message for a human (or an agent) reading their inbox.
    pub fn render(&self) -> String {
        let who = self.from_name.as_deref().unwrap_or(&self.from);
        let mut out = format!("[{}] from {} ({})", self.kind.as_str(), who, self.from);
        if let Some(rt) = &self.reply_to {
            out.push_str(&format!(" re: {rt}"));
        }
        out.push('\n');
        out.push_str(&self.body);
        out
    }
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Envelope {
        Envelope::new("a:1".into(), "b:2".into(), Kind::Inform, "hi".into())
    }

    #[test]
    fn a_new_message_starts_its_own_conversation() {
        let e = env();
        assert_eq!(e.id, e.conversation);
        assert!(e.hop_chain.is_empty());
    }

    #[test]
    fn hops_accumulate_in_order() {
        let mut e = env();
        e.add_hop("a:1").unwrap();
        e.add_hop("b:2").unwrap();
        assert_eq!(e.hop_chain, vec!["a:1", "b:2"]);
    }

    #[test]
    fn the_loop_guard_trips_rather_than_dropping_silently() {
        let mut e = env();
        for i in 0..MAX_HOPS {
            e.add_hop(&format!("r:{i}")).expect("under the limit");
        }
        let err = e.add_hop("r:last").expect_err("should refuse past the limit");
        // The path matters more than the count when diagnosing a loop.
        assert!(err.to_string().contains("r:0 -> r:1"));
    }

    #[test]
    fn kinds_survive_a_round_trip_through_their_wire_names() {
        for k in [Kind::Inform, Kind::Request, Kind::Reply, Kind::Event] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kind::parse("nonsense"), None);
    }
}
