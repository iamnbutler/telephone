//! telephone -- an agent-agnostic communication layer.
//!
//! Coding agents increasingly run several at a time on one machine, and each
//! one is an island. Some runtimes have a private inter-session protocol;
//! most have nothing. telephone normalizes whatever a runtime does offer into
//! one address space and one message format, and supplies a universal fallback
//! for the runtimes that offer nothing at all.

#![deny(unused_must_use)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::let_underscore_must_use)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::dbg_macro
    )
)]

mod adapters;
mod address;
mod discovery;
mod envelope;
mod guidance;
mod identity;
mod inbox;
mod mcp;
mod private_fs;
mod proc;
mod process;
mod registry;
mod runtime;
mod send;
mod store;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use envelope::Kind;
use registry::{Adapter, Registry};
use std::io::Write;

/// Diagnostics for the paths that silently fall back, gated behind
/// `TELEPHONE_DEBUG`. Fallbacks that are invisible are indistinguishable from
/// bugs, and this is the cheapest way to tell them apart.
pub fn debug(msg: impl FnOnce() -> String) {
    if std::env::var("TELEPHONE_DEBUG").is_ok() {
        diagnostic("debug", &msg());
    }
}

pub fn warn(message: impl std::fmt::Display) {
    diagnostic("warning", &message.to_string());
}

fn diagnostic(level: &str, message: &str) {
    let mut stderr = std::io::stderr().lock();
    if writeln!(stderr, "{level}: {}", serde_json::json!(message)).is_err() {
        // stderr is the last-resort diagnostic channel. Do not panic during
        // cleanup or retry a side effect because its warning could not print.
    }
}

fn console_text(value: &str) -> String {
    value.escape_debug().to_string()
}

/// Every adapter telephone knows about, in preference order.
pub fn default_registry() -> Result<Registry> {
    registry_at(&inbox::root()?)
}

