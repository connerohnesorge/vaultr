#!/usr/bin/env bash
# Shared: resolve the focused pane's exact vault session id via herdr metadata.
set -euo pipefail

HERDR="${HERDR_BIN_PATH:-herdr}"

focused_pane() {
  if [ -n "${HERDR_PLUGIN_CONTEXT_JSON:-}" ]; then
    printf '%s' "$HERDR_PLUGIN_CONTEXT_JSON" | python3 -c 'import sys,json;print(json.load(sys.stdin)["focused_pane_id"])' && return
  fi
  printf '%s' "${HERDR_PANE_ID:?no pane context}"
}

# echoes: <session_id> <agent> <cwd>
pane_session() {
  local pane="$1"
  local format="${2:-line}"
  "$HERDR" pane list | python3 -c '
import sys, json
pane = sys.argv[1]
output_format = sys.argv[2]
for p in json.load(sys.stdin)["result"]["panes"]:
    if p["pane_id"] == pane:
        s = p.get("agent_session")
        if not s or s.get("kind") != "id":
            sys.exit(f"pane {pane} has no agent session id")
        values = (s["value"], s.get("agent", "?"), p.get("cwd", ""))
        if output_format == "null":
            for value in values:
                sys.stdout.buffer.write(value.encode() + b"\0")
        else:
            print(*values)
        sys.exit(0)
sys.exit(f"pane {pane} not found")
' "$pane" "$format"
}
