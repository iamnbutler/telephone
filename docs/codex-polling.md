# Automatic routing and native queues

`telephone inbox` / `check_inbox` reads Telephone's inbox, not a runtime's native
queue. A native Codex message may wait until an active turn ends. Telephone 0.2
uses fresh receiver evidence to avoid that mismatch for active exchanges.

## Route selection

Each inbox check, including a peek, advertises polling for **15 seconds**. Sending
`kind: request` advertises the sender's return path for the same period, so a
fast reply can reach its inbox before the first poll. Continue polling every two
seconds while waiting, up to 30 seconds or the user's deadline.

For each new request or reply independently:

1. Explicit `--delivery inbox` / `delivery: "inbox"` selects the Telephone inbox.
2. Fresh recipient evidence selects the inbox automatically.
3. Otherwise use the native route, with inbox fallback only before native sending
   has started.

Codex evidence is keyed to the native thread ID. Claude evidence is tied to its
PID **and session ID**, with a verified process start, so a replacement session
does not inherit another session's polling window. Unknown evidence is not
polling. Clock rollback invalidates evidence from the future.

An inbox does not wake idle agents. If a receiver stops polling just after its
advertisement, messages already queued there stay there until its next check.
Expired evidence changes routing for new messages only. It never triggers a
resend, migration or duplicate copy of an accepted or uncertain native message.

CLI and MCP share receiving state. A long-running sender MCP process observes
route changes without restarting or modifying its environment. All peers must
use the same Telephone state directory.

## Seeing where a message went

Native send results distinguish acceptance from unconfirmed writes and explain
that native delivery bypasses Telephone's inbox. An empty Codex inbox checks the
last 20 journaled sends to that address and reports native queue acceptance once
per newly observed acceptance. Read status remains unknown. A peek may repeat
the notice. Failed or uncertain sends are not counted as accepted.

CLI `inbox --json` keeps an array on stdout and emits notices on stderr. MCP
returns `messages`, `warnings` and `notices`. Failed or cancelled response output
does not consume the notice. Telephone never marks native messages read.

```sh
telephone doctor <address>
telephone doctor <address> --json
```

Doctor reports preferred and native transports, fresh polling timestamps and
expiry, and the latest 20 journal entries to that recipient from
all local senders. It omits message bodies. Native read status stays unknown;
`inbox_read` only records a flushed Telephone response. History remains available
after the native session disappears. Doctor does not send, probe a transport or
refresh a polling window.
