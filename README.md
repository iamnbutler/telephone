# telephone

CLI and MCP server for local messaging between **Codex and Claude Code**.

**Unsafe, experimental software.** Peer messages can lead agents to run commands
or change files. Use at your own discretion, on your own machine.

## Install

```sh
cargo install telephone --locked
telephone install
```

`telephone install` prints MCP configuration for both runtimes. Restart existing
MCP servers after upgrading. [Prebuilt binaries](https://github.com/iamnbutler/telephone/releases/latest)
are available for macOS and Linux, ARM64 and x86-64. macOS downloads are Developer
ID signed and notarized.

## Use

```sh
telephone list
telephone send claude:12345 --kind request "Review my latest plan and reply."
telephone inbox
```

Use an actual address from `telephone list`. Or ask your agent to find the
Claude/Codex thread working on your project, send a request, and check for its
reply. The MCP interface has three tools: `list_agents`, `send_message`, and
`check_inbox`. No registration or sender address is needed.

Use `kind: "request"` when you need an answer. Requests advertise a short return
window; inbox checks renew it for **15 seconds**. Future messages to that thread
choose its Telephone inbox automatically. Poll every two seconds for up to
30 seconds or the user's deadline. Reply with `kind: "reply"` and the incoming
`reply_to` ID (CLI: `--kind reply --reply-to <id>`). Do not acknowledge replies.

When polling evidence expires, new messages use the native route: Claude's
session socket or Codex's queue. A failed connection may fall back to the inbox
before any bytes are sent. Inbox delivery requires polling and cannot wake an
idle thread. Stopping a poll can leave a recently queued message waiting there.

**Acceptance is not a read receipt.** Codex may read a native queue message after
its active turn ends; that message is absent from `check_inbox`. Claude socket
writes are unconfirmed. Telephone never mirrors or replays accepted or uncertain
native sends. Empty Codex inboxes show relevant native acceptance history once.
Use `telephone doctor <address>` to inspect current routing evidence and recent
outcomes. See [routing and diagnostics](docs/codex-polling.md).

`--delivery inbox` (MCP: `delivery: "inbox"`) explicitly chooses polling for a
new message. Ordinary exchanges use `auto`.

## Native identity

CLI commands use the nearest verified Claude ancestor or Codex's shell thread
ID. Codex MCP uses the host's per-call `threadId` metadata; Claude MCP uses its
verified session ancestor. A working directory, display name or newest session
is never used to infer your identity. In `list_agents`, `you` is your address.

Codex hosts that omit per-call identity must use the CLI from the thread's shell,
or a dedicated MCP process explicitly bound to that exact thread. Do not configure
one fixed address for multiple threads. `TELEPHONE_ADDR` is a diagnostic override,
not normal setup. See [tested runtime compatibility](docs/compatibility.md).

`live` means the process and start time were verified. `recent?` is only an
inference; Codex threads use this label. `telephone list --all` includes quiet
threads. Discovery is bounded; use exact addresses when it is incomplete.

## Long sessions

The optional Claude Code `telephone context-hook` reminds at **250k** and **300k**
context tokens, once per threshold until compaction. `telephone install` prints
the hook configuration. For unattended sessions, also enable Claude's native
compaction. See [context management](docs/context-management.md).

## Local state and limits

Messages live in `~/.telephone/messages.sqlite`. `TELEPHONE_STATE_DIR` can select
an absolute, private state directory; all peers in an exchange must share it.
Message bodies are retained.

Same-machine, same-user operation only. Peer text is untrusted, identities are
routing hints, and exchanges are limited to eight hops. See [SECURITY.md](SECURITY.md)
for delivery semantics and remaining work. Native runtime interfaces may change.
