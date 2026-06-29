#!/usr/bin/env bash
# Record an asciinema cast of a routing mode in a split-pane tmux session:
#   left pane  -> `rayline claude --config <MODE>.json` (the new command)
#   right pane -> `rayline top`    (live routing metrics)
#
# Usage:
#   ./visual-demo.sh [MODE] [PROMPT]
#     MODE    one of the configs in this dir (default: RRC), e.g. RRC RLC LL AL ...
#     PROMPT  the prompt sent to Claude Code in print mode
#             (default: "Reply with exactly one word: pong")
#
# Env:
#   RAYLINE_BIN   rayline binary to use (default: rayline from PATH)
#   WINDOW_SIZE   asciinema --window-size COLSxROWS (default: 220x50)
#
# Requires: asciinema, tmux. Forces `--via proxy` so `rayline top` has metrics
# to display. Output: <MODE>-demo.cast in this directory.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${1:-RRC}"
PROMPT="${2:-Reply with exactly one word: pong}"
RAYLINE_BIN="${RAYLINE_BIN:-rayline}"
WINDOW_SIZE="${WINDOW_SIZE:-220x50}"
CFG="$HERE/$MODE.json"
CAST="$HERE/$MODE-demo.cast"
SOCKET="rayline-demo-$$"

for tool in asciinema tmux "$RAYLINE_BIN"; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: '$tool' not found on PATH" >&2; exit 1; }
done
[ -f "$CFG" ] || { echo "error: no config '$CFG' (pick a MODE from this dir)" >&2; exit 1; }

# The tmux driver runs *inside* the asciinema recording. It builds the split,
# starts `rayline top` on the right, drives the command on the left from a
# background scheduler, then kills the server so the foreground attach (and the
# recording) ends.
DRIVER="$(mktemp)"
trap 'rm -f "$DRIVER"; tmux -L "$SOCKET" kill-server 2>/dev/null || true' EXIT
cat >"$DRIVER" <<EOF
#!/usr/bin/env bash
set -u
tm() { tmux -L "$SOCKET" "\$@"; }
tm kill-server 2>/dev/null || true
tm new-session -d -s demo -x ${WINDOW_SIZE%x*} -y ${WINDOW_SIZE#*x} -c "$HERE"
tm set -t demo status off
tm split-window -h -t demo:0 -c "$HERE"
tm select-pane -t demo:0.0
tm send-keys -t demo:0.1 "clear; echo '>>> rayline top  (live routing metrics)'; sleep 1; '$RAYLINE_BIN' top --all" Enter
tm send-keys -t demo:0.0 "clear; echo '=== $MODE: rayline claude --config $MODE.json (--via proxy) ==='; echo" Enter
(
  sleep 3
  tm send-keys -t demo:0.0 "'$RAYLINE_BIN' claude --config '$CFG' --via proxy -- -p '$PROMPT'" Enter
  sleep 45
  tm send-keys -t demo:0.0 "echo; echo '=== demo complete: $MODE routed, observed in rayline top ==='" Enter
  sleep 5
  tm kill-server
) &
exec tmux -L "$SOCKET" attach -t demo
EOF
chmod +x "$DRIVER"

echo "recording $MODE -> $CAST"
asciinema rec "$CAST" --overwrite --window-size "$WINDOW_SIZE" -c "$DRIVER"
echo "done. play with: asciinema play '$CAST'"
