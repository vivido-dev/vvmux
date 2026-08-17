"""Managed vvmux plugin that reports resumable Hermes session identity."""
# VVMUX_INTEGRATION_ID=hermes
# VVMUX_INTEGRATION_VERSION=1

from __future__ import annotations
import os
import subprocess
import time

_INTERACTIVE = {"cli", "tui", "desktop", "acp"}

def _report(**kwargs) -> None:
    if kwargs.get("platform") not in _INTERACTIVE:
        return
    binary = os.environ.get("VVMUX_BIN")
    session = os.environ.get("VVMUX_SESSION")
    pane = os.environ.get("VVMUX_PANE_ID")
    session_id = kwargs.get("session_id")
    if not all((binary, session, pane)) or not isinstance(session_id, str) or not session_id:
        return
    command = [binary, "msg", "--target", session, "report-agent-session", "--agent",
        "hermes", "--source", "vvmux:hermes", "--sequence", str(time.time_ns()),
        "--agent-session-id", session_id, "--pane-id", pane]
    try:
        options = {"timeout": 0.5, "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
        if os.name == "nt":
            options["creationflags"] = subprocess.CREATE_NO_WINDOW
        subprocess.run(command, check=False, **options)
    except Exception:
        pass

def _observe(**kwargs) -> None:
    if kwargs.get("platform") == "cli":
        _report(**kwargs)

def register(ctx) -> None:
    ctx.register_hook("on_session_start", _report)
    ctx.register_hook("on_session_reset", _report)
    ctx.register_hook("pre_llm_call", _observe)
