import json
import os
import struct
import sys
import time


SESSION = os.environ["VVMUX_SESSION"]
INSTANCE = os.environ["VVMUX_PLUGIN_INSTANCE"]


def read_frame():
    prefix = sys.stdin.buffer.read(4)
    if len(prefix) != 4:
        raise EOFError
    size = struct.unpack(">I", prefix)[0]
    return json.loads(sys.stdin.buffer.read(size))


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
        "features": ["events"],
    }
)
while True:
    message = read_frame()
    request_id = message["request_id"]
    if message["type"] == "initialize":
        with open(f"activated-{SESSION}", "w", encoding="utf-8") as marker:
            marker.write(os.environ["VVMUX_SESSION_INSTANCE"])
        write_frame({"type": "ready", "request_id": request_id})
    elif message["type"] == "event":
        with open(f"events-{SESSION}.ndjson", "a", encoding="utf-8") as events:
            events.write(json.dumps(message, separators=(",", ":")) + "\n")
        if os.path.exists("slow-event-ms"):
            with open("slow-event-ms", "r", encoding="utf-8") as delay:
                time.sleep(int(delay.read().strip()) / 1000)
        write_frame({"type": "ready", "request_id": request_id})
    elif message["type"] == "invoke":
        if message["action"] == "crash":
            os._exit(17)
        write_frame(
            {
                "type": "result",
                "request_id": request_id,
                "result": {"session": SESSION, "instance": INSTANCE},
            }
        )
    elif message["type"] == "cancel":
        write_frame({"type": "cancelled", "request_id": request_id})
    elif message["type"] == "shutdown":
        write_frame({"type": "ready", "request_id": request_id})
        break
    else:
        raise RuntimeError(message["type"])
