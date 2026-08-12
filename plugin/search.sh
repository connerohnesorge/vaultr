#!/usr/bin/env bash
# Open a split and run a lexical vault-session search selected by the operator.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

pane=$(focused_pane)
new=$("$HERDR" pane split "$pane" --direction right --ratio 0.45 --focus \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
printf -v command '%q ' bash -c \
  'printf "Search vault sessions: "; read -r query; test -n "$query" && vaultr session search --curated "$query" | less -R' \
  bash
"$HERDR" pane run "$new" "$command"
