#!/usr/bin/env bash
set -euo pipefail

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
marker="$tmp/injected"
hostile="$tmp/"$'cwd with spaces \047 ; touch '"$marker"$' ; $DOLLAR $(touch '"$marker"$')\nnext line'
mkdir -p "$hostile" "$tmp/bin"
test ! -e "$marker"

cat >"$tmp/bin/herdr" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pane list")
    python3 -c 'import json,os; print(json.dumps({"result":{"panes":[{"pane_id":"pane-1","cwd":os.environ["HOSTILE_CWD"],"agent_session":{"kind":"id","value":"11111111-1111-1111-1111-111111111111","agent":"claude"}}]}}))'
    ;;
  "pane split")
    printf '%s\n' '{"result":{"pane":{"pane_id":"pane-2"}}}'
    ;;
  "pane run")
    printf '%s' "$4" >"$PANE_COMMAND"
    printf '2\n' | /bin/bash -c "$4"
    ;;
  *)
    exit 2
    ;;
esac
EOF

cat >"$tmp/bin/vaultr" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
: >"$CAPTURED_ARGV"
for arg in "$@"; do
  printf '%s\0' "$arg" >>"$CAPTURED_ARGV"
done
EOF
chmod +x "$tmp/bin/herdr" "$tmp/bin/vaultr"

export HOSTILE_CWD="$hostile"
export PANE_COMMAND="$tmp/pane-command"
export CAPTURED_ARGV="$tmp/argv"
export HERDR_BIN_PATH="$tmp/bin/herdr"
export HERDR_PANE_ID="pane-1"
export PATH="$tmp/bin:$PATH"

"$(dirname "$0")/fork.sh" >/dev/null

argv=()
while IFS= read -r -d '' arg; do
  argv+=("$arg")
done <"$CAPTURED_ARGV"
expected=(
  session fork 11111111-1111-1111-1111-111111111111
  --into codex --cwd "$hostile"
)
test "${#argv[@]}" -eq "${#expected[@]}"
for i in "${!expected[@]}"; do
  test "${argv[$i]}" = "${expected[$i]}"
done
test ! -e "$marker"
