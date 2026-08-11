#!/usr/bin/env python3
"""Protocol-1 service used to prove live generation drain/restart behavior."""

import json
import os
import struct
import sys
import time
from pathlib import Path


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


version = Path("VERSION").read_text().strip()
instance = os.environ["VVMUX_PLUGIN_INSTANCE"]
write_frame(
    {
        "type": "hello",
        "protocol_version": 1,
        "plugin_id": os.environ["VVMUX_PLUGIN_ID"],
        "instance_id": instance,
        "features": [],
    }
)

while True:
    message = read_frame()
    request_id = message["request_id"]
    if message["type"] == "initialize":
        write_frame({"type": "ready", "request_id": request_id})
    elif message["type"] == "invoke":
        if message["action"] == "slow":
            time.sleep(message["input"].get("seconds", 30))
            result_version = Path("VERSION").read_text().strip()
        else:
            result_version = version
        write_frame(
            {
                "type": "result",
                "request_id": request_id,
                "result": {"version": result_version, "instance": instance},
            }
        )
    elif message["type"] == "shutdown":
        write_frame({"type": "ready", "request_id": request_id})
        break
    elif message["type"] == "cancel":
        write_frame({"type": "cancelled", "request_id": request_id})
    else:
        raise RuntimeError(message["type"])
