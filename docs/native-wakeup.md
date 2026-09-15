# Native wake-up: Zed and Delta

Investigation brief, 2026-09-15. Follow-up to [#25](https://github.com/iamnbutler/telephone/issues/25).
This is a proposed integration contract, not an implemented API or an ACP standard.

## Goal

Deliver untrusted peer text into one explicitly opted-in, existing thread and
schedule that thread to handle it without an LLM polling loop. Keep the same
conversation, tools, permissions and visible UI. Do not create a replacement
session or interrupt the user's current turn.

Telephone already supplies per-thread addresses, a local journal, reply ancestry
and CLI/MCP inboxes. What is missing is a supported connection to the host's
thread scheduler. Installing the MCP server alone does not provide that connection.

## Which integration?

| Target | Where the wake-up must enter |
| --- | --- |
| Zed's built-in agent | Zed's own thread input queue and scheduler |
| An external ACP agent in Zed | Zed's owning ACP connection, followed by the agent's existing session |
| Delta | Delta's thread scheduler on the selected execution machine |

Zed distinguishes its built-in agent, ACP external agents and terminal threads.
Do not infer the harness from the selected model. External-agent installation is
not access to all Zed threads. See [Zed agent paths](https://zed.dev/docs/ai/agents)
and [external agents](https://zed.dev/docs/ai/external-agents).

Delta's documented thread UI does not establish an ACP interface. Its execution
machine can change between turns, and shared history does not share local tools
or credentials. Start with local execution only; a cloud turn must not silently
receive a local-machine route. See [Delta machines](https://delta.dev/docs/concepts/worktrees-and-machines).

## What ACP does and does not solve

ACP's client initiates `session/prompt`; the agent streams `session/update` and
eventually returns a stop reason. Session setup establishes the session ID.
Loading or resuming depends on advertised capabilities. Those mechanisms are not
permission to attach a second controller to a live session, nor a discovery API
for unrelated editor threads. [Prompt lifecycle](https://agentclientprotocol.com/protocol/v1/prompt-turn),
[session setup](https://agentclientprotocol.com/protocol/v1/session-setup).

The documented stdio transport is a connection between the spawning client and
its agent. Telephone cannot join that pipe by knowing a session ID.
[ACP transports](https://agentclientprotocol.com/protocol/v1/transports).

ACP supports extension methods beginning with `_` and capability/data extensions
inside `_meta`. Both implementations must support the behavior; adding a field
does not create routing, scheduling or a security boundary.
[Extensibility](https://agentclientprotocol.com/protocol/v1/extensibility).

A plausible design is a host-owned local bridge. It accepts authorized peer
messages, queues them against the exact thread, and asks the owning ACP client
to start the next turn. A negotiated Client → Agent extension could preserve
typed peer provenance and admission semantics across that second boundary.
For example, `_telephone/peer_message` with a versioned capability in `_meta`.
That name is illustrative, not registered or supported today. Keep custom data
out of reserved ACP root fields. Never substitute `session/update` output for
an input request.

Prefer a host hook first. A wrapper/proxy around an ACP agent is a narrower
experiment: it covers only sessions launched through that wrapper, must forward
permissions, cancellation and all other traffic faithfully, and needs host
support for displaying and scheduling externally initiated turns. It cannot be
presented as support for Zed's built-in agent or Delta.

## Minimum host contract

1. **Opt-in binding.** Bind a Telephone address to an opaque thread/session ID,
   the owning host instance and execution machine. Issue an expiring, revocable
   routing grant. A PID, title, cwd, model name or shared MCP process is not a
   thread identity. Do not expose every thread by default.
2. **Admission.** Accept a message ID, claimed sender, destination grant,
   conversation/reply IDs and bounded text. Check the grant before accepting.
   Treat sender claims as untrusted data. Reject unknown, expired, closed or
   wrong-machine destinations without substituting a new thread.
3. **Scheduling.** Wake an idle opted-in thread. Queue behind a busy turn, or
   return an explicit not-accepted result. Do not cancel work, auto-approve
   tools, switch models/modes, or bypass a user's stop. Define whether delivery
   to a closed thread is refused or requires a separate resume permission.
4. **Provenance.** Show a peer-message event in history and preserve its lower
   authority in model context. Do not represent it as a human's instruction.
   Keep existing approvals and cost controls; accepting a request does not
   authorize its requested actions. A text warning is useful, not a sandbox.
5. **Receipts and deduplication.** Distinguish rejected, accepted/queued, entered
   model context, and completed. A transport write or turn completion is not
   proof of compliance. A repeated ID with identical content must not enqueue
   twice; conflicting content must fail. Document persistence and the bounded
   deduplication window. Provide read-only receipt lookup after a lost response.
6. **Lifecycle and bounds.** Expose unavailable/unknown separately from dead;
   a stored registration does not prove liveness. Bound message sizes, queues,
   deadlines and wake-up rate. Revoke stale bindings on host restart or route
   migration. Preserve pending messages on failure.

For the first implementation, use owner-local IPC with peer checks and protected
credentials. Grants scope routing but cannot isolate mutually hostile same-UID
processes in Telephone's current trust model. Do not scan ports, scrape tokens,
write application databases, attach to private live IPC, or enable remote ingress.
Networked sources remain gated by [#1](https://github.com/iamnbutler/telephone/issues/1).

Telephone must journal before sending. Only a definitely unattempted delivery
may fall back to its inbox. A timeout or disconnect after admission may have
started is uncertain: no automatic retry or second inbox copy. Adopting a richer
receipt protocol must not weaken this rule.

## Investigation and acceptance test

Start by identifying the installed host version, supported extension surface,
thread owner and actual prompt-dispatch code. Determine whether a supported
plugin can invoke that code, whether a host patch is required, or whether only
a custom ACP wrapper is viable. Use [Zed ACP logs](https://zed.dev/docs/ai/external-agents#debugging)
in disposable test threads. Zed's MCP tools and prompts are not a generic
new-turn notification API; [its MCP support](https://zed.dev/docs/ai/mcp) documents
tool-list notifications, which must not be repurposed as messages.

Prove with real clients and harmless nonce messages:

- An idle existing thread responds without polling or a manual nudge.
- Two threads sharing a cwd/model/MCP server cannot receive each other's message.
- A busy thread keeps its current turn; one queued message runs afterward.
- Missing capability, unknown thread, revoked grant and wrong machine fail closed.
- Restart, lost response, duplicate ID and cancellation do not duplicate delivery.
- The UI and model retain peer provenance; permission requests still reach the user.
- Native absence preserves the inbox fallback; uncertain native delivery does not.

Use an isolated checkout/profile and explicit consent for model calls. Never test
by injecting into unrelated live work. A successful inbox `--peek` proves retrieval
but deliberately leaves its unread flag unchanged; it is not a native-wake test.

Deliver a small proof of concept plus exact host/agent versions, protocol trace,
required host changes, unsupported cases and a go/no-go recommendation. If the
only route depends on private internals or UI automation, document that limit
and stop. Do not claim an ACP-only solution until the host-side connection and
scheduler integration are demonstrated.
