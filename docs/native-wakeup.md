# Native delivery requirements: Zed and Delta

Proposed integration contract; not implemented and not an ACP standard.
Current support uses polling inboxes. Tracked in
[#25](https://github.com/iamnbutler/telephone/issues/25).

## Integration point

Native delivery needs access to the host's thread input queue and scheduler.
MCP tools alone do not provide this.

| Target | Required integration |
| --- | --- |
| Zed built-in agent | Zed thread input queue and scheduler |
| External ACP agent in Zed | Owning ACP client and existing agent session |
| Delta | Thread scheduler on the selected execution machine |

Zed's built-in and external agents are separate integrations; see
[Zed agents](https://zed.dev/docs/ai/agents). Delta can change execution machines
between turns; local routes must not follow a thread into cloud execution.
See [Delta machines](https://delta.dev/docs/concepts/worktrees-and-machines).

ACP clients initiate [`session/prompt`](https://agentclientprotocol.com/protocol/v1/prompt-turn).
The [stdio transport](https://agentclientprotocol.com/protocol/v1/transports)
belongs to the spawning client and agent. A session ID does not give Telephone
access to that connection. An ACP extension still requires host-side routing
and scheduling; a wrapper covers only sessions launched through it.

## Host contract

1. **Binding:** opt in an exact thread, host instance and execution machine.
   Use an expiring, revocable routing grant, not a title, cwd or model name.
2. **Admission:** validate the destination grant and bounded message before
   acceptance. Reject unknown, expired, closed or wrong-machine destinations.
3. **Scheduling:** wake idle threads; queue behind busy turns or reject before
   acceptance. Preserve model, mode, permissions, cancellation and user stops.
4. **Provenance:** show peer messages in history as untrusted input, not human
   instructions. Admission does not authorize the requested action.
5. **Receipts:** distinguish rejection, queue acceptance, entry into model
   context and completion. Provide read-only status lookup after a lost response.
6. **Deduplication:** reject conflicting uses of a message ID; do not enqueue
   identical repeats within a documented persistence window.
7. **Lifecycle:** bound queues, deadlines and wake-up rates. Revoke stale grants
   on restart or migration; preserve pending messages on failure.

Use owner-local IPC with peer checks and protected credentials. Routing grants
do not isolate hostile processes under the same UID. Remote ingress requires
the separate [authentication and authorization work](https://github.com/iamnbutler/telephone/issues/1).

Telephone must journal before sending. Only definitely unattempted delivery may
fall back to an inbox. Uncertain admission must not trigger a retry or duplicate.

## Acceptance criteria

- Idle delivery needs no polling or manual prompt.
- Threads sharing a directory or MCP server remain isolated.
- Busy turns finish without interruption; queued messages run afterward.
- Unknown destinations, revoked grants and wrong-machine routes fail closed.
- Restart, lost responses, cancellation and duplicate IDs do not duplicate work.
- Peer provenance and existing permission prompts remain visible.
- Native absence permits polling fallback; uncertain native delivery does not.

Validate in isolated threads with explicit authorization for model calls.
An integration requires a supported host interface, not private IPC, credential
scraping, application-database writes or UI automation.
