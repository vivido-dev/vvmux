#!/usr/bin/env python3
import json
import os
import sys


def action() -> None:
    value = json.load(sys.stdin)
    summary = str(value.get("summary", "No summary supplied"))[:512]
    chart = value.get("chart", {})
    json.dump(
        {
            "title": "Verification dashboard",
            "lines": [summary, f"chart={json.dumps(chart, sort_keys=True)}"],
        },
        sys.stdout,
    )


def pane() -> None:
    plugin = os.environ.get("VVMUX_PLUGIN_ID", "unknown")
    print("\x1b[2J\x1b[H\x1b[1;36mVerification dashboard\x1b[0m")
    print(f"plugin: {plugin}")
    print("This is a real plugin PTY pane; type a note and press Enter.")
    for line in sys.stdin:
        print(f"note: {line.rstrip()}")


if __name__ == "__main__":
    action() if sys.argv[1:] == ["action"] else pane()
