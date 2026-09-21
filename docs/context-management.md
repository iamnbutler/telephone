# Context management

Keep peer updates to new findings, decisions, blockers and relevant file paths.
Avoid repeated plans, full transcripts and acknowledgment loops. Telephone's
message wrapper retains provenance and a reply command; polling instructions
only accompany registration and sends from registered inboxes.

## Claude Code reminders

Merge this into `.claude/settings.json` for one project, or
`~/.claude/settings.json` for all projects. Preserve existing hooks:

```json
{
  "hooks": {
    "PostToolUse": [{
      "hooks": [{
        "type": "command",
        "command": "telephone context-hook",
        "timeout": 5
      }]
    }]
  }
}
```

Install a Telephone build containing `context-hook` first. Use the absolute
executable path printed by `telephone install` if Telephone is absent from the
hook's PATH. Hooks apply after tools finish, including during Telephone-driven
turns. The command also supports `UserPromptSubmit` if configured for that event.

At **250,000 tokens**, the hook reminds the agent to preserve a short checkpoint
and prepare to compact. At **300,000**, it asks for compaction at the next safe
boundary. Preserve the task, decisions, pending work, peer addresses and reply
IDs. Each threshold emits once per session/transcript; repeated calls are silent.
Falling below 250k or encountering a recorded compaction boundary re-arms it.
Starting above 300k emits only the higher-priority reminder.

The estimate is the latest recorded assistant request's input, cache-read,
cache-creation and output tokens, not cumulative session usage. Where Claude
records several request iterations, only the last message iteration counts;
this also supports records with zeroed aggregate counters. The hook reads
at most the last 1 MiB of the transcript. Missing, malformed or unreadable
telemetry is skipped; errors never block a tool or turn. Claude writes transcripts
asynchronously, so reminders may lag. Unknown usage does not mean low usage.
Reminder state lives in Telephone's private SQLite journal; transcript contents
are not stored there. No reminder text is added to ordinary message wrappers,
tool definitions or empty inbox polls.

The hook is advisory: it cannot execute `/compact`. For unattended sessions,
enable native compaction too. In Claude Code v2.1.221 or later, run:

```text
/autocompact 300k
```

This saves the window for current and future sessions. For a single launch, use
`claude --autocompact 300k`, or merge `"autoCompactWindow": 300000` into Claude
settings. Ensure auto-compaction is enabled. The runtime caps the window at the
model's limit; `CLAUDE_CODE_AUTO_COMPACT_WINDOW` takes precedence over these
settings. Use `/compact` to summarize an existing large conversation. `/clear`
starts a new conversation and is not a substitute while work is pending.

See Claude's [hook reference](https://code.claude.com/docs/en/hooks#add-context-for-claude)
and [auto-compaction settings](https://code.claude.com/docs/en/model-config#set-the-auto-compact-window).
Other runtimes should use their native context/compaction controls; this hook
only understands Claude Code transcripts and does not monitor Codex or OpenCode.
