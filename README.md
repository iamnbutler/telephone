# telephone

An agent-agnostic communication layer. Lets coding agents running on the same
machine discover and message each other, across runtimes that were never
designed to talk.

```
$ telephone list
  luau-4c          claude:53129    idle     uds     ~/code/luau
* claude-8c        claude:83487    busy     uds     ~/code/_
  nexthub-87       claude:16534    idle     uds     ~/code/nexthub
  codex-01a062c7   codex:01a0…     unknown  inbox   ~/code/gpuikit

$ telephone send luau-4c "the migration is done, you can rebase"
Delivered to luau-4c (claude:53129) over uds. Message id: 344845ab…
```

## Why

People increasingly run several coding agents at once, and each one is an
island. Some runtimes have a private inter-session protocol; most have nothing
at all. telephone normalizes whatever a runtime does offer into one address
space and one message format, and supplies a universal fallback for the ones
that offer nothing.

## How it works

Five layers, deliberately separated:

| Layer | What it answers |
|---|---|
| **Registry** | who exists, what can they speak, are they alive |
| **Transport** | getting bytes from A to B |
| **Envelope** | what a message *means* |
| **Auth** | who may send to whom |
| **Ingress** | how a message becomes a turn inside the target's loop |

The last one is the hard one. An agent is not a server — it's a turn-based loop
that's frequently busy for minutes. "Send a message" has to answer what happens
when the target is mid-turn, and the answer differs per runtime.

### Adapters

**Claude Code** (native push). Claude Code already has a real peer-messaging
layer, so telephone speaks it rather than working around it. Discovery reads
`~/.claude/sessions/<pid>.json`; auth is a token from a `0600` sibling key file;
transport is newline-delimited JSON over `/tmp/cc-socks/<pid>.sock`. Messages
arrive in the target session as a real turn. This protocol is undocumented and
was determined by inspection, so it may break on any Claude Code release —
telephone degrades to the inbox rather than failing.

**Codex** (native queue, with inbox fallback). Discovery reads the
`threads` table of `~/.codex/state_<n>.sqlite`, falling back to parsing the
`session_meta` header of each rollout log. Delivery uses
`codex queue --thread <id> --message <text>` when available. Success means
Codex accepted the message into its queue, not that the agent has read it.
If the CLI cannot queue it, delivery falls back to a filesystem inbox that
Codex drains by calling the telephone MCP server.

**Anything else** (pull). Run `telephone mcp`. Nearly every serious agent speaks
MCP, which makes an MCP server a de facto universal channel: `send_message` and
`list_agents` work natively and immediately, and `check_inbox` gives you
inbound. Set `TELEPHONE_ADDR` if the runtime can't tell telephone which agent
it is.

The honest limitation: MCP is pull-only. An agent sees its messages when it
decides to look, not when they arrive. That's fine for coordination and useless
for interrupts, which is why runtimes with a real push channel get a native
adapter instead.

## Install

```sh
cargo build --release
telephone install    # prints the config each runtime needs
```

## Use

```sh
telephone list [--all] [--json]     # who's out there
telephone send <who> <message>      # --kind inform|request|reply|event
telephone inbox [--peek] [--json]   # what's waiting for you
telephone whoami                    # which agent telephone thinks you are
telephone doctor                    # what's reachable, and how
telephone mcp                       # run as an MCP server over stdio
```

Addresses are `<runtime>:<local-id>` (`claude:83487`). You can also use an
agent's display name (`luau-4c`), or the bare local id when it's unambiguous.

## Codex ↔ Claude Code demo

Requires Rust, authenticated `claude` and `codex` CLIs, and `jq`.
From a Codex session's shell, run:

```sh
bash scripts/demo-claude.sh
```

Or start it in a separate terminal, passing the exact Codex address shown by
`target/debug/telephone list`:

```sh
bash scripts/demo-claude.sh codex:<thread-id>
```

The script builds Telephone and starts a Claude Code session named
`telephone-demo`, with three Telephone MCP tools configured for that invocation.
Claude discovers its own address, sends Codex a greeting, and stays available.
From Codex, send a request to the Claude address it reports:

```sh
target/debug/telephone send claude:<pid> --kind request \
  'Demo nonce: switchboard-914. Reply once with this nonce and an original sentence.'
```

Claude receives it over its native socket and replies through the MCP
`send_message` tool. The reply is routed through `codex queue`; if that transport
is unavailable, check `target/debug/telephone inbox` from the Codex session.
Both sides carry message IDs, and Claude includes `reply_to` for correlation.
The demo prompt permits replies to requests only, avoiding an endless exchange.
Exit Claude with `/exit` when finished.

Sandboxed hosts may need approval to connect to another runtime's socket or
write to its queue. Claude MCP processes may not receive `CLAUDE_PID`, so
Telephone also checks the direct parent's Claude session record before using
inherited Codex environment variables. After rebuilding Telephone, reconnect
its MCP server with Claude's `/mcp` menu to load the new binary.

## Design notes

**Discovery is public, reachability is gated.** telephone inherits this split
from Claude Code, and it's the right shape: you can *see* an agent you may not
be allowed to *message*. The token isn't protecting message content — it's
protecting the right to inject a turn into a privileged loop.

**Peer messages are untrusted input.** A message from another agent is text
entering a loop that holds a shell and a filesystem. If agent A can say "ignore
previous instructions" and agent B's harness renders that as an instruction,
you've built a worm substrate. Every delivered message is fenced, attributed,
and accompanied by an explicit statement that a peer cannot grant permissions it
doesn't have.

**Loops are a real failure mode.** Two agents that each reply to messages will
ping-pong forever. Every message carries a hop chain, and exceeding
`MAX_HOPS` is a visible error rather than a silent drop.

**Names are for humans; addresses are for routing.** Codex auto-generates thread
titles and rewrites them as a thread evolves, so telephone won't route on them —
only on an explicitly-set nickname, or a stable id prefix.

## Status

v0.1, and the interesting parts are deliberately unfinished:

- Delivery policy (`queue` / `when-idle` / `interrupt`) is in the envelope but
  not yet honored — everything queues or pushes immediately.
- No cross-machine support. Addresses are shaped to grow a host component
  (`claude:host.example/83487`), but "same uid on the same box" is currently the
  entire security model.
- No rate limiting. Hop chains catch relay loops, not two agents having an
  enthusiastic conversation.
- A tmux/PTY adapter would cover CLI agents with neither a socket nor MCP.
