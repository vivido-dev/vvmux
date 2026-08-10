"""Protocol-1 helpers for trusted native vvmux plugin services."""

from __future__ import annotations

import json
import struct
import sys
from collections.abc import Callable
from typing import Any, BinaryIO

PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 1024 * 1024


class ProtocolError(Exception):
    """A malformed or oversized native protocol frame."""


def read_frame(stream: BinaryIO) -> dict[str, Any]:
    prefix = stream.read(4)
    if len(prefix) != 4:
        raise EOFError("native plugin stream ended before a frame prefix")
    (length,) = struct.unpack(">I", prefix)
    if length > MAX_FRAME_BYTES:
        raise ProtocolError("native plugin frame exceeds 1 MiB")
    body = stream.read(length)
    if len(body) != length:
        raise EOFError("native plugin stream ended within a frame")
    value = json.loads(body)
    if not isinstance(value, dict):
        raise ProtocolError("native plugin frame must contain a JSON object")
    return value


def write_frame(stream: BinaryIO, value: dict[str, Any]) -> None:
    body = json.dumps(value, separators=(",", ":")).encode()
    if len(body) > MAX_FRAME_BYTES:
        raise ProtocolError("native plugin frame exceeds 1 MiB")
    stream.write(struct.pack(">I", len(body)))
    stream.write(body)
    stream.flush()


def serve(
    plugin_id: str,
    instance_id: str,
    handler: Callable[[str, Any, dict[str, Any]], Any],
) -> None:
    """Serve serialized invocations on stdin/stdout; use stderr only for logs."""

    reader = sys.stdin.buffer
    writer = sys.stdout.buffer
    write_frame(
        writer,
        {
            "type": "hello",
            "protocol_version": PROTOCOL_VERSION,
            "plugin_id": plugin_id,
            "instance_id": instance_id,
            "features": [],
        },
    )
    while True:
        message = read_frame(reader)
        request_id = message.get("request_id")
        match message.get("type"):
            case "initialize":
                write_frame(writer, {"type": "ready", "request_id": request_id})
            case "invoke":
                try:
                    result = handler(message["action"], message["input"], message["context"])
                    write_frame(
                        writer,
                        {"type": "result", "request_id": request_id, "result": result},
                    )
                except Exception as error:  # plugin errors cross as typed data
                    write_frame(
                        writer,
                        {
                            "type": "error",
                            "request_id": request_id,
                            "code": "runtime_crashed",
                            "message": str(error),
                        },
                    )
            case "cancel":
                write_frame(writer, {"type": "cancelled", "request_id": request_id})
            case "shutdown":
                write_frame(writer, {"type": "ready", "request_id": request_id})
                return
            case other:
                raise ProtocolError(f"unexpected native plugin message: {other!r}")


__all__ = [
    "MAX_FRAME_BYTES",
    "PROTOCOL_VERSION",
    "ProtocolError",
    "read_frame",
    "serve",
    "write_frame",
]
