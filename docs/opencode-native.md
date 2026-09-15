# OpenCode native delivery

Opt-in delivery to an existing OpenCode session over its
[HTTP API](https://opencode.ai/docs/server/). Unbound addresses still use polling.
This does not configure or restart OpenCode for you.

## Set up

Use a local OpenCode server you control, bound to loopback with
`OPENCODE_SERVER_PASSWORD` set. Keep its endpoint, existing `ses_...` session ID
and exact session directory. Starting another `opencode serve` does not attach
to an already-running TUI; bind the server that owns the intended session.

Store its HTTP credentials in an owner-only, single-link regular JSON file
outside the repository. Use an absolute path. The username defaults to
`opencode` in OpenCode unless you configured `OPENCODE_SERVER_USERNAME`.

```json
{"username":"opencode","password":"your-server-password"}
```

Restrict that file to mode `0600`. Never commit it or put the password in
Telephone's command-line arguments. Telephone records only its path in the
private journal and reads it again when sending.

Register a Telephone address for the thread if it does not have one. Bind it:

```sh
telephone bind-opencode \
  --address opencode:YOUR-REGISTERED-ADDRESS \
  --endpoint http://127.0.0.1:4096 \
  --session ses_YOUR_EXISTING_SESSION \
  --directory /absolute/session/directory \
  --credentials /absolute/private/opencode-auth.json
```

Substitute actual values; the example is not a discoverable session. Binding
makes read-only requests to check authentication and the exact session/directory.
It does not create a session, send a test prompt or claim the thread is alive.
The Telephone address and OpenCode session ID are different identifiers.

Other agents keep sending to the same Telephone address. Native delivery can
start model work. Telephone copies the last recorded agent/model selection
(including its variant when present); omitting those fields would let OpenCode
choose defaults instead. An empty thread falls back to its inbox until it has
a recorded turn. Unsent UI selections are not visible through this API, and the
read followed by POST is not atomic with concurrent user changes. Unbind before
switching a thread to a different mode/model if that distinction matters.
Telephone supplies peer text, not system instructions or tool-permission overrides.

To return to polling without removing the inbox:

```sh
telephone unbind-opencode opencode:YOUR-REGISTERED-ADDRESS
```

Unregistering or expiration followed by re-registration also removes the native
binding. Unbinding does not cancel a request already in flight. No binding tool
is exposed through MCP; endpoint configuration is an explicit local CLI action.

## Outcomes and limits

- `http` in discovery means configured, not reachable or verified live.
- `204` means OpenCode accepted asynchronous work; processing can fail later.
  It is not a read receipt. Check the OpenCode thread for errors.
- Failed read-only preflight can fall back to the registered inbox, with a reason.
  Missing/unsafe credentials or malformed bindings fail visibly instead.
- After POST starts, any transport error or unexpected status is uncertain.
  Telephone does not retry or queue a duplicate. Native IDs are assigned by
  OpenCode; the Telephone message ID remains in the peer text for correlation.
- HTTP calls have three-second deadlines (one second to connect), response
  headers are limited to 16 KiB and read-only response bodies to 256 KiB.
  Redirects, proxies and DNS hostnames are disabled. TLS/remote hosts are unsupported.

The operator must trust the configured loopback service. HTTP Basic authenticates
the caller, **not the server**; a hostile process replacing the listener can steal
the credential. Do not use this route on a machine with untrusted local users or
services. It does not expand Telephone's same-user trust model into safe network
messaging, and does not authenticate claimed peer identities. Rotate the server
credential and rebind if ownership changes. See [security limits](../SECURITY.md).

## Compatibility check

Tested against OpenCode 1.4.3. The opt-in test starts a separate authenticated
server with isolated HOME/XDG directories, no configured providers, plugins or
MCP servers. It checks session binding, insertion with `noReply`, production
delivery preserving a nondefault agent, and fallback after shutdown. The
production-path check deliberately names a nonexistent provider: the message
is stored but model execution cannot proceed. No model calls are made:

```sh
TELEPHONE_TEST_OPENCODE=/absolute/path/to/opencode \
  cargo test --locked real_opencode_accepts -- --ignored --nocapture
```

This verifies transport compatibility, not a model-driven wake-up. Before release,
test one harmless request in a disposable real client thread without polling,
then a request while that thread is busy. Do not test against unrelated live work.
