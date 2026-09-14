//! The universal adapter: telephone as an MCP server over stdio.
//!
//! This is the piece that makes telephone work for runtimes with no peer
//! protocol of their own. Nearly every serious agent speaks MCP, so an MCP
//! server is a de facto universal channel: outbound works natively and
//! immediately, and inbound works by the agent pulling its inbox.
//!
//! The catch, stated plainly: MCP is pull-only. An agent sees its messages
//! when it decides to call `check_inbox`, not when they arrive. That is fine
//! for coordination and useless for interrupts, which is why runtimes with a
//! real push channel (Claude Code) get a native adapter instead.

use crate::envelope::{Envelope, Kind};
use crate::{identity, send};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };

        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = req.get("id").cloned();

        // Notifications have no id and take no response.
        if id.is_none() {
            continue;
        }

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "telephone", "version": env!("CARGO_PKG_VERSION") }
            })),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => call_tool(&req),
            _ => Err(anyhow::anyhow!("unknown method: {method}")),
        };

        let response = match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(e) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32000, "message": e.to_string() }
            }),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_agents",
            "description": "List other coding agents currently running on this \
                machine that can be messaged, across runtimes (Claude Code, Codex, \
                and any agent registered with telephone).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "send_message",
            "description": "Send a message to another agent. Use list_agents first \
                to find a valid address. Messages are delivered natively when the \
                target runtime supports it, otherwise queued in the target's inbox.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Agent address or name, e.g. 'claude:83487' or 'nexthub-87'" },
                    "body": { "type": "string", "description": "The message text" },
                    "kind": {
                        "type": "string",
                        "enum": ["inform", "request", "reply", "event"],
                        "description": "'request' expects a reply; 'inform' and 'event' do not"
                    },
                    "reply_to": { "type": "string", "description": "Message id this answers, if any" }
                },
                "required": ["to", "body"]
            }
        },
        {
            "name": "check_inbox",
            "description": "Read and clear messages other agents have sent to you. \
                Call this when you want to see if anyone has been in touch; nothing \
                will interrupt you otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peek": {
                        "type": "boolean",
                        "description": "Read without clearing. Defaults to false."
                    }
                }
            }
        }
    ])
}

fn text_result(body: String) -> Value {
    json!({ "content": [{ "type": "text", "text": body }] })
}

fn call_tool(req: &Value) -> Result<Value> {
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "list_agents" => {
            let me = identity::whoami();
            let registry = crate::default_registry()?;
            let (agents, _) = registry.discover();
            let listed: Vec<Value> = agents
                .iter()
                .filter(|a| Some(&a.addr) != me.addr.as_ref())
                .map(|a| {
                    json!({
                        "address": a.addr,
                        "name": a.name,
                        "runtime": a.runtime,
                        "status": a.status.as_str(),
                        "cwd": a.cwd.as_ref().map(|c| c.display().to_string()),
                        "transport": a.transports.first().map(|t| t.label()),
                    })
                })
                .collect();
            Ok(text_result(serde_json::to_string_pretty(&json!({
                "you": me.addr,
                "agents": listed
            }))?))
        }

        "send_message" => {
            let to = args
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("'to' is required"))?;
            let body = args
                .get("body")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("'body' is required"))?;
            let kind = args
                .get("kind")
                .and_then(|v| v.as_str())
                .and_then(Kind::parse)
                .unwrap_or(Kind::Inform);
            let reply_to = args.get("reply_to").and_then(|v| v.as_str()).map(String::from);

            let outcome = send::send(to, body, kind, reply_to)?;
            Ok(text_result(outcome))
        }

        "check_inbox" => {
            let me = identity::whoami();
            let addr = me
                .addr
                .ok_or_else(|| anyhow::anyhow!("cannot determine which agent you are"))?;
            let peek = args.get("peek").and_then(|v| v.as_bool()).unwrap_or(false);

            let messages: Vec<Envelope> = if peek {
                crate::inbox::peek(&addr)?.into_iter().map(|p| p.env).collect()
            } else {
                crate::inbox::drain(&addr)?
            };

            if messages.is_empty() {
                return Ok(text_result("No new messages.".into()));
            }
            let rendered: Vec<String> = messages
                .iter()
                .map(crate::adapters::format_for_delivery)
                .collect();
            Ok(text_result(format!(
                "{} message(s):\n\n{}",
                messages.len(),
                rendered.join("\n\n====================\n\n")
            )))
        }

        _ => anyhow::bail!("unknown tool: {name}"),
    }
}
