#!/usr/bin/env python3
"""Browser tabs-view paging QA (synthetic replay, no OS input).

Seeds two pages worth of tabs, checks the 12th-tab side gutters and page
pills, walks L1/R1 paging, then closes back to one page and re-adds a tab so
the gutter/card-size tween is exercised in both directions.
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
    browser_rects,
    central_band_colors,
    grid_card_rects,
    has_color,
    read_png,
    region_colors,
)

RED = (192, 57, 43)
BLUE = (31, 95, 191)
ACCENT = (65, 65, 251)
SIDE = 64.0
PILL_W = 58.0
PILL_H = 30.0
PILL_PAD = 18.0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def pill_rects(
    width: int, height: int
) -> tuple[tuple[int, int, int, int], tuple[int, int, int, int]]:
    _, (tx, _, tw, _) = browser_rects(width, height)
    cards = grid_card_rects(width, height, SIDE)
    grid_y = cards[0][1]
    grid_bottom = cards[8][1] + cards[8][3]
    cy = int((grid_y + grid_bottom) / 2 - PILL_H / 2)
    prev = (int(tx + PILL_PAD), cy, int(PILL_W), int(PILL_H))
    nxt = (int(tx + tw - PILL_PAD - PILL_W), cy, int(PILL_W), int(PILL_H))
    return prev, nxt


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-grid-pages" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page_a = session / "a.html"
    page_b = session / "b.html"
    page_a.write_text(PAGE.format(name="page A", band="#c0392b"), encoding="utf-8")
    page_b.write_text(PAGE.format(name="page B", band="#1f5fbf"), encoding="utf-8")
    red = page_a.as_uri()
    blue = page_b.as_uri()

    def step(action: str, **params: object) -> dict[str, object]:
        return {"app": "browser", "action": action, "params": params}

    actions: list[dict[str, object]] = [
        step("navigate", url=red),
        step("wait"),
        step("screenshot"),
    ]
    # Boot tab is red; ten more red tabs fill page 1, then the 12th is blue.
    # Let each new tab paint once: background tabs keep their last frame, so
    # the grid thumbnails are real page bands instead of URL placeholders.
    for _ in range(10):
        actions.append(step("tab.new", url=red))
        actions.append(step("wait"))
    actions.append(step("tab.new", url=blue))
    actions.append(step("wait"))
    actions += [
        step("wait"),
        step("grid.open"),  # active tab is the blue 12th -> page 2
        step("wait"),
        step("screenshot"),
        step("grid.page_prev"),
        step("wait"),
        step("screenshot"),
        step("grid.page_next"),
        step("wait"),
        step("screenshot"),
        step("grid.page_prev"),
        step("wait"),
        step("grid.close_tab", index=0),  # 11 tabs -> one page
        step("wait"),
        step("screenshot"),
        step("tab.new", url=blue),  # grid stays open; 12 tabs -> two pages
        step("wait"),
        step("screenshot"),
        step("grid.page_next"),
        step("wait"),
        step("screenshot"),
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
            if len(shots) >= 7:
                break
            time.sleep(0.25)
        if len(shots) < 7:
            print(f"error: expected 7 screenshots, found {len(shots)}", file=sys.stderr)
            return 1
        print(f"sandbox={sandbox}")
        print("shots=" + ", ".join(path.name for path in shots))

        images = [read_png(path) for path in shots]
        width, height = images[0][0], images[0][1]
        if any(img[0] != width or img[1] != height for img in images):
            print("error: screenshots have different window sizes", file=sys.stderr)
            return 1
        print(f"window={width}x{height}")

        two_cards = grid_card_rects(width, height, SIDE)
        one_cards = grid_card_rects(width, height, 0.0)
        prev_pill, next_pill = pill_rects(width, height)
        print(f"two-page cards={two_cards[:2]} one-page cards={one_cards[:2]}")

        def band(
            shot: int, rect: tuple[int, int, int, int]
        ) -> dict[tuple[int, int, int], int]:
            return central_band_colors(images[shot][2], width, height, rect)

        def accent(shot: int, rect: tuple[int, int, int, int]) -> bool:
            colors = region_colors(images[shot][2], width, rect)
            return has_color(colors, ACCENT, 55, 1)

        # Shot 1: opened grid, page 2 (blue + plus) with two-page gutters.
        page2_blue = band(1, two_cards[1])
        # Shot 2: page 1 fully populated with red tabs, next pill enabled.
        page1_first = band(2, two_cards[1])
        page1_last = band(2, two_cards[11])
        # Shot 3: back on page 2.
        page2_again = band(3, two_cards[1])
        # Shot 4: 11 tabs -> one page, no side gutter.
        one_page_red = band(4, one_cards[1])
        one_page_blue = band(4, one_cards[11])
        # Shot 5: tab added while open -> two pages, gutter returns.
        readded_red = band(5, two_cards[1])
        readded_blue = band(5, two_cards[11])
        # Shot 6: page 2 after the manual add.
        readded_page2 = band(6, two_cards[1])

        checks = [
            ("page 2 starts with the blue tab", has_color(page2_blue, BLUE)),
            ("page 1 first card is red", has_color(page1_first, RED)),
            ("page 1 last card is red", has_color(page1_last, RED)),
            ("page 2 survived paging", has_color(page2_again, BLUE)),
            ("one-page grid first card is red", has_color(one_page_red, RED)),
            ("one-page grid ends with the blue tab", has_color(one_page_blue, BLUE)),
            (
                "one-page grid drops the side gutter",
                one_cards[0][0] < two_cards[0][0] - 40,
            ),
            (
                "one-page grid widens the cards",
                one_cards[1][2] > two_cards[1][2] + 8,
            ),
            ("page 2 enables the L1 pill", accent(1, prev_pill)),
            ("page 1 enables the R1 pill", accent(2, next_pill)),
            ("page 1 disables the L1 pill", not accent(2, prev_pill)),
            ("page 2 disables the R1 pill", not accent(1, next_pill)),
            (
                "readded tab restores the gutter",
                bool(readded_red) and two_cards[0][0] > one_cards[0][0] + 40,
            ),
            ("readded tab sits on page 2", has_color(readded_page2, BLUE)),
        ]
        for label, ok in checks:
            print(f"check {label}: {'ok' if ok else 'FAIL'}")
        if not all(ok for _, ok in checks):
            print("error: tabs paging checks failed", file=sys.stderr)
            return 1

        log = (sandbox / "app.log").read_text(encoding="utf-8", errors="replace")
        browser_trace = next(
            sandbox.glob("trace/session-*/browser.jsonl"), None
        )
        trace_text = (
            browser_trace.read_text(encoding="utf-8", errors="replace")
            if browser_trace
            else ""
        )
        checks = [("browser grid entered", "browser grid entered" in log)]
        checks += [
            (wanted, wanted in trace_text)
            for wanted in (
                "browser.grid.page_next",
                "browser.grid.page_prev",
            )
        ]
        for label, ok in checks:
            print(f"check log {label}: {'ok' if ok else 'FAIL'}")
            if not ok:
                print("error: missing grid paging log line", file=sys.stderr)
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
