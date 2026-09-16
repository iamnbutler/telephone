# Polling during an active Codex turn

The native Codex queue can accept a message that the current turn will not read
until it ends. `telephone inbox` reads the polling inbox, not native queue history.
For a team already doing work and polling for replies, all senders can choose:

```sh
export TELEPHONE_CODEX_INBOX=1
export TELEPHONE_ADDR='your-actual-session-address'
telephone send <codex-address> --kind request 'Please review the task and reply.'
telephone inbox
```

Use the same environment for Telephone MCP processes when that is the sender.
This option affects only Codex recipients; Claude and registered runtime routes
keep their normal behavior. Discovery reports Codex inbox transport while the
option is enabled. With the variable unset, native queue delivery stays preferred.

Inbox delivery does not wake an idle thread. Agree on polling first. Reply using
`--kind reply --reply-to <id>`, and do not acknowledge acknowledgments. An empty
inbox is not evidence of a failed send.

This selects a route before native delivery starts. It neither reads an earlier
native queue nor retries its messages. Do not resend an uncertain or accepted
native message merely to move it into the inbox.

The Jeeves team encountered this distinction on 16 September 2026: two completed
handshake replies were accepted by the native queue while the active leader's
polling inbox remained empty. The explicit route avoids ending useful work solely
to receive a team response.
