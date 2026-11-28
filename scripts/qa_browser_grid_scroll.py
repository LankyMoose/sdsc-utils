#!/usr/bin/env python3
"""Browser tabs row QA (newest-first strip, scrollbar, carry-over).

Seeds more tabs than fit on screen, checks the newest tab opens at the start
of the strip with a visible horizontal scrollbar, holds the left stick right
until the repeat scrolls the oldest tab into view, then returns Home and
checks that the focused tab was carried into tabs.json.
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
from qa_browser_grid import (
    PAGE,
    central_band_colors,
    grid_card_rects,
    has_color,
    read_png,
    region_colors,
)

RED = (192, 57, 43)
BLUE = (31, 95, 191)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def bright_pixels(colors: dict[tuple[int, int, int], int], threshold: int = 70) -> int:
    return sum(
        count
        for (red, green, _blue), count in colors.items()
        if red > threshold and green > threshold
    )


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-grid-scroll" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page_a = session / "a.html"
    page_b = session / "b.html"
    page_a.write_text(
        PAGE.format(name="row A", band="#c0392b", favicon="data:,"), encoding="utf-8"
    )
    page_b.write_text(
        PAGE.format(name="row B", band="#1f5fbf", favicon="data:,"), encoding="utf-8"
    )
    red = page_a.as_uri()
    blue = page_b.as_uri()

    def step(action: str, **params: object) -> dict[str, object]:
        return {"app": "browser", "action": action, "params": params}

    actions: list[dict[str, object]] = [step("navigate", url=red), step("wait")]
    for _ in range(7):
        actions += [step("tab.new", url=red), step("wait")]
    actions += [
        # Newest tab first: blue opens at the start of the strip.
        step("tab.new", url=blue),
        step("wait"),
        step("grid.open"),
        step("wait"),
        step("screenshot"),
        # Hold the left stick right: the grid frame tick must repeat past a
        # single card and scroll the oldest tab into view. Four screenshots
        # span ~1s (First + ~8 repeats at the MENU cadence).
        step("pad.stick", x=1.0),
        step("screenshot"),
        step("screenshot"),
        step("screenshot"),
        step("screenshot"),
        step("pad.stick", x=0.0),
        step("wait"),
        step("screenshot"),
        step("screen.home"),
        step("wait"),
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
    deadline = time.time() + 180
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

        shots: list[Path] = []
        deadline = time.time() + 10
        while time.time() < deadline:
            shots = (
                sorted(
                    (sandbox / "shots").glob("shot-*.png"),
                    key=lambda p: p.stat().st_mtime_ns,
                )
                if sandbox
                else []
            )
            if len(shots) >= 6:
                break
            time.sleep(0.25)
        if len(shots) < 6:
            print(f"error: expected 6 screenshots, found {len(shots)}", file=sys.stderr)
            return 1
        print(f"sandbox={sandbox}")
        print("shots=" + ", ".join(path.name for path in shots))

        images = [read_png(path) for path in shots]
        width, height = images[0][0], images[0][1]
        cards = grid_card_rects(width, height)
        card_w = cards[0][2]
        row_right = cards[3][0] + cards[3][2]
        lane = (cards[0][0], cards[0][1] + cards[0][3] + 1, row_right - cards[0][0], 7)
        opening_newest = central_band_colors(
            images[0][2],
            width,
            height,
            (cards[1][0], cards[1][1], cards[1][2], cards[1][3]),
        )
        opening_plus = region_colors(
            images[0][2],
            width,
            (cards[0][0], cards[0][1], cards[0][2], cards[0][3]),
        )
        lane_bright = bright_pixels(region_colors(images[0][2], width, lane))
        scrolled_right = central_band_colors(
            images[4][2],
            width,
            height,
            (row_right - card_w // 2, cards[0][1], card_w // 2, cards[0][3]),
        )
        scrolled_left = central_band_colors(
            images[4][2],
            width,
            height,
            (cards[0][0], cards[0][1], cards[0][2], cards[0][3]),
        )
        released_left = central_band_colors(
            images[5][2],
            width,
            height,
            (cards[0][0], cards[0][1], cards[0][2], cards[0][3]),
        )
        checks = [
            ("newest tab opens first in the strip", has_color(opening_newest, BLUE)),
            (
                "the plus card stays at the start",
                has_color(opening_plus, (65, 65, 251), 55, 1),
            ),
            ("overflow shows the horizontal scrollbar", lane_bright > 20),
            ("held stick repeats past the first card", not has_color(scrolled_left, BLUE)),
            ("the oldest tab scrolls into view at the end", has_color(scrolled_right, RED)),
            (
                "releasing the stick stays at the end",
                not has_color(released_left, BLUE),
            ),
        ]
        for label, ok in checks:
            print(f"check {label}: {'ok' if ok else 'FAIL'}")
        if not all(ok for _, ok in checks):
            print("error: tabs row checks failed", file=sys.stderr)
            return 1

        tabs_path = sandbox / "tabs.json" if sandbox else None
        if not tabs_path or not tabs_path.is_file():
            print("error: tabs.json missing", file=sys.stderr)
            return 1
        saved = json.loads(tabs_path.read_text(encoding="utf-8"))
        active = saved.get("active", 0)
        # The focused oldest tab is the boot tab at order index 0.
        print(f"check carried active tab: {active} ({'ok' if active == 0 else 'FAIL'})")
        if active != 0:
            print("error: Circle->Home did not persist the focused tab", file=sys.stderr)
            return 1
        if not offscreen.report():
            return 1
        return 0
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
