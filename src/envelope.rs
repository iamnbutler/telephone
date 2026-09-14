//! Bounded, validated messages. Transport metadata cannot confer authority.
use crate::address::Address;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const MAX_HOPS: usize = 8;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_ENVELOPE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Inform,
    Request,
    Reply,
    Event,
}
impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "inform" => Some(Self::Inform),
            "request" => Some(Self::Request),
            "reply" => Some(Self::Reply),
            "event" => Some(Self::Event),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inform => "inform",
            Self::Request => "request",
            Self::Reply => "reply",
            Self::Event => "event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    #[default]
    // Legacy "peer" is read as untrusted, never as a distinct authority level.
    #[serde(alias = "peer")]
    Untrusted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub id: Uuid,
    pub conversation: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Uuid>,
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    pub to: Address,
    pub kind: Kind,
    pub body: String,
    // Older versions wrote null here. Non-null delivery controls are unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<()>,
    #[serde(default)]
    pub trust: Trust,
    #[serde(default)]
    pub hop_chain: Vec<Address>,
    pub sent_at: u64,
}

impl Envelope {
    /// Creates a draft. The store attaches journal ancestry before delivery.
    pub fn new(from: &str, to: &str, kind: Kind, body: String) -> Result<Self> {
        if body.is_empty() || body.len() > MAX_BODY_BYTES {
            bail!("message body must contain 1..={MAX_BODY_BYTES} bytes");
        }
        let id = Uuid::new_v4();
        Ok(Self {
            id,
            conversation: id,
            reply_to: None,
            from: from.parse()?,
            to: to.parse()?,
            from_name: None,
            kind,
            body,
            delivery: None,
            trust: Trust::Untrusted,
            hop_chain: Vec::new(),
            sent_at: now_millis(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        if self.body.is_empty() || self.body.len() > MAX_BODY_BYTES {
            bail!("invalid message body length");
        }
        if self.from == self.to {
            bail!("refusing a message addressed to its sender");
        }
        if self.id.is_nil()
            || self.conversation.is_nil()
            || self.reply_to.is_some_and(|id| id.is_nil())
        {
            bail!("nil message identifiers are not allowed");
        }
        if self
            .from_name
            .as_ref()
            .is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control))
        {
            bail!("sender name must be at most 256 bytes without control characters");
        }
        if self.kind == Kind::Reply && self.reply_to.is_none() {
            bail!("a reply requires reply_to");
        }
        if self.hop_chain.is_empty()
            || self.hop_chain.len() > MAX_HOPS
            || self.hop_chain.last() != Some(&self.from)
        {
            bail!("invalid hop chain");
        }
        Ok(())
    }
    pub fn add_hop(&mut self, address: Address) -> Result<()> {
        if self.hop_chain.len() >= MAX_HOPS {
            bail!("hop limit ({MAX_HOPS}) exceeded; start a new conversation only with user direction");
        }
        self.hop_chain.push(address);
        Ok(())
    }
}
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
pub fn now_millis() -> u64 {
    // A pre-epoch clock is treated as zero, never used as identity proof.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_limits_and_requires_reply_ancestry() {
        assert!(Envelope::new("a:1", "b:2", Kind::Inform, "x".repeat(MAX_BODY_BYTES + 1)).is_err());
        let mut e = Envelope::new("a:1", "b:2", Kind::Reply, "hello".into()).unwrap();
        e.add_hop(e.from.clone()).unwrap();
        assert!(e.validate().is_err());
        e.reply_to = Some(Uuid::new_v4());
        assert!(e.validate().is_ok());
        assert_eq!(e.trust, Trust::Untrusted);
        assert_eq!(
            serde_json::from_str::<Trust>("\"peer\"").unwrap(),
            Trust::Untrusted
        );
    }
}
