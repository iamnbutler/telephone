# Live Codex ↔ Claude Code exchange

Verified locally on September 14, 2026 with `codex-cli 0.154.0` and
Claude Code `2.1.270`.

| Endpoint | Telephone address |
| --- | --- |
| This Codex session | `codex:01a0a060-dc66-7f72-b40b-42e4154a7f2b` |
| Claude Code, named `telephone-demo` | `claude:24207` |

1. Codex sent a request using `target/debug/telephone send`.
   Telephone delivered it through Claude's native Unix socket; it appeared as
   a new user turn in Claude's transcript.
2. Claude called the Telephone MCP `list_agents` tool and verified its own
   identity. It then called `send_message` with `kind: reply` and the request's
   message ID as `reply_to`.
3. Telephone used `codex queue` to enqueue that reply for the originating Codex
   thread. A read-only inspection of that thread's queue confirmed the exact
   sender, reply ID, nonce, and text.
4. Codex sent an informational acknowledgment back through Claude's socket.
   Claude was instructed not to reply to acknowledgments.
5. After the originating Codex turn finished, the queued Claude reply arrived
   automatically as a new Codex turn, with the same reply ID and nonce.
   This confirmed the full round trip through both runtimes' agent loops.

Request ID: `3bb018be-73a9-4173-a5a5-fd68c85fa687`

Reply ID: `df1b066d-595e-41b0-b8db-cfd20dc17216`

Claude's reply:

> Verified: I am claude:24207. Nonce: switchboard-914-r2. Two agents traded careful signals and finished together.

The Codex reply was first verified in its native queue while the originating
turn was still running, then received as a subsequent agent turn after that
turn completed. In this demo, delivery waited for the turn boundary.

## Bugs found and fixed during the demo

- A sandbox-denied process probe hid live Claude sessions. Discovery now
  treats permission denial as potentially live, instead of declaring it dead.
- Claude's MCP process inherited the launching Codex thread ID but did not
  receive `CLAUDE_PID`. Telephone now checks its direct parent's Claude
  session record before accepting an inherited Codex identity. The first
  attempted reply was safely rejected as a self-send; after rebuilding and
  reconnecting MCP, Claude identified itself correctly and sent the reply.

## Repeat it

From a Codex shell: `bash scripts/demo-claude.sh`.

From a separate terminal, pass the desired Codex address explicitly:

```sh
bash scripts/demo-claude.sh codex:<thread-id>
```

See [README.md](README.md#codex--claude-code-demo) for the request command,
prerequisites, and queue/inbox behavior. The Claude PID changes each launch;
use the new address reported by `list_agents`.
