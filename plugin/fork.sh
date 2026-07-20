#!/usr/bin/env bash
# Open a focused right split, ask Claude or Codex, then fork the pane's session into it.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

pane=$(focused_pane)
exec 3< <(pane_session "$pane" null)
IFS= read -r -d '' sid <&3
IFS= read -r -d '' _agent <&3
IFS= read -r -d '' pcwd <&3
exec 3<&-
new=$("$HERDR" pane split "$pane" --direction right --ratio 0.5 --focus \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
# Prompt runs inside the new pane so the fork's CLI takes over the same
# terminal. The pane's cwd is passed explicitly — it is where the agent is
# actually running, and some captured sessions have no recorded cwd.
printf -v command '%q ' bash -c \
  'printf "Fork %s into [1] claude or [2] codex? " "$1"; read -r c; case $c in 2|codex) t=codex;; *) t=claude;; esac; vaultr session fork "$1" --into "$t" --cwd "$2"' \
  bash "$sid" "$pcwd"
"$HERDR" pane run "$new" "$command"
