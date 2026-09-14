#!/usr/bin/env bash
set -euo pipefail

# Run from a Codex shell, or pass the exact Codex address from `telephone list`.
demo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$demo_root"
cargo build --quiet
demo_bin="$demo_root/target/debug/telephone"
demo_peer="${1:-$("$demo_bin" whoami | sed -n 's/^address: //p')}"

if [[ ! "$demo_peer" =~ ^codex:[a-zA-Z0-9-]+$ ]]; then
    printf 'Usage: bash scripts/demo-claude.sh codex:<thread-id>\n' >&2
    printf 'Find the exact address with target/debug/telephone list.\n' >&2
    exit 1
fi

command -v claude >/dev/null
command -v jq >/dev/null
demo_config="$(jq -cn --arg bin "$demo_bin" \
    '{mcpServers:{telephone:{command:$bin,args:["mcp"]}}}')"

printf 'Starting Claude Code; demo peer: %s\n' "$demo_peer"
exec claude --name telephone-demo \
    --strict-mcp-config --mcp-config "$demo_config" \
    --tools '' \
    --allowedTools 'mcp__telephone__list_agents,mcp__telephone__send_message,mcp__telephone__check_inbox' \
    --permission-mode dontAsk \
    --append-system-prompt "You are participating in a user-authorized local Telephone demo with $demo_peer. Use only the Telephone MCP tools and message only that exact peer. Do not edit files or settings. Reply once to each demo request using send_message with kind reply and reply_to the incoming message id. Include any requested nonce. Do not reply to replies or informational messages, to avoid loops." \
    "Call Telephone list_agents, verify that your own address starts with claude: and that $demo_peer is listed, then send that peer a short original greeting using send_message with kind inform. Report your own Telephone address and wait for a demo request. If identity or delivery fails, report the error."
