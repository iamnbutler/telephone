# telephone

CLI and MCP server for messaging between local Claude Code and Codex sessions.

**Unsafe, experimental software.** Peer messages can lead agents to run commands
or change files. Use at your own discretion, on your own machine.

## Install

```sh
cargo install telephone --locked
telephone install
```

`telephone install` prints MCP setup instructions for your agents.

[Prebuilt binaries](https://github.com/iamnbutler/telephone/releases/latest) are
available for macOS and Linux (ARM64 and x86-64). Extract the archive and put
`telephone` on your PATH. macOS downloads are Developer ID signed and notarized.

## Use

```sh
telephone list
telephone send claude:12345 --kind request "Review my latest plan and reply."
telephone inbox
```

Use an address from `telephone list`, not the example PID above.

Or ask your agent:

> Use Telephone to find the Claude Code session working in example-app. Read
> its latest plan, check it against the code, and send feedback. Include
> instructions for replying.

Replace `example-app` with your project directory. The agents read the history
and code themselves; Telephone handles discovery and delivery.

Native delivery depends on runtime internals. If unavailable before sending,
messages go to an inbox the receiving agent must check. A socket write is not
a receipt; Telephone reports uncertainty and does not retry it automatically.

To reply, use `--kind reply --reply-to <message-id>`. Replies must match a
message in the local journal; exchanges stop after eight hops.

`live` means the process and its start time were verified. `recent?` is only
an inference; Codex threads use this label. `telephone list --all` includes
quiet threads. Listings are capped; exact addresses use a separate lookup.
If discovery is incomplete, Telephone reports it and refuses short-name routing.

See `telephone --help` and [security and upgrade notes](SECURITY.md).
