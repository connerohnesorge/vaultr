#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

pane=$(focused_pane)
read -r sid _agent _cwd < <(pane_session "$pane")
dir=$(vaultr session path "$sid")
printf '%s' "$dir" | pbcopy 2>/dev/null || printf '%s' "$dir" | xclip -selection clipboard
"$HERDR" notification show "Vault session path copied" --body "$dir" --sound done || true
echo "copied: $dir"
