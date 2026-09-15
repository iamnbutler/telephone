# OpenCode native delivery

Experimental delivery to an existing OpenCode session through its
[HTTP API](https://opencode.ai/docs/server/). Unbound addresses use polling.
MCP registration and native binding are separate.

## Requirements

- A registered `opencode:` address for the receiving thread.
- The owning server's authenticated loopback endpoint, existing `ses_...` ID
  and exact session directory. Starting another server does not attach to it.
- A credential file outside the repository, owned by the current user, with
  mode `0600`. Symlinks and multiple hard links are rejected.

Credential file format:

```json
{"username":"opencode","password":"your-server-password"}
```

OpenCode uses `OPENCODE_SERVER_PASSWORD` and an optional
`OPENCODE_SERVER_USERNAME` (default: `opencode`). Telephone stores the file path,
not the credentials, and reads the file when sending. Keep credentials out of
command-line arguments, messages and version control.

## Bind

Replace the placeholders with the registered address and owning session's values:

```sh
telephone bind-opencode \
  --address opencode:YOUR-REGISTERED-ADDRESS \
  --endpoint http://127.0.0.1:4096 \
  --session ses_YOUR_EXISTING_SESSION \
  --directory /absolute/session/directory \
  --credentials /absolute/private/opencode-auth.json
```

The Telephone address and OpenCode session ID are distinct. Binding validates
authentication, session ID and directory with read-only requests. It does not
create a session or send a prompt.

Return to polling:

```sh
telephone unbind-opencode opencode:YOUR-REGISTERED-ADDRESS
```

Unbinding retains queued messages and does not cancel in-flight requests.
Unregistering or expiration followed by re-registration removes the binding.
Binding is CLI-only.

## Desktop

Desktop owns its server; a separate `opencode serve` process does not attach to
it. Setup requires the owning server's endpoint, credentials and exact session
ID. Telephone does not discover these. Use polling if they are unavailable.
Verify the binding after a restart and rebind if the endpoint or credentials
change.

## Delivery behavior

- Native delivery can start model work. It uses the last recorded agent, model
  and variant. Unsent UI selections are not available; concurrent changes may
  race with delivery. Unbind before changing modes if this matters.
- A session with no recorded turn uses its polling inbox.
- Failed read-only preflight can fall back to polling. Missing or unsafe
  credentials and malformed bindings fail with an error.
- HTTP `204` means asynchronous acceptance, not a read receipt. Processing can
  fail later.
- Errors after POST begins are uncertain. Telephone neither retries nor queues
  a duplicate. Message IDs provide correlation, not idempotency.
- `http` in discovery indicates a configured route, not reachability or liveness.

## Security and limits

Only literal HTTP loopback addresses are allowed. DNS hostnames, remote hosts,
proxies and redirects are disabled. Requests have a three-second deadline,
including a one-second connection timeout. Response headers are limited to
16 KiB; read-only response bodies to 256 KiB.

HTTP Basic authenticates the caller, **not the server**. Trust the configured
listener and local users: a replacement listener can steal its credentials.
This route does not authenticate peer identities and is unsuitable for hostile
multi-user machines. See [security and delivery limits](../SECURITY.md).
