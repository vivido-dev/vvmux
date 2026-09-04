#!/usr/bin/env python3
"""Summarize `vvmux msg layout` as one line per pane.

    vvmux msg --target SESSION layout | ./panes.py

`layout` is the only call carrying the split tree, but it also carries geometry, a neighbor graph,
and a locator for every pane. When the question is only "which pane do I target", this folds it
down to the selectors that actually work:

    TAB/PANE  SELECTOR          FLAGS  SPLIT  NEIGHBOURS        TITLE
    1/1       --pane-id 1       ---    1      r:3 d:2           zsh
    1/2       --pane-name editor -c-   2.1    u:1 r:3           nvim
    2/4       --alias reviewer  -f-    -                        codex

FLAGS are `c` caller, `f` focused, `v` visible, `z` zoomed; `-` otherwise. `visible` is about
projection, so every pane of a detached session reports false.

The SELECTOR column is the most durable handle each pane has: an agent name outlives splits and
renumbering, a pane name outlives a server restart, and a pane ID is valid only within one run of a
server. Prefer what is printed over `--pane-id` when saving or re-running anything.

A pane's agent-mesh address is `f<tab_id>p<pane_id>`, which the TAB/PANE column already spells.

`--json` prints the same rows as JSON. Everything is read-only; nothing is sent to vvmux.
"""

from __future__ import annotations

import argparse
import json
import sys

DIRECTIONS = (("left", "l"), ("right", "r"), ("up", "u"), ("down", "d"))


class LayoutError(Exception):
    """A message that is useful to the caller rather than a traceback."""


def rows(layout: dict) -> list[dict]:
    """Flatten the layout into one record per pane, in tab and pane order."""
    tabs = layout.get("tabs")
    if not isinstance(tabs, list):
        raise LayoutError("input has no 'tabs' array; expected the JSON from `vvmux msg layout`")
    out = []
    for tab in tabs:
        for pane in tab.get("panes", []):
            locator = pane.get("locator", {})
            out.append(
                {
                    "tab_id": tab.get("tab_id"),
                    "tab_name": tab.get("tab_name"),
                    "tab_active": bool(tab.get("active")),
                    "pane_id": pane.get("pane_id"),
                    "pane_name": pane.get("pane_name") or locator.get("pane_name"),
                    "agent_alias": pane.get("agent_alias"),
                    "selector": _selector(pane, locator),
                    "title": pane.get("title"),
                    "split_path": pane.get("split_path") or [],
                    "layer": pane.get("layer"),
                    "is_caller": bool(pane.get("is_caller")),
                    "focused": bool(pane.get("focused")),
                    "visible": bool(pane.get("visible")),
                    "zoomed": bool(pane.get("zoomed")),
                    "neighbors": {
                        direction: value
                        for direction, value in (pane.get("neighbors") or {}).items()
                        if value is not None
                    },
                }
            )
    return out


def _selector(pane: dict, locator: dict) -> str:
    """The most durable way to name this pane, most durable first."""
    if pane.get("agent_alias"):
        return f"--alias {pane['agent_alias']}"
    name = pane.get("pane_name") or locator.get("pane_name")
    if name:
        return f"--pane-name {name}"
    return f"--pane-id {pane.get('pane_id')}"


def flags(row: dict) -> str:
    return "".join(
        letter if row[key] else "-"
        for letter, key in (
            ("c", "is_caller"),
            ("f", "focused"),
            ("v", "visible"),
            ("z", "zoomed"),
        )
    )


def neighbours(row: dict) -> str:
    return " ".join(
        f"{short}:{row['neighbors'][direction]}"
        for direction, short in DIRECTIONS
        if direction in row["neighbors"]
    )


def _shorten(value, width: int) -> str:
    text = "" if value is None else str(value)
    return text if len(text) <= width else text[: width - 1] + "…"


def render(records: list[dict]) -> str:
    if not records:
        return "no panes"
    header = ("TAB/PANE", "SELECTOR", "FLAGS", "SPLIT", "NEIGHBOURS", "TITLE")
    lines = [
        (
            f"{record['tab_id']}/{record['pane_id']}",
            record["selector"],
            flags(record),
            ".".join(str(step) for step in record["split_path"]) or "-",
            neighbours(record) or "-",
            _shorten(record["title"], 28),
        )
        for record in records
    ]
    widths = [max(len(row[column]) for row in [header, *lines]) for column in range(len(header))]
    out = [
        "  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip()
        for row in [header, *lines]
    ]

    floating = [record for record in records if record["layer"] not in (None, "tiled")]
    if floating:
        out.append("")
        out.append(
            f"{len(floating)} pane(s) are not tiled: "
            + ", ".join(f"{r['tab_id']}/{r['pane_id']} ({r['layer']})" for r in floating)
        )
        out.append("A directional route does not reach a floating pane; name it instead.")
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--from",
        dest="source",
        default="-",
        metavar="PATH",
        help="layout JSON; '-' (the default) reads stdin",
    )
    parser.add_argument("--tab", type=int, help="only this tab, by stable tab id")
    parser.add_argument("--agents", action="store_true", help="only panes running a named agent")
    parser.add_argument("--caller", action="store_true", help="only the calling pane")
    parser.add_argument("--json", action="store_true", help="print records as JSON")
    arguments = parser.parse_args(argv)

    try:
        if arguments.source == "-":
            text = sys.stdin.read()
        else:
            with open(arguments.source, encoding="utf-8") as handle:
                text = handle.read()
        if not text.strip():
            raise LayoutError("no input; pipe `vvmux msg layout` in, or pass --from PATH")
        try:
            layout = json.loads(text)
        except json.JSONDecodeError as error:
            raise LayoutError(f"input is not JSON: {error}") from error
        if not isinstance(layout, dict):
            raise LayoutError("input must be one JSON object")
        records = rows(layout)
    except (LayoutError, OSError) as error:
        json.dump({"error": str(error)}, sys.stdout)
        sys.stdout.write("\n")
        return 2

    if arguments.tab is not None:
        records = [r for r in records if r["tab_id"] == arguments.tab]
    if arguments.agents:
        records = [r for r in records if r["agent_alias"]]
    if arguments.caller:
        records = [r for r in records if r["is_caller"]]

    if arguments.json:
        json.dump(records, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        sys.stdout.write(render(records) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
