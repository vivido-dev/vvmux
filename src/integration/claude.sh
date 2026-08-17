#!/bin/sh
# VVMUX_INTEGRATION_ID=claude
# VVMUX_INTEGRATION_VERSION=1
# Managed by vvmux; reinstalling may overwrite this file.

set -eu
[ "${1:-}" = "session" ] || exit 0
[ -n "${VVMUX_BIN:-}" ] || exit 0
[ -n "${VVMUX_SESSION:-}" ] || exit 0
[ -n "${VVMUX_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0
input_file="$(mktemp "${TMPDIR:-/tmp}/vvmux-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$input_file"' EXIT HUP INT TERM
cat >"$input_file" 2>/dev/null || true
VVMUX_HOOK_INPUT="$input_file" python3 - <<'PY' >/dev/null 2>&1 || true
import json, os, subprocess, time
try:
    with open(os.environ["VVMUX_HOOK_INPUT"], encoding="utf-8") as handle:
        payload = json.load(handle)
except Exception:
    raise SystemExit(0)
if payload.get("agent_id") or payload.get("hook_event_name") == "SubagentStop":
    raise SystemExit(0)
session_id = payload.get("session_id")
if not isinstance(session_id, str) or not session_id:
    raise SystemExit(0)
args = [os.environ["VVMUX_BIN"], "msg", "--target", os.environ["VVMUX_SESSION"],
        "report-agent-session", "--agent", "claude", "--source", "vvmux:claude",
        "--sequence", str(time.time_ns()), "--agent-session-id", session_id,
        "--pane-id", os.environ["VVMUX_PANE_ID"]]
path = payload.get("transcript_path")
if isinstance(path, str) and path:
    args[-2:-2] = ["--agent-session-path", path]
try:
    subprocess.run(args, timeout=0.5, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
except Exception:
    pass
PY
exit 0
