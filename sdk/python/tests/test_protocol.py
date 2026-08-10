import io

import pytest

from vvmux_plugin import MAX_FRAME_BYTES, ProtocolError, read_frame, write_frame


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
