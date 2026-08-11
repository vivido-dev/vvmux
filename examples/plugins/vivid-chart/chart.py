#!/usr/bin/env python3
import binascii
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import zlib


def statistics(values: list[float]) -> dict[str, float | int]:
    return {
        "count": len(values),
        "minimum": min(values),
        "maximum": max(values),
    }


def chunk(kind: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", binascii.crc32(kind + data))


def chart_png(values: list[float]) -> bytes:
    width, height = 320, 120
    low, high = min(values), max(values)
    span = max(high - low, 1.0)
    points = [
        (
            round(index * (width - 1) / max(len(values) - 1, 1)),
            height - 1 - round((value - low) * (height - 1) / span),
        )
        for index, value in enumerate(values)
    ]
    pixels = bytearray([18, 24, 38, 255] * width * height)
    for x, y in points:
        for delta_y in range(-2, 3):
            for delta_x in range(-2, 3):
                px, py = x + delta_x, y + delta_y
                if 0 <= px < width and 0 <= py < height:
                    offset = (py * width + px) * 4
                    pixels[offset : offset + 4] = bytes((56, 189, 248, 255))
    rows = b"".join(b"\0" + pixels[row * width * 4 : (row + 1) * width * 4] for row in range(height))
    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


def action() -> None:
    value = json.load(sys.stdin)
    json.dump(statistics([float(item) for item in value["values"]]), sys.stdout)


def pane() -> None:
    helper = os.environ.get("VVMUX_VIVI_BIN")
    if not helper or not Path(helper).is_absolute():
        raise RuntimeError("VVMUX_VIVI_BIN is unavailable; install release-matched Vivi")
    if os.environ.get("VVMUX_VIVI_PROTOCOL_VERSION") != "1.5":
        raise RuntimeError("release-matched Vivi protocol 1.5 is required")
    with tempfile.TemporaryDirectory(prefix="vvmux-chart-") as directory:
        image = Path(directory) / "chart.png"
        image.write_bytes(chart_png([3, 5, 4, 9, 7, 12, 10]))
        subprocess.run([helper, "--inline", str(image)], check=True, shell=False)
    print("Vivid chart submitted off the PTY; type q to close.")
    for line in sys.stdin:
        if line.strip().lower() == "q":
            break


if __name__ == "__main__":
    action() if sys.argv[1:] == ["action"] else pane()
