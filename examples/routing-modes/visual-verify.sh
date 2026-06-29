#!/usr/bin/env bash
# Demo a routing mode: run `rayline claude --config <MODE>.json` and watch routing.
#
#   interactive (TTY)        -> records an asciinema cast in a split-pane tmux
#                               session: left pane = the claude run, right pane =
#                               `rayline top` (live routing metrics).
#   headless (no TTY, or     -> runs the same command, then prints `rayline top
#   DEMO_HEADLESS=1)            --all` as a plain-text transcript (no asciinema /
#                               tmux, which need a real terminal). For CI / scripted
#                               verification.
#
# Usage:
#   ./visual-verify.sh [MODE] [PROMPT]
#     MODE    one of the configs in this dir (default: RRC), e.g. RRC RLC ARCL RRL LL AL ...
#     PROMPT  the prompt sent to Claude Code in print mode; overrides the default.
#             The default spawns subagents so subagent routing — may-local
#             (RRCL/ARCL) and per-class LSR routing (RRL) — is actually visible.
#             (A plain "say pong" never spawns a subagent.)
#
# Env:
#   RAYLINE_BIN    rayline binary to use (default: rayline from PATH)
#   DEMO_HEADLESS  set to 1 to force the text path even on a TTY
#   WINDOW_SIZE    asciinema --window-size COLSxROWS (default: 220x50)
#
# Forces `--via proxy` so `rayline top` has metrics to display.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${1:-RRC}"
PROMPT="${2:-Launch TWO subagents with the Task tool, in parallel: an Explore subagent and a general-purpose subagent. Each must reply with exactly the word PONG. After both return, output DONE.}"
RAYLINE_BIN="${RAYLINE_BIN:-rayline}"
WINDOW_SIZE="${WINDOW_SIZE:-220x50}"
CFG="$HERE/$MODE.json"
CAST="$HERE/$MODE-demo.cast"
SOCKET="rayline-demo-$$"

command -v "$RAYLINE_BIN" >/dev/null 2>&1 || { echo "error: '$RAYLINE_BIN' not found on PATH" >&2; exit 1; }
[ -f "$CFG" ] || { echo "error: no config '$CFG' (pick a MODE from this dir)" >&2; exit 1; }

# Headless: no TTY (CI / scripted) or DEMO_HEADLESS=1. asciinema/tmux need a real
# terminal, so run the command and print `rayline top` as text instead.
if [ "${DEMO_HEADLESS:-}" = 1 ] || [ ! -t 1 ]; then
  echo "=== $MODE: $RAYLINE_BIN claude --config $MODE.json --via proxy  (headless) ==="
  echo "--- prompt: $PROMPT"
  echo
  "$RAYLINE_BIN" claude --config "$CFG" --via proxy -- -p "$PROMPT" || true
  echo
  echo "=== routing observed (recent requests, newest last) ==="
  # A one-shot `top` snapshot taken after the run shows no *active* requests, so
  # report the *recent* (completed) ones — each line is one routed turn.
  if command -v python3 >/dev/null 2>&1; then
    "$RAYLINE_BIN" top --json --all 2>/dev/null | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
rows = d.get("recent") or []
if not rows:
    print("  (no recent requests — did the run reach the router?)")
for r in reversed(rows[:24]):
    cls = r.get("agent_type") or "main"
    print("  %-16s target=%-10s model=%s" % (cls, r.get("target"), r.get("selected_model")))
' || "$RAYLINE_BIN" top --all || true
  else
    "$RAYLINE_BIN" top --all || true
  fi
  exit 0
fi

# Interactive: record an asciinema cast driving a tmux split. The tmux driver runs
# *inside* the recording. It builds the split, starts `rayline top` on the right,
# drives the command on the left from a background scheduler, then kills the server
# so the foreground attach (and the recording) ends.
for tool in asciinema tmux; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: '$tool' not found (needed for the recorded demo; set DEMO_HEADLESS=1 for the text path)" >&2; exit 1; }
done
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
