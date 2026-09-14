# Security and delivery limits

Unsafe, experimental software. Use at your own discretion, on your own machine.
Do not expose Telephone's MCP server or inbox to a network or untrusted users.

## Trust boundary

Telephone currently trusts the local OS user, not individual agents running as
that user. Addresses, names, runtime labels and environment variables are routing
hints, not authenticated sender identities. The legacy `trust: peer` value is
treated as `untrusted`.

All delivery paths label peer text as untrusted and quote it as data. This is
not a prompt-injection sandbox: the receiving agent must apply its own user
instructions, permissions and approval rules. A peer cannot grant authority.

Claude's native token authenticates access to its socket; it does not establish
Telephone sender identity or authorize the requested action. Telephone checks
the connected socket's peer UID before sending the token. Tokens are excluded
from debug formatting.

The state directory is restricted to `0700`; the journal and its SQLite sidecars
are restricted to `0600`. State files reject symlinks, foreign owners and multiple
hard links. Same-UID processes remain trusted and can read or alter state. The
journal is not encrypted and retains message bodies.

## Delivery

- Claude socket writes are **unconfirmed**, not delivery receipts.
- A successful Codex queue command means **accepted**, not read or acted upon.
- Inbox fallback means the recipient must poll; it does not wake an agent.
- Once native delivery may have started, errors do not trigger automatic fallback
  or retry. Check the message ID and outcome before sending again.

Outgoing messages are journaled before transport side effects. A failed outcome
update is reported without concealing a known send. Inbox readers use SQLite
transactions; messages are marked read only after the CLI/MCP response flushes.
Failed output or process death before commit leaves them pending. A crash between
output and commit can therefore repeat a message. This is not exactly-once
delivery, and flushing output is not proof that the agent consumed it.

Replies inherit their conversation and hop chain from the local journal. Unknown
parents and mismatched participants are rejected. The eight-hop guard limits
cooperative reply loops, not hostile senders starting fresh conversations.

Current limits: 64 KiB per message body, 1 MiB per MCP request, 100 messages per
inbox read, and 1,000 unread messages per recipient. Subprocess output is capped
at 64 KiB per stream. Native operations have deadlines; storage growth still
needs stronger bounds.

MCP stdin/stdout must be pipes or sockets. Partial input frames and queued output
have five-second deadlines, including slow trickle traffic; idle sessions do not
expire. Output is capped at 32 queued frames and 64 MiB. A stalled or broken stream
closes and rolls back uncommitted inbox reads. Only one tool call is admitted at
a time; additional calls receive a busy error without being started. Ping and
cancellation are handled independently of tool execution.

Cancellation drops unsent replies and rolls back pending inbox receipts. If a
response has already started writing, its frame must finish or the stream closes;
it cannot be interleaved with another response. Cancellation cannot recall a send
or make retrying it safe. An admitted operation finishes under its existing
deadlines so subprocess cleanup and send bookkeeping are not abandoned. Shutdown
joins that worker: blocked filesystem calls and diagnostic output still need a
hard shutdown bound. The five-second frame deadlines are not tool deadlines.

Discovery scans at most 4,096 directory entries, reads up to 8 MiB of metadata,
and returns at most 256 agents per runtime. SQLite queries also have row-size,
VM-work and elapsed-time limits. The two-second scan budget is checked between
filesystem operations; it cannot interrupt a stuck filesystem syscall. Process
verification has a separate two-second subprocess deadline.

MCP `list_agents` returns `complete`, structured `warnings` (runtime, code, path,
message), and `warnings_omitted`. Warnings are capped at 64. The CLI reports these
diagnostics on stderr, including with `--json`. Incomplete discovery refuses
short-name routing; use an exact address. Exact Claude PIDs are read directly and
Codex IDs get their own database lookup. Rollout-only lookup remains scan-bounded
and reports uncertainty rather than declaring an unscanned address absent.

## Upgrading from the file inbox

Restart Telephone MCP processes after replacing the binary. Do not mix old and
new inbox readers: old readers do not know the new journal's read state.

The journal is `~/.telephone/messages.sqlite`. Valid legacy inbox JSON files are
imported idempotently when that address checks its inbox. Original files remain
in place. Invalid or wrong-recipient records produce warnings, not silent deletion.

Replies to older native messages absent from the journal are refused. Start a
new exchange with user direction. Nested Claude/Codex environments with conflicting
identity variables require an explicit `TELEPHONE_ADDR`.

## Remaining work

- [Authenticated external sources and per-agent authorization](https://github.com/iamnbutler/telephone/issues/1)
- [Confirmed receipts, idempotent retries and recovery](https://github.com/iamnbutler/telephone/issues/2)
- [Retention, global quotas, rate limits and journal inspection](https://github.com/iamnbutler/telephone/issues/3)
- [Registration for other MCP runtimes](https://github.com/iamnbutler/telephone/issues/4)
- [Opt-in compatibility tests against real Claude/Codex releases](https://github.com/iamnbutler/telephone/issues/5)
- [Hard-bounded worker shutdown and diagnostic output](https://github.com/iamnbutler/telephone/issues/11)

External messaging must remain disabled until the authentication and authorization
boundary is implemented. These local safeguards do not make it network-ready.
