//! Validate, journal, then deliver. An uncertain send must not be retried blindly.
use crate::{
    envelope::{Envelope, Kind},
    identity,
    registry::Delivered,
    store::Store,
};
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    #[default]
    Auto,
    Inbox,
}

pub fn send(
    to: &str,
    body: &str,
    kind: Kind,
    reply_to: Option<String>,
    delivery: Delivery,
) -> Result<String> {
    let reply_to = reply_to
        .map(|id| uuid::Uuid::parse_str(&id).context("reply_to must be a UUID"))
        .transpose()?;
    send_as(
        &crate::inbox::root()?,
        identity::whoami()?,
        to,
        body,
        kind,
        reply_to,
        delivery,
    )
}

pub fn send_as(
    root: &std::path::Path,
    me: identity::Me,
    to: &str,
    body: &str,
    kind: Kind,
    reply_to: Option<uuid::Uuid>,
    delivery: Delivery,
) -> Result<String> {
    if kind == Kind::Reply && reply_to.is_none() {
        anyhow::bail!("a reply requires --reply-to");
    }
    let from = me
        .addr
        .context("cannot identify sender; use a per-thread native host (telephone install) or set TELEPHONE_ADDR explicitly")?;
    let registry = crate::default_registry()?;
    let mut discovery = registry.discover();
    let target = registry.resolve(&mut discovery, to).with_context(|| {
        format!(
            "recipient lookup: discovery complete={}, warnings={} ({} omitted)",
            discovery.complete,
            serde_json::json!(discovery.warnings),
            discovery.warnings_omitted
        )
    })?;
    let mut draft = Envelope::new(from.as_str(), target.addr(), kind, body.to_owned())?;
    draft.from_name = me.name;
    let adapter = registry
        .adapter_for(&target)
        .context("no adapter for target runtime")?;
    let mut store = Store::open(root)?;
    let env = store.prepare(draft, reply_to)?;
    let receiving = if env.kind == Kind::Request {
        crate::receiving::advertise(root, &env.from)?;
        format!("\n{}", crate::guidance::WAITING)
    } else {
        String::new()
    };
    let polling = target
        .session_key
        .as_deref()
        .map(|key| store.receiving(&env.to, key, crate::envelope::now_millis()))
        .transpose()?
        .flatten();
    let delivered = if delivery == Delivery::Inbox || polling.is_some() {
        let deposited = store.deposit(&env.to, &env);
        deposited.map(|()| Delivered::Queued {
            path: store.path.clone(),
            note: if delivery == Delivery::Inbox {
                "Inbox delivery requested; no native wake-up attempted."
            } else {
                "Recipient recently checked its inbox or requested a reply; selected polling automatically."
            }
            .into(),
        })
    } else {
        adapter.deliver(&target, &env)
    };
    let outcome = match delivered {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Err(journal) = store.outcome(env.id, "failed-or-uncertain") {
                return Err(e).context(format!("delivery failed or is uncertain; journal update also failed: {journal:#}; message {}{receiving}", env.id));
            }
            return Err(e).context(format!(
                "message {}; inspect before retrying{receiving}",
                env.id
            ));
        }
    };
    let (state, description) = match outcome {
        Delivered::Unconfirmed { via } => (
            "unconfirmed",
            format!(
                "Written over {via}; acceptance and reading are unconfirmed. Native delivery does not populate telephone inbox/check_inbox. Do not retry blindly."
            ),
        ),
        Delivered::Accepted { via: "codex queue" } => (
            "accepted",
            "Accepted by codex queue; not a read receipt. This bypasses telephone inbox/check_inbox and may wait until the recipient's active turn ends. Fresh recipient inbox checks select polling for future messages. Do not resend this accepted message blindly.".into(),
        ),
        Delivered::Accepted { via } => (
            "accepted",
            format!("Accepted by {via}; not a read receipt. Native delivery does not populate telephone inbox/check_inbox."),
        ),
        Delivered::Queued { path, note } => (
            "queued",
            format!(
                "Queued in Telephone inbox. {note}\nJournal: {}",
                path.display()
            ),
        ),
    };
    let mut report = format!(
        "{description}\nTo: {} ({})\nMessage id: {}",
        serde_json::json!(target.name),
        target.addr(),
        env.id
    );
    // Delivery has already happened: a bookkeeping error must not masquerade as
    // a safe-to-retry failure. Preserve the known outcome in the response.
    if let Err(e) = store.outcome(env.id, state) {
        report.push_str(&format!(
            "\nWARNING: outcome journal update failed: {e:#}; do not resend blindly."
        ));
    }
    for warning in discovery.warnings {
        report.push_str(&format!("\nwarning: {}", serde_json::json!(warning)));
    }
    if discovery.warnings_omitted > 0 {
        report.push_str(&format!(
            "\n{} additional discovery warnings omitted.",
            discovery.warnings_omitted
        ));
    }
    report.push_str(&receiving);
    Ok(report)
}
