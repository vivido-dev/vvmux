#!/usr/bin/env python3
"""Protocol-1 fixture that exercises brokered host calls without importing an SDK."""

import json
import os
import struct
import sys


def read_frame():
    prefix = sys.stdin.buffer.read(4)
    if len(prefix) != 4:
        raise EOFError
    length = struct.unpack(">I", prefix)[0]
    return json.loads(sys.stdin.buffer.read(length))


def write_frame(value):
    body = json.dumps(value, separators=(",", ":")).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(body)) + body)
    sys.stdout.buffer.flush()


write_frame(
    {
        "type": "hello",
        "protocol_version": 1,
        "plugin_id": os.environ["VVMUX_PLUGIN_ID"],
        "instance_id": os.environ["VVMUX_PLUGIN_INSTANCE"],
        "features": ["host_calls"],
    }
)

while True:
    message = read_frame()
    request_id = message["request_id"]
    if message["type"] == "initialize":
        write_frame({"type": "ready", "request_id": request_id})
    elif message["type"] == "invoke":
        write_frame(
            {
                "type": "host_call",
                "request_id": 41,
                "method": "pane.get_text",
                "params": {"pane_id": 1},
            }
        )
        pane_text = read_frame()
        write_frame(
            {
                "type": "host_call",
                "request_id": 42,
                "method": "pane.input",
                "params": {"pane_id": 1, "text": "BROKER_INPUT\n"},
            }
        )
        input_result = read_frame()
        write_frame(
            {
                "type": "host_call",
                "request_id": 43,
                "method": "session.inspect",
                "params": {},
            }
        )
        session = read_frame()
        write_frame(
            {
                "type": "result",
                "request_id": request_id,
                "result": {
                    "saw_ready": "READY" in pane_text.get("result", ""),
                    "input_accepted": input_result.get("type") == "host_call_result",
                    "session": session.get("result", {}).get("session"),
                    "broker_token_present": bool(
                        os.environ.get("VVMUX_PLUGIN_BROKER_TOKEN")
                    ),
                },
            }
        )
    elif message["type"] == "shutdown":
        write_frame({"type": "ready", "request_id": request_id})
        break
    elif message["type"] == "cancel":
        write_frame({"type": "cancelled", "request_id": request_id})
    else:
        raise RuntimeError(message["type"])
