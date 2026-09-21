//! Recipient diagnostics use local discovery and journal metadata, never delivery.
use crate::{
    address::Address,
    inbox,
    registry::Transport,
    store::{diagnostics::HISTORY_LIMIT, Store},
};
use anyhow::{Context, Result};
use serde_json::json;

pub fn recipient(address: &Address, as_json: bool) -> Result<()> {
    let registry = crate::default_registry()?;
    let runtime = address.runtime()?;
    let adapter = registry
        .adapters
        .iter()
        .find(|a| a.runtime() == runtime)
        .context("unsupported recipient runtime")?;
    let report = adapter.find_exact(address)?;
    let agent = report.agents.iter().find(|a| a.address() == address);
    let preferred = agent.and_then(|a| a.transports.first());
    let route_note = match preferred {
        Some(Transport::CodexQueue) => "Native Codex queue bypasses telephone inbox/check_inbox; active turns may read it later. Use --delivery inbox (MCP: delivery=\"inbox\") for future polling exchanges.",
        Some(Transport::Inbox) => "Telephone inbox requires recipient polling; it does not wake an idle thread.",
        Some(_) => "Native route is configured, not probed; fallback may require recipient polling.",
        None => "No current route discovered. Journal history can outlive a session or registration.",
    };
    let history = Store::open(&inbox::root()?)?.recent_deliveries(address)?;
    let receipt_note = "History covers sends to this recipient from all local senders. Native read status and the recipient's current polling behavior are unknown. inbox_read only records a Telephone inbox response. Do not resend accepted or uncertain messages blindly.";
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "recipient": address,
                "preferred_transport": preferred.map(Transport::label),
                "liveness": agent.map(|a| a.liveness.as_str()),
                "discovery_complete": report.complete,
                "warnings": report.warnings,
                "warnings_omitted": report.warnings_omitted,
                "route_note": route_note,
                "history_limit": HISTORY_LIMIT,
                "recent_deliveries": history,
                "receipt_note": receipt_note,
            }))?
        );
    } else {
        println!("recipient: {address}");
        println!(
            "preferred transport: {} (not probed)",
            preferred.map(Transport::label).unwrap_or("unknown")
        );
        println!(
            "liveness: {}",
            agent.map(|a| a.liveness.as_str()).unwrap_or("unknown")
        );
        println!("{route_note}");
        for warning in &report.warnings {
            crate::warn(warning);
        }
        if !report.complete {
            crate::warn("recipient discovery is incomplete");
        }
        if report.warnings_omitted > 0 {
            crate::warn(format!(
                "{} discovery warnings omitted",
                report.warnings_omitted
            ));
        }
        println!("\nRecent deliveries (newest first, up to {HISTORY_LIMIT}):");
        if history.is_empty() {
            println!("No journaled sends to this recipient.");
        }
        for item in history {
            let inbox = match item.inbox_read {
                Some(true) => "inbox response written",
                Some(false) => "inbox unread",
                None => "no inbox copy",
            };
            println!(
                "{}  from={}  outcome={}  channel={}  {}",
                item.id,
                item.from,
                crate::console_text(&item.outcome),
                item.channel,
                inbox
            );
        }
        println!("\n{receipt_note}");
    }
    Ok(())
}
