#!/usr/bin/env python3
"""Browser tab-grid QA (synthetic replay, no OS input).

Opens the Share/Create grid over local pages, verifies card geometry and the
bookmark star, creates a second tab, and switches back through the grid.
Screenshots are mapped to replay steps by write order.
"""

from __future__ import annotations

import argparse
import json
import os
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from pathlib import Path

import qa_offscreen

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>{name}</title>
<style>
  html, body {{ margin:0; width:100%; height:100%; background:#f5f0e6; }}
  #band {{ position:fixed; left:0; top:0; width:100%; height:55%;
          background:{band}; }}
  h1 {{ position:fixed; left:30%; top:65%; margin:0;
        font:48px sans-serif; color:#1f2937; }}
</style></head>
<body><div id="band"></div><h1>{name}</h1></body></html>
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def read_png(path: Path) -> tuple[int, int, bytes]:
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG")
    pos = 8
    width = height = bit_depth = color_type = interlace = None
    idat = bytearray()
    while pos + 12 <= len(data):
        length = int.from_bytes(data[pos : pos + 4], "big")
        kind = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            width, height, bit_depth, color_type, _, _, interlace = struct.unpack(
                ">IIBBBBB", chunk
            )
        elif kind == b"IDAT":
            idat += chunk
        elif kind == b"IEND":
            break
    if bit_depth != 8 or color_type != 6 or interlace != 0:
        raise ValueError("expected 8-bit RGBA PNG")
    raw = zlib.decompress(bytes(idat))
    bpp = 4
    stride = width * bpp
    out = bytearray()
    prev = bytearray(stride)
    src = 0
    for _ in range(height):
        filter_type = raw[src]
        src += 1
        line = bytearray(raw[src : src + stride])
        src += stride
        if filter_type == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 0xFF
        elif filter_type == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif filter_type == 3:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif filter_type == 4:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        elif filter_type != 0:
            raise ValueError(f"unsupported PNG filter {filter_type}")
        out += line
        prev = line
    return width, height, bytes(out)


def pixel(rgba: bytes, width: int, x: int, y: int) -> tuple[int, int, int]:
    i = (y * width + x) * 4
    return rgba[i], rgba[i + 1], rgba[i + 2]


def near(color: tuple[int, int, int], want: tuple[int, int, int], slop: int = 24) -> bool:
    return all(abs(a - b) <= slop for a, b in zip(color, want))


def browser_rects(width: int, height: int) -> tuple[tuple[int, int, int, int], tuple[int, int, int, int]]:
    """Browser page rect and the headerless tabs-view area."""
    outer = 50.0
    card_pad = 4.0
    header = 45.0
    avail_w = max(width - 2 * outer, 320.0)
    avail_h = max(height - 2 * outer, 240.0)
    card_h = avail_h
    card_w = card_h * 16 / 9
    if card_w > avail_w:
        card_w = avail_w
        card_h = card_w * 9 / 16
    page_w = round(card_w - 2 * card_pad)
    page_h = round(card_h - 2 * card_pad - header)
    card_x = (width - round(card_w)) / 2
    card_y = (height - round(card_h)) / 2
    page = (int(card_x + card_pad), int(card_y + card_pad + header), page_w, page_h)
    tabs = (page[0], int(card_y + card_pad), page_w, page_h + int(header))
    return page, tabs


def grid_card_rects(
    width: int, height: int
) -> list[tuple[int, int, int, int]]:
    """Compact tabs card: one horizontal row of four page-aspect cards."""
    page, _ = browser_rects(width, height)
    px, py, pw, ph = page
    pad = 4.0
    grid_pad = 18.0
    gap = 14.0
    bookmark_h = 88.0
    island_gap = 10.0
    island_h = 44.0
    scrollbar_lane = 9.0
    page_aspect = pw / ph
    card_total_w = round((pw + 2 * pad) * 0.70)
    area_w = card_total_w - 2 * pad
    row_w = area_w - 2 * grid_pad
    card_w = max((row_w - 3 * gap) / 4, 150.0)
    card_h = card_w / page_aspect
    area_h = (
        grid_pad
        + card_h
        + scrollbar_lane
        + gap
        + bookmark_h
        + island_gap
        + island_h
        + grid_pad
    )
    card_x = (width - card_total_w) / 2
    card_y = (height - (area_h + 2 * pad)) / 2
    row_x = card_x + pad + grid_pad
    row_y = card_y + pad + grid_pad
    rects = []
    for index in range(12):
        rects.append(
            (
                int(row_x + index * (card_w + gap)),
                int(row_y),
                int(card_w),
                int(card_h),
            )
        )
    return rects


def central_band_colors(
    rgba: bytes, width: int, height: int, rect: tuple[int, int, int, int]
) -> dict[tuple[int, int, int], int]:
    x, y, w, h = rect
    counts: dict[tuple[int, int, int], int] = {}
    step = max(2, min(w, h) // 24)
    for yy in range(y + h // 4, y + 3 * h // 4, step):
        for xx in range(x + w // 4, x + 3 * w // 4, step):
            c = pixel(rgba, width, xx, yy)
            counts[c] = counts.get(c, 0) + 1
    return counts


def region_colors(
    rgba: bytes, width: int, rect: tuple[int, int, int, int], step: int = 1
) -> dict[tuple[int, int, int], int]:
    x, y, w, h = rect
    counts: dict[tuple[int, int, int], int] = {}
    for yy in range(y, y + h, step):
        for xx in range(x, x + w, step):
            c = pixel(rgba, width, xx, yy)
            counts[c] = counts.get(c, 0) + 1
    return counts


def has_color(
    colors: dict[tuple[int, int, int], int],
    want: tuple[int, int, int],
    slop: int = 28,
    minimum: int = 2,
) -> bool:
    return sum(n for c, n in colors.items() if near(c, want, slop)) >= minimum


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-grid-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page_a = session / "a.html"
    page_b = session / "b.html"
    page_a.write_text(
        PAGE.format(name="grid A", band="#c0392b"), encoding="utf-8"
    )
    page_b.write_text(
        PAGE.format(name="grid B", band="#1f5fbf"), encoding="utf-8"
    )
    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page_a.as_uri()}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "grid.open", "params": {}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "grid.bookmark", "params": {"index": 0}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {
            "app": "browser",
            "action": "grid.new_tab",
            "params": {"url": page_b.as_uri()},
        },
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "grid.open", "params": {}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "grid.activate", "params": {"index": 1}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "grid.open", "params": {}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "grid.close_tab", "params": {"index": 0}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "screen.ps", "params": {}},
        {"app": "browser", "action": "screen.ps", "params": {}},
        {"app": "browser", "action": "wait", "params": {}},
        {"app": "browser", "action": "screenshot", "params": {}},
        {"app": "browser", "action": "screen.home", "params": {}},
        {"app": "browser", "action": "screen.ps", "params": {}},
        {"app": "launcher", "action": "open", "params": {}},
        {"app": "launcher", "action": "screen.ps", "params": {}},
        {"app": "launcher", "action": "screen.ps", "params": {}},
        {"app": "launcher", "action": "wait", "params": {}},
        {"app": "launcher", "action": "screen.home", "params": {}},
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
    deadline = time.time() + 90
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
            shots = sorted(
                (sandbox / "shots").glob("shot-*.png"),
                key=lambda p: p.stat().st_mtime_ns,
            ) if sandbox else []
            if len(shots) >= 9:
                break
            time.sleep(0.25)
        if len(shots) < 8:
            print(
                f"error: expected 9 screenshots, found {len(shots)}",
                file=sys.stderr,
            )
            return 1
        print(f"sandbox={sandbox}")
        print("shots=" + ", ".join(path.name for path in shots))

        images = [read_png(path) for path in shots]
        widths = {img[0] for img in images}
        heights = {img[1] for img in images}
        if len(widths) != 1 or len(heights) != 1:
            print("error: screenshots have different window sizes", file=sys.stderr)
            return 1
        width, height = images[0][0], images[0][1]
        print(f"window={width}x{height}")
        cards = grid_card_rects(width, height)
        print(f"cards={cards}")

        red = (192, 57, 43)
        blue = (31, 95, 191)
        cream = (245, 240, 230)
        gold = (242, 193, 78)

        page_a_colors = central_band_colors(images[0][2], width, height, browser_rects(width, height)[0])
        grid_colors = central_band_colors(images[1][2], width, height, cards[1])
        star_x, star_y, star_w, _ = cards[1]
        star_colors = region_colors(
            images[2][2],
            width,
            (star_x + star_w - 34, star_y + 4, 30, 30),
        )
        page_b_colors = central_band_colors(images[3][2], width, height, browser_rects(width, height)[0])
        two_grid = central_band_colors(images[4][2], width, height, cards[1])
        two_grid_second = central_band_colors(images[4][2], width, height, cards[2])
        page_a_again = central_band_colors(images[5][2], width, height, browser_rects(width, height)[0])
        closed_active = central_band_colors(images[7][2], width, height, cards[1])
        resumed_grid = central_band_colors(images[8][2], width, height, cards[1])

        plus_w, plus_h = cards[0][2], cards[0][3]
        tab_w, tab_h = cards[1][2], cards[1][3]
        checks = [
            ("plus card is first", cards[0][0] < cards[1][0] and cards[0][1] == cards[1][1]),
            (
                "plus card matches tab aspect",
                abs(plus_w - tab_w) <= 1 and abs(plus_h - tab_h) <= 1,
            ),
            ("page A band", has_color(page_a_colors, red)),
            ("page A cream", has_color(page_a_colors, cream)),
            ("grid active card content", has_color(grid_colors, red) or has_color(grid_colors, cream)),
            ("bookmark star filled", has_color(star_colors, gold, 42, 1)),
            ("page B band", has_color(page_b_colors, blue)),
            ("two-card grid first tab", has_color(two_grid, blue)),
            ("two-card grid second tab", has_color(two_grid_second, red)),
            ("switched page A band", has_color(page_a_again, red)),
            ("closing a stored tab keeps the active page", has_color(closed_active, red)),
            ("PS resume restored the grid", has_color(resumed_grid, red) or has_color(resumed_grid, cream)),
        ]
        for label, ok in checks:
            print(f"check {label}: {'ok' if ok else 'FAIL'}")
        if not all(ok for _, ok in checks):
            print("error: tab-grid pixel checks failed", file=sys.stderr)
            return 1

        platform = next(sandbox.glob("trace/session-*/platform.jsonl"), None) if sandbox else None
        events = []
        if platform and platform.is_file():
            for line in platform.read_text(encoding="utf-8", errors="replace").splitlines():
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        seen = {record.get("event") for record in events}
        for wanted in ("screen.dismiss", "screen.resume", "screen.home"):
            ok = wanted in seen
            print(f"check trace {wanted}: {'ok' if ok else 'FAIL'}")
            if not ok:
                print("error: screen transition trace missing", file=sys.stderr)
                return 1
        app_log = sandbox / "app.log" if sandbox else None
        log_text = app_log.read_text(encoding="utf-8", errors="replace") if app_log else ""
        for wanted in ("ui-diag: launcher immersive enter", "ui-diag: launcher close"):
            ok = wanted in log_text
            print(f"check log {wanted}: {'ok' if ok else 'FAIL'}")
            if not ok:
                print("error: Launcher PS/Circle path did not run", file=sys.stderr)
                return 1
        windows = qa_offscreen.visible_windows(proc.pid)
        print(f"post-circle windows={len(windows)}")
        if not windows:
            print("error: Circle did not leave the Home screen open", file=sys.stderr)
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
