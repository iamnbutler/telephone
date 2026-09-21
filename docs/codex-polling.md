# Polling during an active Codex turn

The native Codex queue can accept a message that the current turn will not read
until it ends. `telephone inbox` reads the polling inbox, not native queue history.
For a team already doing work and polling for replies, all senders can choose
the inbox per message:

```sh
export TELEPHONE_ADDR='your-actual-session-address'
telephone send <codex-address> --delivery inbox --kind request 'Please review the task and reply.'
telephone inbox
```

MCP callers set `delivery: "inbox"` on `send_message`; this works without restarting
or changing the environment of a shared MCP server. The option also works for
other runtimes whose recipient has agreed to poll. The default `delivery: "auto"`
uses the existing native/fallback preferences. Neither option grants permission
to start an exchange or changes how the sender receives replies.

For a session-wide Codex-only preference, the existing `TELEPHONE_CODEX_INBOX=1`
environment variable still works. Set it in each sending CLI/MCP process.
Discovery then lists Codex inbox transport; other runtimes keep their normal
routes. Without that variable or a per-call override, native delivery is preferred.

Inbox delivery does not wake an idle thread. Agree on polling first. Reply using
`--kind reply --reply-to <id>`, and do not acknowledge acknowledgments. An empty
inbox is not evidence of a failed send.

This selects a route before native delivery starts. It neither reads an earlier
native queue nor retries its messages. Do not resend an uncertain or accepted
native message merely to move it into the inbox.

## Seeing where a message went

Native send reports explicitly say that native delivery bypasses
`telephone inbox` / `check_inbox`. For an empty Codex inbox, Telephone checks the
last 20 journaled sends to that address and reports accepted native-queue entries.
This is local acceptance history: Telephone cannot tell whether Codex has read
them, whether they are still pending, or whether the recipient currently polls.
Failed or uncertain sends are not counted as accepted.

An ordinary inbox poll shows the notice once per newly observed native acceptance.
`inbox --peek` (MCP: `peek: true`) can repeat the diagnostic without consuming it.
CLI `inbox --json` keeps its JSON array on stdout and puts the notice on stderr;
MCP returns a `notices` array alongside `messages` and `warnings`. Failed or
cancelled response output does not consume the notice. No native message is
copied, marked read in Codex, or replayed.

```sh
telephone doctor <codex-address>
telephone doctor <codex-address> --json
```

This reports the currently preferred transport and the latest 20 journal entries
to that exact recipient, from all local senders, with outcomes and Telephone inbox
receipt state. It includes older entries written before these diagnostics were
added. Message bodies are omitted. History remains available after the recipient
disappears from discovery. The command does not probe native delivery or send
messages. `recent?` still means inferred liveness, not evidence of a polling inbox.
