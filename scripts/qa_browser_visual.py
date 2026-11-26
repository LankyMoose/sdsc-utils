#!/usr/bin/env python3
"""Pixel smoke test for the Browser window (no OS input).

Runs a local page through replay, asks the debug build for a window screenshot,
then checks the pixels: the card/page must show the test page's colors and the
window must not be a nearly uniform grey box. This catches widget-tree/render
regressions that behavioral traces cannot see.
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

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>visual qa</title>
<style>
  html, body { margin:0; width:100%; height:100%; background:#f5f0e6; }
  #band { position:fixed; left:0; top:0; width:100%; height:45%; background:#c0392b; }
  h1 { position:fixed; left:35%; top:52%; margin:0; font: 48px sans-serif; color:#1f2937; }
</style></head>
<body><div id="band"></div><h1>visual qa</h1></body></html>
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


def near(color: tuple[int, int, int], want: tuple[int, int, int], slop: int = 18) -> bool:
    return all(abs(a - b) <= slop for a, b in zip(color, want))


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-visual-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page = session / "visual.html"
    page.write_text(PAGE, encoding="utf-8")
    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page.as_uri()}},
        {"app": "browser", "action": "screenshot", "params": {}},
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
    env["SDSC_DEBUG_QUIET"] = "1"
    env["SDSC_DEBUG_INPUT"] = "0"
    print(f"session={session}")
    proc = subprocess.Popen(
        [str(exe), "--trace-replay", str(session), "--flow", "1"],
        cwd=str(root),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    sandbox = None
    done = False
    deadline = time.time() + 45
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
        shot = None
        while time.time() < deadline:
            shots = sorted(
                (sandbox / "shots").glob("shot-*.png"),
                key=lambda p: p.stat().st_mtime_ns,
            ) if sandbox else []
            if shots:
                shot = shots[-1]
                break
            time.sleep(0.25)
        if not shot:
            print("error: no debug screenshot was written", file=sys.stderr)
            return 1
        width, height, rgba = read_png(shot)
        print(f"shot={shot} size={width}x{height}")

        # The page is centered in a 16:9 card; the image includes the card's
        # page area. Look for both test colors in a central band, plus enough
        # color variety that a uniform grey widget-tree failure cannot pass.
        red = (192, 57, 43)
        cream = (245, 240, 230)
        found_red = found_cream = False
        colors = set()
        x0, x1 = width // 4, width * 3 // 4
        y0, y1 = height // 4, height * 3 // 4
        step = max(1, min(width, height) // 160)
        for y in range(y0, y1, step):
            for x in range(x0, x1, step):
                c = pixel(rgba, width, x, y)
                colors.add(c)
                found_red = found_red or near(c, red)
                found_cream = found_cream or near(c, cream)
        print(f"central_colors={len(colors)} red={found_red} cream={found_cream}")
        if not found_red or not found_cream:
            print(
                "error: browser page pixels are missing (grey/uniform render)",
                file=sys.stderr,
            )
            return 1
        if len(colors) < 4:
            print("error: browser render is nearly uniform", file=sys.stderr)
            return 1
        return 0
    finally:
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
