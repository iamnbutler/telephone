#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 ]] || { printf 'Usage: bash scripts/smoke-test.sh <binary> <version>\n' >&2; exit 1; }
binary=$1
version=$2
[[ $("$binary" --version) == "telephone $version" ]]
"$binary" --help >/dev/null
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"package-smoke","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"ping"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/list"}' |
  "$binary" mcp | jq -se 'length == 3 and .[1].result == {} and (.[2].result.tools | map(.name) | sort == ["check_inbox", "list_agents", "register_agent", "send_message", "unregister_agent"])' >/dev/null
