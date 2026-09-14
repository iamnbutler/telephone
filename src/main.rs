//! telephone -- an agent-agnostic communication layer.
//!
//! Coding agents increasingly run several at a time on one machine, and each
//! one is an island. Some runtimes have a private inter-session protocol;
//! most have nothing. telephone normalizes whatever a runtime does offer into
//! one address space and one message format, and supplies a universal fallback
//! for the runtimes that offer nothing at all.

mod adapters;
mod envelope;
mod identity;
mod inbox;
mod mcp;
mod proc;
mod registry;
mod send;

use anyhow::Result;
use clap::{Parser, Subcommand};
use envelope::Kind;
use registry::{Adapter, Registry};

/// Diagnostics for the paths that silently fall back, gated behind
/// `TELEPHONE_DEBUG`. Fallbacks that are invisible are indistinguishable from
/// bugs, and this is the cheapest way to tell them apart.
pub fn debug(msg: impl FnOnce() -> String) {
    if std::env::var("TELEPHONE_DEBUG").is_ok() {
        eprintln!("debug: {}", msg());
    }
}

/// Every adapter telephone knows about, in preference order.
pub fn default_registry() -> Result<Registry> {
    let mut adapters: Vec<Box<dyn Adapter>> = Vec::new();
    if let Ok(a) = adapters::claude_code::ClaudeCode::new() {
        adapters.push(Box::new(a));
    }
    if let Ok(a) = adapters::codex::Codex::new() {
        adapters.push(Box::new(a));
    }
    Ok(Registry::new(adapters))
}

#[derive(Parser)]
#[command(
    name = "telephone",
    about = "Send messages between coding agents, whatever runtime they're on",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List messageable agents on this machine
    #[command(alias = "ls")]
    List {
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
        /// Include agents that have gone quiet, not just currently-live ones
        #[arg(long)]
        all: bool,
    },

    /// Send a message to another agent
    Send {
        /// Target address or name, e.g. `claude:83487` or `nexthub-87`
        to: String,
        /// The message
        body: String,
        /// What you want the receiver to do about it
        #[arg(long, default_value = "inform")]
        kind: String,
        /// Message id this is a reply to
        #[arg(long)]
        reply_to: Option<String>,
    },

    /// Read messages sent to you
    Inbox {
        /// Read without clearing
        #[arg(long)]
        peek: bool,
        /// Emit JSON instead of rendered text
        #[arg(long)]
        json: bool,
    },

    /// Report which agent telephone thinks you are
    Whoami,

    /// Run as an MCP server over stdio (the universal adapter)
    Mcp,

    /// Check what telephone can see and reach
    Doctor,

    /// Print the configuration needed to wire telephone into each runtime
    Install,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { json, all } => cmd_list(json, all),
        Command::Send { to, body, kind, reply_to } => {
            let kind = Kind::parse(&kind)
                .ok_or_else(|| anyhow::anyhow!("kind must be inform, request, reply or event"))?;
            println!("{}", send::send(&to, &body, kind, reply_to)?);
            Ok(())
        }
        Command::Inbox { peek, json } => cmd_inbox(peek, json),
        Command::Whoami => cmd_whoami(),
        Command::Mcp => mcp::serve(),
        Command::Doctor => cmd_doctor(),
        Command::Install => cmd_install(),
    }
}

