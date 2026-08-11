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


class HostCallError(Exception):
    """A typed rejection returned by the vvmux plugin broker."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


class NativeHost:
    """Scoped broker client available only while a service invocation is active."""

    def __init__(self, reader: BinaryIO, writer: BinaryIO) -> None:
        self._reader = reader
        self._writer = writer
        self._next_request_id = 1

    def call(self, method: str, params: Any) -> Any:
        request_id = self._next_request_id
        self._next_request_id += 1
        write_frame(
            self._writer,
            {
                "type": "host_call",
                "request_id": request_id,
                "method": method,
                "params": params,
            },
        )
        reply = read_frame(self._reader)
        if reply.get("request_id") != request_id:
            raise ProtocolError("host call reply ID does not match")
        match reply.get("type"):
            case "host_call_result":
                return reply.get("result")
            case "host_call_error":
                raise HostCallError(str(reply.get("code")), str(reply.get("message")))
            case other:
                raise ProtocolError(f"unexpected host call reply: {other!r}")


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

    serve_with_host(
        plugin_id,
        instance_id,
        lambda action, value, context, _host: handler(action, value, context),
    )


def serve_with_host(
    plugin_id: str,
    instance_id: str,
    handler: Callable[[str, Any, dict[str, Any], NativeHost], Any],
) -> None:
    """Serve serialized invocations with access to scoped brokered host calls."""

    serve_with_host_and_events(
        plugin_id, instance_id, handler, lambda _name, _value, _context, _host: None
    )


def serve_with_events(
    plugin_id: str,
    instance_id: str,
    handler: Callable[[str, Any, dict[str, Any]], Any],
    event_handler: Callable[[str, Any, dict[str, Any]], None],
) -> None:
    """Serve serialized invocations and manifest event hooks."""

    serve_with_host_and_events(
        plugin_id,
        instance_id,
        lambda action, value, context, _host: handler(action, value, context),
        lambda name, value, context, _host: event_handler(name, value, context),
    )


def serve_with_host_and_events(
    plugin_id: str,
    instance_id: str,
    handler: Callable[[str, Any, dict[str, Any], NativeHost], Any],
    event_handler: Callable[[str, Any, dict[str, Any], NativeHost], None],
) -> None:
    """Serve actions and events with access to scoped brokered host calls."""

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
                    host = NativeHost(reader, writer)
                    result = handler(
                        message["action"], message["input"], message["context"], host
                    )
                    write_frame(
                        writer,
                        {"type": "result", "request_id": request_id, "result": result},
                    )
                except Exception as error:  # noqa: BLE001 - plugin errors cross as typed data
                    write_frame(
                        writer,
                        {
                            "type": "error",
                            "request_id": request_id,
                            "code": (
                                error.code
                                if isinstance(error, HostCallError)
                                else "runtime_crashed"
                            ),
                            "message": str(error),
                        },
                    )
            case "event":
                try:
                    host = NativeHost(reader, writer)
                    event_handler(
                        str(message["name"]),
                        message["payload"],
                        message["context"],
                        host,
                    )
                    write_frame(writer, {"type": "ready", "request_id": request_id})
                except Exception as error:  # noqa: BLE001 - plugin errors cross as typed data
                    write_frame(
                        writer,
                        {
                            "type": "error",
                            "request_id": request_id,
                            "code": (
                                error.code
                                if isinstance(error, HostCallError)
                                else "runtime_crashed"
                            ),
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
    "HostCallError",
    "NativeHost",
    "ProtocolError",
    "read_frame",
    "serve",
    "serve_with_events",
    "serve_with_host",
    "serve_with_host_and_events",
    "write_frame",
]
