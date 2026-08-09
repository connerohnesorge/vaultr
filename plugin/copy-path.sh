#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

pane=$(focused_pane)
IFS= read -r -d '' sid < <(pane_session "$pane")
dir=$(vaultr session path "$sid")
# macOS: pbcopy reaches the pasteboard of the machine the server runs on, which is the
# one the operator is sitting at. Linux: this plugin runs on a headless server with no
# DISPLAY, so the xclip branch could never succeed — it failed silently and looked like a
# no-op. Print the path instead and let the operator drag-select it, which crosses the
# client/server boundary to the Mac pasteboard by a mechanism with no silent-loss mode.
# See runbooks/herdr-copy-a-path-across-the-boundary.md.
if [ "$(uname -s)" = "Darwin" ]; then
  printf '%s' "$dir" | pbcopy
  "$HERDR" notification show "Vault session path copied" --body "$dir" --sound done || true
  echo "copied: $dir"
else
  "$HERDR" notification show "Vault session path" --body "$dir" --sound done || true
  echo "select to copy: $dir"
fi
