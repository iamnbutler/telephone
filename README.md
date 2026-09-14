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

## Discovery and liveness

`telephone list` marks each agent `live` or `recent?`, because the two are not
the same and mixing them silently is worse than admitting the difference.

Claude Code sessions are `live`: each one keeps a registry entry while it runs,
and telephone checks both that the pid exists and that the process holding it
started when the session says it did. Without that second check a recycled pid
would accept a message meant for a session that exited hours ago.

Codex threads are `recent?`. Its on-disk records answer "when was this last
written to", not "is this running now" -- a thread that exited a second after
its last write is indistinguishable from one mid-turn. The app-server protocol
does answer the real question, but only the daemon that actually ran a thread
knows its status, and threads created in the ChatGPT desktop app are owned by a
daemon that exposes no way to ask. Starting a daemon on demand doesn't help: a
fresh one owns nothing and reports every thread as `notLoaded`, which means
"cannot tell", never "dead".

So telephone reports recency and says that's what it is. The investigation
behind that stop is written up at the top of `src/adapters/codex.rs`.

`telephone doctor` reports which agents are confirmed and why.

See `telephone --help`, the [demo](https://github.com/iamnbutler/telephone/blob/main/DEMO.md),
or [release instructions](https://github.com/iamnbutler/telephone/blob/main/RELEASING.md).