fn registry_at(root: &std::path::Path) -> Result<Registry> {
    let mut adapters: Vec<Box<dyn Adapter>> = vec![
        Box::new(adapters::claude_code::ClaudeCode::new()?),
        Box::new(adapters::codex::Codex::new()?),
    ];
    adapters.extend(adapters::registered_adapters(root));
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
    /// Register a pollable inbox for an OpenCode, Zed, Delta or other thread
    Register {
        /// Generate a unique address for this runtime
        #[arg(long, value_enum, conflicts_with = "address")]
        runtime: Option<runtime::InboxRuntime>,
        /// Register or renew this exact address (defaults to TELEPHONE_ADDR)
        #[arg(long)]
        address: Option<String>,
        /// Optional display name (not a unique identifier)
        #[arg(long)]
        name: Option<String>,
        /// Include lease timestamps and polling instructions as JSON
        #[arg(long)]
        json: bool,
    },

    /// Stop routing new messages to a registered inbox; preserve its messages
    Unregister {
        /// Defaults to your current identity
        address: Option<String>,
    },

    /// Opt into native delivery to an existing, trusted local OpenCode server
    BindOpencode {
        #[arg(long)]
        address: String,
        /// Canonical http://127.0.0.1:PORT or http://[::1]:PORT; no remote hosts
        #[arg(long)]
        endpoint: String,
        /// Actual OpenCode ses_ ID (not the Telephone address UUID)
        #[arg(long)]
        session: String,
        /// Existing session's absolute directory
        #[arg(long)]
        directory: String,
        /// Owner-only JSON file containing username and password; never pass a token in argv
        #[arg(long)]
        credentials: std::path::PathBuf,
    },

    /// Remove a native binding while retaining the polling inbox
    UnbindOpencode { address: String },

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

    /// Report discovered agents, configured routes and liveness evidence
    Doctor,

    /// Print the configuration needed to wire telephone into each runtime
    Install,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            diagnostic("error", &format!("{e:#}"));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Register {
            runtime,
            address,
            name,
            json,
        } => {
            let mut address = address;
            let mut name = name;
            if runtime.is_none() && address.is_none() {
                let me = identity::whoami()?;
                address = me.addr.map(String::from);
                name = name.or(me.name);
            }
            let address = store::registrations::registration_address(runtime, address.as_deref())?;
            let registration = store::Store::open(&inbox::root()?)?.register(
                &address,
                name.as_deref(),
                envelope::now_millis(),
            )?;
            let output = if json {
                serde_json::to_string(&guidance::RegistrationReport::new(&registration))?
            } else {
                registration.address.to_string()
            };
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{output}").context("writing registration; it may already exist")?;
            stdout
                .flush()
                .context("flushing registration; it may already exist")?;
            if !json {
                // Keep stdout address-only for command substitution. Advice is
                // best-effort: a broken stderr must not imply registration failed.
                let mut stderr = std::io::stderr().lock();
                if writeln!(
                    stderr,
                    "{}",
                    guidance::Polling::new(&registration.address).text(None)
                )
                .is_err()
                {
                    // The address was already flushed; do not retry registration.
                }
            }
            Ok(())
        }
        Command::Unregister { address } => {
            let address = match address {
                Some(a) => a,
                None => identity::whoami()?
                    .addr
                    .context("set TELEPHONE_ADDR or supply an address")?
                    .into(),
            };
            store::Store::open(&inbox::root()?)?.unregister(&address.parse()?)
        }
        Command::BindOpencode {
            address,
            endpoint,
            session,
            directory,
            credentials,
        } => {
            let address = address.parse()?;
            adapters::opencode::address(&address)?;
            let root = inbox::root()?;
            store::Store::open(&root)?.registered_identity(&address, envelope::now_millis())?;
            let route = adapters::opencode::Route::try_from(adapters::opencode::RouteConfig {
                endpoint,
                session_id: session,
                directory,
                credentials,
            })?;
            route.verify()?;
            store::Store::open(&root)?.bind_opencode(&address, &route)?;
            writeln!(std::io::stdout().lock(), "Bound {address} to its existing OpenCode session. Native prompts can start model work. HTTP acceptance is not a read receipt; inbox fallback still requires polling.")
                .context("writing binding result; the binding may already exist")
        }
        Command::UnbindOpencode { address } => {
            store::Store::open(&inbox::root()?)?.unbind_opencode(&address.parse()?)
        }
        Command::List { json, all } => cmd_list(json, all),
        Command::Send {
            to,
            body,
            kind,
            reply_to,
        } => {
            let kind = Kind::parse(&kind)
                .ok_or_else(|| anyhow::anyhow!("kind must be inform, request, reply or event"))?;
            let report = send::send(&to, &body, kind, reply_to)?;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{report}").context(
                "writing send report; delivery may already have occurred, do not retry blindly",
            )?;
            stdout
                .flush()
                .context("flushing send report; delivery may already have occurred")?;
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
    let me = identity::whoami()?;
    let registry = default_registry()?;
    let discovery = registry.discover_with(all);
    let agents = &discovery.agents;
    let warnings = &discovery.warnings;
    if !discovery.complete {
        warn("discovery is incomplete; results are partial");
    }
    if discovery.warnings_omitted > 0 {
        warn(format!(
            "{} additional discovery warnings omitted",
            discovery.warnings_omitted
        ));
    }

    if json {
        for warning in warnings {
            warn(warning);
        }
        let out: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "address": a.addr(),
                    "name": a.name,
                    "runtime": a.runtime(),
                    "status": a.status.as_str(),
                    "liveness": a.liveness.as_str(),
                    "cwd": a.cwd.as_ref().map(|c| c.display().to_string()),
                    "transport": a.transports.first().map(|t| t.label()),
                    "self": Some(a.addr()) == me.addr.as_ref().map(address::Address::as_str),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if agents.is_empty() {
        println!("No agents found.");
    }

    // Size the columns to what's actually present: Codex addresses are a
    // runtime prefix plus a uuid, and a fixed width either truncates them or
    // wastes half the terminal when no Codex threads are around.
    let name_width = agents
        .iter()
        .map(|a| console_text(&a.name).len())
        .max()
        .unwrap_or(0);
    let addr_width = agents.iter().map(|a| a.addr().len()).max().unwrap_or(0);

    for a in agents {
        let is_me = Some(a.addr()) == me.addr.as_ref().map(address::Address::as_str);
        let marker = if is_me { "*" } else { " " };
        let transport = a.transports.first().map(|t| t.label()).unwrap_or("none");
        let cwd = a
            .cwd
            .as_ref()
            .map(|c| console_text(&c.display().to_string()))
            .unwrap_or_default();
        println!(
            "{marker} {:<name_width$}  {:<addr_width$}  {:<7}  {:<7}  {:<5}  {}",
            console_text(&a.name),
            a.addr(),
            a.status.as_str(),
            a.liveness.as_str(),
            transport,
            cwd
        );
    }

    let inferred = agents
        .iter()
        .any(|a| a.liveness == registry::Liveness::Inferred);
    if me.addr.is_some() || inferred {
        println!();
    }
    if me.addr.is_some() {
        println!("* = you");
    }
    if inferred {
        println!("recent? = was active recently, but nothing could confirm it is still running");
    }
    for w in warnings {
        warn(w);
    }
    Ok(())
}

fn cmd_inbox(peek: bool, json: bool) -> Result<()> {
    let me = identity::whoami()?;
    let Some(addr) = me.addr else {
        anyhow::bail!(
            "can't tell which agent you are; set TELEPHONE_ADDR to your address \
             (see `telephone whoami`)"
        );
    };

    let batch = inbox::read(&addr, peek)?;
    for warning in &batch.warnings {
        warn(warning);
    }
    let rendered = if json {
        serde_json::to_string_pretty(&batch.messages)?
    } else if batch.messages.is_empty() {
        "No new messages.".into()
    } else {
        batch
            .messages
            .iter()
            .map(adapters::format_for_delivery)
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{rendered}").context("writing inbox; messages remain unread on failure")?;
    stdout
        .flush()
        .context("flushing inbox; messages remain unread on failure")?;
    batch.acknowledge()
}

fn cmd_whoami() -> Result<()> {
    let me = identity::whoami()?;
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
    let me = identity::whoami()?;
    println!(
        "you:        {}",
        me.addr
            .as_ref()
            .map(address::Address::as_str)
            .unwrap_or("unknown")
    );
    println!("via:        {}", me.source);
    println!("inbox root: {}", inbox::root()?.display());

    let registry = default_registry()?;
    println!("\nadapters:");
    let mut any_inferred = false;
    for adapter in &registry.adapters {
        match adapter.discover() {
            Ok(found) => {
                for warning in &found.warnings {
                    warn(warning);
                }
                if !found.complete {
                    warn(format!("{} discovery is incomplete", adapter.runtime()));
                }
                if found.warnings_omitted > 0 {
                    warn(format!(
                        "{} additional discovery warnings omitted",
                        found.warnings_omitted
                    ));
                }
                let found = found.agents;
                let native_routes = found
                    .iter()
                    .filter(|a| {
                        !matches!(
                            a.transports.first(),
                            None | Some(registry::Transport::Inbox)
                        )
                    })
                    .count();
                let confirmed = found
                    .iter()
                    .filter(|a| a.liveness == registry::Liveness::Verified)
                    .count();
                if confirmed < found.len() {
                    any_inferred = true;
                }
                println!(
                    "  {:<8} ok       {} agent(s), {native_routes} native route(s) configured, \
                     {confirmed} confirmed live",
                    adapter.runtime(),
                    found.len()
                );
            }
            Err(e) => println!(
                "  {:<8} error    {}",
                adapter.runtime(),
                console_text(&e.to_string())
            ),
        }
    }

    println!("\nA configured native route is not proof of reachability or receipt.");

    if any_inferred {
        // Explain the gap rather than leaving "recent?" to be guessed at.
        println!(
            "\nsome agents could not be confirmed live.\n  \
             Claude Code sessions are confirmed by pid and process start time.\n  \
             Codex threads use recency, which cannot tell a\n  \
             running thread from one that exited just after its last write."
        );
    }

    if let Some(addr) = &me.addr {
        let batch = inbox::read(addr, true)?;
        let waiting = batch.messages.len();
        println!("\ninbox:      {waiting} message(s) waiting");
        batch.acknowledge()?;
    }
    Ok(())
}

/// Prints setup instructions rather than editing anyone's configuration.
///
/// Silently rewriting an agent's config file is exactly the kind of surprise
/// this tool should not be in the business of causing.
fn cmd_install() -> Result<()> {
    let exe = std::env::current_exe()
        .context("locating telephone executable for setup")?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("executable path is not valid Unicode"))?;

    println!("Claude Code -- run this:\n");
    println!(
        "  claude mcp add telephone -s user -- {} mcp\n",
        address::shell_quote(&exe)
    );
    println!("Codex -- add this to ~/.codex/config.toml:\n");
    println!("  [mcp_servers.telephone]");
    println!("  command = {}", serde_json::to_string(&exe)?);
    println!("  args = [\"mcp\"]\n");
    println!("OpenCode, Zed, Delta and other local harnesses: run this as a stdio MCP server:\n");
    println!("  {} mcp\n", address::shell_quote(&exe));
    println!("Call register_agent with runtime opencode, zed, delta or generic once per thread.");
    println!(
        "Keep its returned address; pass it as from to send_message and address to check_inbox."
    );
    println!("These inbox routes require polling; they do not wake a thread.");
    println!("CLI: export TELEPHONE_ADDR=\"$(telephone register --runtime delta)\"");
    println!("Leases expire after 24 hours without a send or inbox check. Use unregister when finished.\n");
    println!("If it can't tell you which agent it is, set TELEPHONE_ADDR in its");
    println!("environment (see `telephone whoami`).");
    Ok(())
}
