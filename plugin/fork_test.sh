#!/usr/bin/env bash
set -euo pipefail

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
marker="$tmp/injected"
hostile="$tmp/"$'cwd with spaces \047 ; touch '"$marker"$' ; $DOLLAR $(touch '"$marker"$')\nnext line'
mkdir -p "$hostile" "$tmp/bin"
test ! -e "$marker"

assert_nul_fields() {
  local file="$1"
  shift
  local actual=()
  local value
  while IFS= read -r -d '' value; do
    actual+=("$value")
  done <"$file"
  test "${#actual[@]}" -eq "$#"
  local i=0
  for value in "$@"; do
    test "${actual[$i]}" = "$value"
    i=$((i + 1))
  done
}

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
    if [ "${RUN_PANE_COMMAND:-0}" = 1 ]; then
      printf '2\n' | /bin/bash -c "$4"
    fi
    ;;
  "notification show")
    : >"$NOTIFICATION_ARGV"
    for arg in "$@"; do
      printf '%s\0' "$arg" >>"$NOTIFICATION_ARGV"
    done
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
if [ "$1 $2" = "session path" ]; then
  printf '%s' "$FAKE_SESSION_PATH"
fi
EOF

cat >"$tmp/bin/pbcopy" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
cat >"$CLIPBOARD"
EOF
chmod +x "$tmp/bin/herdr" "$tmp/bin/vaultr" "$tmp/bin/pbcopy"

export HOSTILE_CWD="$hostile"
export PANE_COMMAND="$tmp/pane-command"
export CAPTURED_ARGV="$tmp/argv"
export NOTIFICATION_ARGV="$tmp/notification-argv"
export CLIPBOARD="$tmp/clipboard"
export FAKE_SESSION_PATH="$hostile"
export HERDR_BIN_PATH="$tmp/bin/herdr"
export HERDR_PANE_ID="pane-1"
export PATH="$tmp/bin:$PATH"
sid="11111111-1111-1111-1111-111111111111"

(
  cd "$(dirname "$0")"
  . ./lib.sh
  pane_session "$HERDR_PANE_ID"
) >"$tmp/pane-session"
assert_nul_fields "$tmp/pane-session" "$sid" claude "$hostile"

export RUN_PANE_COMMAND=1
"$(dirname "$0")/fork.sh" >/dev/null
assert_nul_fields "$CAPTURED_ARGV" \
  session fork "$sid" --into codex --cwd "$hostile"
test ! -e "$marker"

export RUN_PANE_COMMAND=0
"$(dirname "$0")/show.sh" >/dev/null
test "$(cat "$PANE_COMMAND")" = "vaultr session show $sid | less -R"

"$(dirname "$0")/copy-path.sh" >/dev/null
assert_nul_fields "$CAPTURED_ARGV" session path "$sid"
printf '%s' "$hostile" >"$tmp/expected-path"
cmp "$tmp/expected-path" "$CLIPBOARD"
assert_nul_fields "$NOTIFICATION_ARGV" \
  notification show "Vault session path copied" --body "$hostile" --sound done
test ! -e "$marker"
