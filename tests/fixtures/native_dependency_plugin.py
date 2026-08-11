#!/usr/bin/env python3
"""Protocol-1 fixture for dependency-alias-scoped plugin invocation."""

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
                "method": "plugin.invoke",
                "params": {
                    "reference": "runner/double",
                    "input": message["input"],
                },
            }
        )
        dependency = read_frame()
        write_frame(
            {
                "type": "host_call",
                "request_id": 42,
                "method": "plugin.invoke",
                "params": {"reference": "undeclared/pass", "input": {}},
            }
        )
        denied = read_frame()
        write_frame(
            {
                "type": "result",
                "request_id": request_id,
                "result": {
                    "value": dependency["result"]["value"],
                    "undeclared_denied": denied.get("code") == "scope_denied",
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
