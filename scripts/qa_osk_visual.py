#!/usr/bin/env python3
"""Pixel smoke test for the system OSK overlay (no OS input).

The OSK is a separate window, so this asks the debug build for an overlay
screenshot through replay and checks the actual rendered pixels. It catches
the class of bug where the keyboard card is too narrow and clips keys.

Checks:
- the opaque card spans the window width (not a one-third card);
- key ink is present on both the left and right sides of the QWERTY block;
- the focused key has an accent highlight on the alpha and symbol layers.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import qa_offscreen
from qa_browser_visual import pixel, read_png

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>osk visual qa</title>
<style>
  html, body { margin:0; height:2000px; background:#20242c; }
  #q { position:absolute; left:40px; top:40px; width:400px; height:60px; font-size:30px; }
</style></head>
<body><input id="q" autofocus></body></html>
"""

def accentish(color: tuple[int, int, int]) -> bool:
    """The active key fill is ACCENT @ 28% over the dark card."""
    r, g, b = color
    return b >= 80 and b - r >= 35 and b - g >= 35


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def write_plan(session: Path) -> None:
    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": (session / "osk.html").as_uri()}},
        {"app": "browser", "action": "cursor.dock", "params": {"x": 100, "y": 70}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "osk.screenshot", "params": {}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "square"}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "osk.screenshot", "params": {}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "circle"}},
    ]
    with (session / "actions.jsonl").open("w", encoding="utf-8") as out:
        for index, action in enumerate(actions, 1):
            out.write(
                json.dumps(
                    {
                        "kind": "action",
                        "flow": 1,
                        "seq": index,
                        "repro": action,
                        "requires": [],
                        "pre": {},
                    }
                )
                + "\n"
            )


def alpha_at(rgba: bytes, width: int, x: int, y: int) -> int:
    return rgba[(y * width + x) * 4 + 3]


def opaque_bounds(width: int, height: int, rgba: bytes) -> tuple[int, int, int, int]:
    min_x, min_y = width, height
    max_x = max_y = -1
    for y in range(height):
        for x in range(width):
            if alpha_at(rgba, width, x, y) >= 250:
                min_x = min(min_x, x)
                min_y = min(min_y, y)
                max_x = max(max_x, x)
                max_y = max(max_y, y)
    if max_x < min_x or max_y < min_y:
        raise ValueError("no opaque OSK card pixels")
    return min_x, min_y, max_x, max_y


def card_background(width: int, height: int, rgba: bytes, bounds: tuple[int, int, int, int]) -> tuple[int, int, int]:
    counts: dict[tuple[int, int, int], int] = {}
    x0, y0, x1, y1 = bounds
    for y in range(y0, y1 + 1, 3):
        for x in range(x0, x1 + 1, 3):
            if alpha_at(rgba, width, x, y) >= 250:
                c = pixel(rgba, width, x, y)
                counts[c] = counts.get(c, 0) + 1
    if not counts:
        raise ValueError("no opaque card pixels to sample")
    return max(counts.items(), key=lambda item: item[1])[0]


def inspect(shot: Path, label: str) -> None:
    width, height, rgba = read_png(shot)
    print(f"{label}: shot={shot} size={width}x{height}")
    if width < 900 or height < 300:
        raise ValueError(f"{label}: OSK overlay is unexpectedly small ({width}x{height})")

    x0, y0, x1, y1 = opaque_bounds(width, height, rgba)
    card_w = x1 - x0 + 1
    card_h = y1 - y0 + 1
    print(f"{label}: card=({x0},{y0})-({x1},{y1}) {card_w}x{card_h}")
    if card_w < width * 0.85:
        raise ValueError(
            f"{label}: OSK card is {card_w}px of {width}px; keyboard is clipped to one side"
        )

    bg = card_background(width, height, rgba, (x0, y0, x1, y1))
    mid_x = (x0 + x1) // 2
    left_ink = right_ink = 0
    left_accent = right_accent = 0
    for y in range(y0, y1 + 1, 2):
        for x in range(x0, x1 + 1, 2):
            if alpha_at(rgba, width, x, y) < 250:
                continue
            c = pixel(rgba, width, x, y)
            ink = sum(abs(a - b) for a, b in zip(c, bg)) > 36
            accent = accentish(c)
            if x < mid_x:
                left_ink += int(ink)
                left_accent += int(accent)
            else:
                right_ink += int(ink)
                right_accent += int(accent)
    print(
        f"{label}: ink left={left_ink} right={right_ink} "
        f"accent left={left_accent} right={right_accent}"
    )
    if left_ink < 200 or right_ink < 200:
        raise ValueError(f"{label}: key ink missing from a keyboard half")
    if left_accent + right_accent < 20:
        raise ValueError(f"{label}: focused-key highlight missing")


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-osk-visual-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    (session / "osk.html").write_text(PAGE, encoding="utf-8")
    write_plan(session)

    replay_root = Path(tempfile.gettempdir()) / "sdsc-replay"
    prefix = session.name
    before = set(replay_root.glob(f"{prefix}-*")) if replay_root.is_dir() else set()
    env = os.environ.copy()
    qa_offscreen.apply_env(env)
    print(f"session={session}")
    proc = subprocess.Popen(
        [str(exe), "--trace-replay", str(session), "--flow", "1"],
        cwd=str(root),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    offscreen = qa_offscreen.watch(proc.pid)

    sandbox = None
    done = False
    deadline = time.time() + 60
    try:
        while time.time() < deadline and proc.poll() is None:
            if replay_root.is_dir():
                candidates = set(replay_root.glob(f"{prefix}-*")) - before
                if candidates:
                    sandbox = max(candidates, key=lambda path: path.stat().st_mtime_ns)
                    manifests = list(sandbox.glob("trace/session-*/manifest.jsonl"))
                    if manifests and '"replay.done"' in manifests[0].read_text(
                        encoding="utf-8", errors="replace"
                    ):
                        done = True
                        break
            time.sleep(0.5)
        if not done:
            print("error: replay.done not observed before timeout", file=sys.stderr)
            return 1

        deadline = time.time() + 8
        shots: list[Path] = []
        while time.time() < deadline:
            shots = sorted(
                (sandbox / "shots").glob("osk-shot-*.png"),
                key=lambda path: path.stat().st_mtime_ns,
            ) if sandbox else []
            if len(shots) >= 2:
                break
            time.sleep(0.25)
        if len(shots) < 2:
            print(f"error: expected 2 OSK screenshots, got {len(shots)}", file=sys.stderr)
            return 1

        inspect(shots[0], "alpha")
        inspect(shots[1], "symbol")
        if not offscreen.report():
            return 1
        print("osk visual passed")
        return 0
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    finally:
        offscreen.stop()
        if not args.keep and proc.poll() is None:
            if os.name == "nt":
                subprocess.run(
                    ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )
            else:
                proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    raise SystemExit(main())
