import io
import sys
from types import SimpleNamespace

import pytest

from vvmux_plugin import (
    MAX_FRAME_BYTES,
    NativeHost,
    ProtocolError,
    read_frame,
    serve_with_events,
    write_frame,
)


def test_big_endian_frame_round_trip() -> None:
    stream = io.BytesIO()
    write_frame(stream, {"type": "ready", "request_id": 9})
    encoded = stream.getvalue()
    assert int.from_bytes(encoded[:4], "big") == len(encoded) - 4
    assert read_frame(io.BytesIO(encoded))["request_id"] == 9


def test_oversized_prefix_fails_before_body_read() -> None:
    stream = io.BytesIO((MAX_FRAME_BYTES + 1).to_bytes(4, "big"))
    with pytest.raises(ProtocolError):
        read_frame(stream)


def test_native_host_emits_and_correlates_broker_call() -> None:
    reply = io.BytesIO()
    write_frame(
        reply,
        {"type": "host_call_result", "request_id": 1, "result": {"ok": True}},
    )
    output = io.BytesIO()
    result = NativeHost(io.BytesIO(reply.getvalue()), output).call(
        "session.inspect", {}
    )
    assert result == {"ok": True}
    request = read_frame(io.BytesIO(output.getvalue()))
    assert request == {
        "type": "host_call",
        "request_id": 1,
        "method": "session.inspect",
        "params": {},
    }


def test_service_dispatches_event_frames(monkeypatch: pytest.MonkeyPatch) -> None:
    incoming = io.BytesIO()
    write_frame(incoming, {"type": "initialize", "request_id": 1})
    write_frame(
        incoming,
        {
            "type": "event",
            "request_id": 2,
            "sequence": 7,
            "name": "pane-exited",
            "payload": {"pane_id": 3},
            "context": {"causation_depth": 1},
        },
    )
    write_frame(incoming, {"type": "shutdown", "request_id": 3})
    incoming.seek(0)
    outgoing = io.BytesIO()
    monkeypatch.setattr(sys, "stdin", SimpleNamespace(buffer=incoming))
    monkeypatch.setattr(sys, "stdout", SimpleNamespace(buffer=outgoing))
    seen: list[tuple[str, object, dict[str, object]]] = []
    serve_with_events(
        "dev.events",
        "instance-a",
        lambda _action, _value, _context: {},
        lambda name, value, context: seen.append((name, value, context)),
    )
    outgoing.seek(0)
    assert read_frame(outgoing)["type"] == "hello"
    assert read_frame(outgoing) == {"type": "ready", "request_id": 1}
    assert read_frame(outgoing) == {"type": "ready", "request_id": 2}
    assert read_frame(outgoing) == {"type": "ready", "request_id": 3}
    assert seen == [("pane-exited", {"pane_id": 3}, {"causation_depth": 1})]
