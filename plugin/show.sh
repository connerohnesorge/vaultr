#!/usr/bin/env bash
# Open an unfocused right split rendering the pane's transcript (held open in a pager).
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

pane=$(focused_pane)
read -r sid _agent _cwd < <(pane_session "$pane")
new=$("$HERDR" pane split "$pane" --direction right --ratio 0.45 --no-focus \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
"$HERDR" pane run "$new" "vaultr session show $sid | less -R"