fn cmd_list(json: bool, all: bool) -> Result<()> {
    if all {
        // A week is "anything you might plausibly still care about".
        // Effectively no cutoff.
        std::env::set_var("TELEPHONE_WINDOW_MINS", "5256000");
    }
    let me = identity::whoami();
    let registry = default_registry()?;
    let (agents, warnings) = registry.discover();

    if json {
        let out: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "address": a.addr,
                    "name": a.name,
                    "runtime": a.runtime,
                    "status": a.status.as_str(),
                    "cwd": a.cwd.as_ref().map(|c| c.display().to_string()),
                    "transport": a.transports.first().map(|t| t.label()),
                    "self": Some(&a.addr) == me.addr.as_ref(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if agents.is_empty() {
        println!("No agents found.");
    }
    for a in &agents {
        let is_me = Some(&a.addr) == me.addr.as_ref();
        let marker = if is_me { "*" } else { " " };
        let transport = a.transports.first().map(|t| t.label()).unwrap_or("none");
        let cwd = a
            .cwd
            .as_ref()
            .map(|c| c.display().to_string())
            .unwrap_or_default();
        println!(
            "{marker} {:<16} {:<28} {:<8} {:<6} {}",
            a.name,
            a.addr,
            a.status.as_str(),
            transport,
            cwd
        );
    }
    if me.addr.is_some() {
        println!("\n* = you");
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok(())
}

fn cmd_inbox(peek: bool, json: bool) -> Result<()> {
    let me = identity::whoami();
    let Some(addr) = me.addr else {
        anyhow::bail!(
            "can't tell which agent you are; set TELEPHONE_ADDR to your address \
             (see `telephone whoami`)"
        );
    };

    let messages: Vec<envelope::Envelope> = if peek {
        inbox::peek(&addr)?.into_iter().map(|p| p.env).collect()
    } else {
        inbox::drain(&addr)?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&messages)?);
        return Ok(());
    }
    if messages.is_empty() {
        println!("No new messages.");
        return Ok(());
    }
    for m in &messages {
        println!("{}\n", m.render());
    }
    Ok(())
}

fn cmd_whoami() -> Result<()> {
    let me = identity::whoami();
    match &me.addr {
        Some(addr) => {
            println!("address: {addr}");
            if let Some(n) = &me.name {
                println!("name:    {n}");
            }
            if let Some(r) = me.runtime {
                println!("runtime: {r}");
            }
            println!("source:  {}", me.source);
        }
        None => {
            println!("Unknown -- telephone can't tell which agent this is.");
            println!("Set TELEPHONE_ADDR (e.g. `codex:my-session`) to give yourself an address.");
        }
    }
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    let me = identity::whoami();
    println!("you:        {}", me.addr.clone().unwrap_or_else(|| "unknown".into()));
    println!("via:        {}", me.source);
    println!("inbox root: {}", inbox::root()?.display());

    let registry = default_registry()?;
    println!("\nadapters:");
    for adapter in &registry.adapters {
        match adapter.discover() {
            Ok(found) => {
                let native = found
                    .iter()
                    .filter(|a| {
                        !matches!(a.transports.first(), None | Some(registry::Transport::Inbox))
                    })
                    .count();
                println!(
                    "  {:<8} ok       {} agent(s), {native} reachable natively",
                    adapter.runtime(),
                    found.len()
                );
            }
            Err(e) => println!("  {:<8} error    {e}", adapter.runtime()),
        }
    }

    if let Some(addr) = &me.addr {
        let waiting = inbox::peek(addr)?.len();
        println!("\ninbox:      {waiting} message(s) waiting");
    }
    Ok(())
}

/// Prints setup instructions rather than editing anyone's configuration.
///
/// Silently rewriting an agent's config file is exactly the kind of surprise
/// this tool should not be in the business of causing.
fn cmd_install() -> Result<()> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "telephone".into());

    println!("Claude Code -- run this:\n");
    println!("  claude mcp add telephone -s user -- {exe} mcp\n");
    println!("Codex -- add this to ~/.codex/config.toml:\n");
    println!("  [mcp_servers.telephone]");
    println!("  command = \"{exe}\"");
    println!("  args = [\"mcp\"]\n");
    println!("Any other runtime that speaks MCP over stdio: run `{exe} mcp`.");
    println!("If it can't tell you which agent it is, set TELEPHONE_ADDR in its");
    println!("environment (see `telephone whoami`).");
    Ok(())
}
