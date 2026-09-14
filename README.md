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
`telephone` on your PATH. macOS downloads are not notarized.

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

Native delivery depends on runtime internals. If unavailable, messages go to a
filesystem inbox the receiving agent must check.

See `telephone --help`, the [demo](https://github.com/iamnbutler/telephone/blob/main/DEMO.md),
or [release instructions](https://github.com/iamnbutler/telephone/blob/main/RELEASING.md).
