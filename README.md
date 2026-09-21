# telephone

CLI and MCP server for messaging between local coding agents.

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

For a peer that polls `telephone inbox`, send with
`telephone send <address> <message> --delivery inbox` (MCP: `delivery: "inbox"`).
This chooses the inbox before any native attempt; it does not wake an idle
thread. The default uses native delivery when available. Native-queued messages
are not also copied into the polling inbox. Empty Codex inboxes report relevant
native-queue history once per newly observed acceptance; this is not a read or
pending receipt. Inspect a recipient with `telephone doctor <address>` (or
`--json`). See [Codex polling](docs/codex-polling.md), including the existing
`TELEPHONE_CODEX_INBOX=1` environment option.

To reply, use `--kind reply --reply-to <message-id>`. Replies must match a
message in the local journal; exchanges stop after eight hops.

`live` means the process and its start time were verified. `recent?` is only
an inference; Codex threads use this label. `telephone list --all` includes
quiet threads. Listings are capped; exact addresses use a separate lookup.
If discovery is incomplete, Telephone reports it and refuses short-name routing.

See `telephone --help` and [security and upgrade notes](SECURITY.md).

For long-running exchanges, the optional Claude Code `telephone context-hook`
reminds at 250k and 300k context tokens, once per threshold until context drops.
See [context management](docs/context-management.md) for hook setup and native
auto-compaction for unattended sessions.

## OpenCode, Zed, Delta and other harnesses

Register once per thread, keeping the returned address for later calls:

```sh
export TELEPHONE_ADDR="$(telephone register --runtime delta --name reviewer)"
telephone send claude:12345 --kind request "Review my plan and reply."
telephone inbox
telephone unregister
```

Use `opencode`, `zed`, `delta` or `generic` as the runtime. If you already set
`TELEPHONE_ADDR`, run `telephone register` before sending.

Over MCP, call `register_agent` with `runtime`; keep its returned `address` per
thread. Pass it as `from` to `send_message` and `address` to `check_inbox` or
`list_agents`. Call `unregister_agent` when done.

These routes use polling by default. OpenCode can opt into
[native delivery to an existing session](docs/opencode-native.md).
Registrations expire
after 24 hours without use. Sending and checking the inbox renew an active lease;
`telephone register` renews an expired one. Registration is not proof of liveness
or identity. All participants must use the same machine and OS account.
