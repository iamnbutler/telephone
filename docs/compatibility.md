# Runtime compatibility

Telephone 0.2 supports **Codex and Claude Code on the same machine and OS account**.
The native interfaces are version-sensitive; a successful write or queue command
is not a read receipt.

## Verified release baseline

Real disposable sessions were tested on macOS 27.0 / ARM64 with:

- **Codex CLI 0.155.1**, using the app-server's real thread and MCP interfaces.
- **Claude Code 2.1.278**, using persistent stream-JSON sessions and native sockets.

The opt-in suite passed these exchanges without registration or sender routing
configuration:

| Exchange | Verified behavior |
| --- | --- |
| Codex → Codex | Exact per-call MCP identity for concurrent threads in the same directory; MCP request, CLI inbox read, correlated MCP reply |
| Codex → Claude | Native write to an idle Claude session; Claude model reads the nonce and replies into Codex's polling inbox |
| Claude → Codex | Claude model sends a request through MCP; Codex replies; Claude reads the returned nonce through MCP |
| Claude → Claude | Native request between separate Claude sessions; model sends a correlated reply into the sender's inbox |

The same suite verifies native Codex queue acceptance during an admitted active
turn, absence of a duplicate
polling copy, the empty-inbox channel notice, actual 15-second expiry, return to
native routing without restarting the sender, and Claude identity despite an
inherited outer Codex ID. Codex native acceptance is deliberately **not** asserted
as a model read or immediate wake-up. Test driver CLI reads use exact owned test
addresses; native MCP identity is supplied by the runtimes.

Normal Rust tests separately exercise native socket failures after bytes are
sent (no duplicate fallback), replaced Claude session keys/PID reuse, stale and
future-clock observations, CLI/MCP identity consistency, shared-process metadata
switching, missing host metadata, malformed input, cancellation/output rollback,
reply ancestry and storage integrity. These run on all four release targets:
macOS ARM64/x86-64 and Linux ARM64/x86-64 musl. The authenticated runtime suite
has been run on macOS ARM64, not on every binary target.

## Supported host contracts

**Codex MCP:** the host must supply a UUID `threadId` in each `tools/call` request's
`_meta`. Telephone uses that native thread for the call, allowing a shared host
to switch threads without an app-wide fixed identity. An inherited outer
`CODEX_THREAD_ID` is not accepted as MCP identity. Hosts lacking metadata must
use Telephone CLI inside the native thread, or a dedicated MCP process bound
explicitly to that known thread. A fixed override conflicting with metadata
fails. Codex CLI's shell identity uses `CODEX_THREAD_ID`.

**Claude MCP/CLI:** Telephone walks a bounded ancestor chain and verifies the
session record against its process start. A Claude host without a matching
record fails instead of inheriting an outer Codex identity. Polling evidence
includes the session ID, so resumed/replaced sessions cannot inherit a previous
session's advertisement. Native sockets still require the runtime's own inbound
policy and private peer token; Telephone does not change them.

Names, working directories and recent activity are discovery hints, never
caller identity. Local routing hints are not authenticated sender identities.
Native Codex queue and Telephone inbox are different channels; see
[routing](codex-polling.md). A recent thread may be closed. An inbox cannot wake
an idle peer and an expired polling window does not move already queued messages.

## Run the opt-in suite

Authenticate both runtimes normally, build Telephone, then explicitly opt in:

```sh
cargo build --locked
python3 scripts/compatibility.py --run --binary target/debug/telephone \
  --report /tmp/telephone-compatibility.json
```

This starts two owned Codex threads and two owned Claude sessions in a temporary
working directory. It makes authenticated model calls and may incur usage costs.
It uses a private Telephone journal and isolated Codex state. It copies Codex
auth locally into the private temporary directory for the run and removes that
copy on exit. Claude retains its existing auth helper and inbound policy, with
unrelated settings, hooks and plugins disabled. No global config is edited.
The runtime's existing inbound policy must permit the test; the suite does not
enable it. It never selects an existing user session as a recipient.

JSON results record exact versions, platform and passed checks. Private runtime
stderr logs and test state are retained at the reported temporary path for
investigation; remove that directory when finished. Model calls are not part of
ordinary CI. Rerun the suite when changing a native adapter or qualifying a new
runtime version. See [OpenAI's MCP configuration](https://developers.openai.com/codex/mcp)
for host setup; runtime metadata behavior above is verified against the installed
version, not a promise about every Codex host.
