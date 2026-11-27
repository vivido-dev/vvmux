"""Crop a visible session-view pane from a local Vivido presentation using ImageMagick."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def query(*command):
    return json.loads(subprocess.check_output(command, timeout=15))


def target(session, pane_id, known_session):
    client = ("vvmux", "msg", "-t", session)
    status = query(*client, "session-inspect")
    outer = status["outer"]
    if outer is None:
        raise ValueError("no attached presenter")
    if outer["remote"] or not outer["vivido_automation_reachable"]:
        raise ValueError("no local Vivido automation route for this attachment")
    name = outer["vivido_session"]
    if name and known_session and name != known_session:
        raise ValueError("the requested Vivido session differs from the attachment")
    if not (name or known_session):
        raise ValueError("supply the known presenting --vivido-session for this attachment")
    if query(*client, "list-clients")["view"]["kind"] != "session":
        raise ValueError("capture requires the mux session view")
    pane = query(*client, "inspect", "--pane-id", str(pane_id))["pane"]
    crop = pane["outer_crop"]
    if not pane["visible"] or not crop or crop["vivido_window_id"] != outer["vivido_window_id"]:
        raise ValueError("pane has no visible crop in the presenting window")
    return (name or known_session, outer["vivido_window_id"], status["session_instance"],
            status["presenter_client_id"], status["layout_revision"], outer["cell_width"],
            outer["cell_height"], crop)


def capture(args):
    original = target(args.mux_session, args.pane_id, args.vivido_session)
    session, window, _, _, _, cw, ch, crop = original
    client = ("vivido", "msg", "-t", session)
    if args.after_frame is not None:
        query(*client, "wait", "frame", "--window-id", str(window), "--after-frame",
              str(args.after_frame), "--timeout", "10s")
    frame = query(*client, "screenshot", "--window-id", str(window), "--json")
    source = Path(frame["path"])
    try:
        if target(args.mux_session, args.pane_id, args.vivido_session) != original:
            raise ValueError("attachment or layout changed during capture")
        if frame["window_id"] != window or (frame["cell"]["width"], frame["cell"]["height"]) != (cw, ch):
            raise ValueError("presenter window or cell metrics differ from mux geometry")
        if args.after_frame is not None and frame["frame_sequence"] <= args.after_frame:
            raise ValueError("screenshot does not contain a newer frame")
        # The mux measures from the grid origin. The screenshot includes window padding.
        x = frame["padding"]["x"] + crop["x"]
        y = frame["padding"]["y"] + crop["y"]
        w, h = crop["width"], crop["height"]
        if not (int(x) == x and int(y) == y and w > 0 and h > 0 and
                0 <= x <= frame["width"] - w and 0 <= y <= frame["height"] - h):
            raise ValueError("pane crop is outside the captured physical pixel grid")
        geometry = f"{w}x{h}+{int(x)}+{int(y)}"
        fd = os.open(args.output, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        try:
            with os.fdopen(fd, "wb") as stream:
                subprocess.run(["magick", str(source), "-crop", geometry, "+repage", "png:-"],
                               stdout=stream, check=True, timeout=15)
        except BaseException:
            args.output.unlink()
            raise
        print(json.dumps({"pane_id": args.pane_id, "window_id": window,
                          "frame_sequence": frame["frame_sequence"], "geometry": geometry}))
    finally:
        source.unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mux_session")
    parser.add_argument("pane_id", type=int)
    parser.add_argument("output", type=Path, help="new PNG file")
    parser.add_argument("--vivido-session", help="known local presenter if the mux reports no name")
    parser.add_argument("--after-frame", type=int, help="previous frame sequence on this window")
    try:
        capture(parser.parse_args())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
