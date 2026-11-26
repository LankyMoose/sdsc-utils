#!/usr/bin/env python3
"""D-pad cursor nudge QA (no OS input).

Local 2x2 link page -> synthetic replay -> real browser/cursor handlers.
Asserts:
  * four directional jumps actually fly (not silent no-ops);
  * every flight lands on the previous target before the next press;
  * the first tween frame after a jump starts near zero, not with a stale
    idle-time step.

The last point is the regression guard for demand-driven cursor clocks: a
150ms tween that inherits 100ms of pre-press idle time starts 74% across.
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

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>cursor qa</title>
<style>
  html, body { margin:0; width:100%; height:100%; background:#f4f1ea; }
  a { position:absolute; width:200px; height:40px; display:block;
      background:#34495e; color:#fff; font:20px sans-serif; line-height:40px;
      text-align:center; text-decoration:none; }
  #a { left:80px;  top:80px; }
  #b { left:340px; top:80px; }
  #c { left:80px;  top:320px; }
  #d { left:340px; top:320px; }
</style></head>
<body>
<a id="a" href="#a">A</a>
<a id="b" href="#b">B</a>
<a id="c" href="#c">C</a>
<a id="d" href="#d">D</a>
</body></html>
"""

# Wide top bar: the far-right link is >500px from the left pair, and the
# decoy sits down-left of it. The DOM cull used to keep only the decoy as the
# "nearest left" target, which the cursor's directional test then rejected
# (out of the row band), so Left stopped working after a Right nudge.
PAGE_ROW = """<!doctype html>
<html><head><meta charset="utf-8"><title>cursor row qa</title>
<style>
  html, body { margin:0; width:100%; height:100%; background:#f4f1ea; }
  a { position:absolute; display:block; height:26px; color:#fff;
      font:16px sans-serif; line-height:26px; text-align:center;
      text-decoration:none; background:#34495e; }
  #l1 { left:21px; top:17px; width:47px; }
  #l2 { left:78px; top:17px; width:43px; }
  #r1 { left:1484px; top:17px; width:34px; height:24px; line-height:24px; }
  #r2 { left:1533px; top:17px; width:43px; height:24px; line-height:24px; }
  #decoy { left:1105px; top:384px; width:97px; height:36px; line-height:36px; }
</style></head>
<body>
<a id="l1" href="#l1">L1</a>
<a id="l2" href="#l2">L2</a>
<a id="r1" href="#r1">R1</a>
<a id="r2" href="#r2">R2</a>
<a id="decoy" href="#decoy">D</a>
</body></html>
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def read_jsonl(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    rows = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return rows


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-cursor-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page = session / "cursor.html"
    page.write_text(PAGE, encoding="utf-8")
    row_page = session / "cursor-row.html"
    row_page.write_text(PAGE_ROW, encoding="utf-8")
    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page.as_uri()}},
        {"app": "browser", "action": "cursor.dock", "params": {"x": 150, "y": 100}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "right"}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "down"}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "left"}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "up"}},
        {"app": "browser", "action": "navigate", "params": {"url": row_page.as_uri()}},
        {"app": "browser", "action": "cursor.dock", "params": {"x": 100, "y": 30}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "right"}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "left"}},
        {"app": "browser", "action": "wait", "params": {}},
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
    try:
        deadline = time.time() + 45
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
            time.sleep(0.25)
        if not done or sandbox is None:
            print("error: replay.done not observed before timeout", file=sys.stderr)
            return 1

        trace_dir = next(sandbox.glob("trace/session-*"), None)
        browser = read_jsonl(trace_dir / "browser.jsonl") if trace_dir else []
        platform = read_jsonl(trace_dir / "platform.jsonl") if trace_dir else []
        jumps = [row for row in browser if row.get("event") == "cursor.jump"]
        frames = [row for row in browser if row.get("event") == "cursor.frame"]
        timed = sorted(
            (
                row
                for row in browser
                if row.get("event") in ("cursor.jump", "cursor.frame")
            ),
            key=lambda row: row.get("seq", 0),
        )
        timeouts = [row for row in platform if row.get("event") == "replay.settle_timeout"]
        print(f"sandbox={sandbox}")
        print(f"jumps={len(jumps)} frames={len(frames)} timeouts={len(timeouts)}")
        if len(jumps) != 6:
            print(f"error: expected 6 D-pad jumps, got {len(jumps)}", file=sys.stderr)
            return 1
        if len(frames) < 12:
            print(f"error: expected tween frames, got {len(frames)}", file=sys.stderr)
            return 1
        if timeouts:
            print(
                f"error: replay settle timed out {len(timeouts)} time(s)",
                file=sys.stderr,
            )
            return 1

        # The wide top-bar flow: Right must reach the far right link and Left
        # must come back to the same row's left link, not a no-op.
        right, left = jumps[-2], jumps[-1]
        if right.get("dir") != "Right" or float(right.get("to_x", 0)) < 1400:
            print(f"error: row Right nudge did not reach the far link: {right}", file=sys.stderr)
            return 1
        if left.get("dir") != "Left" or float(left.get("to_x", 1e9)) > 200:
            print(
                "error: row Left nudge after Right did not return to the left link: "
                f"{left}",
                file=sys.stderr,
            )
            return 1

        # Every jump must land before the next press starts, or the test is
        # measuring a mid-flight retarget rather than a clean nudge. Jump 4
        # (index 4) starts the second page after a dock reset, so it is not a
        # continuation of the first page's final landing.
        for index in range(1, len(jumps)):
            if index == 4:
                continue
            prev = jumps[index - 1]
            cur = jumps[index]
            dx = abs(float(cur.get("from_x", 0)) - float(prev.get("to_x", 0)))
            dy = abs(float(cur.get("from_y", 0)) - float(prev.get("to_y", 0)))
            if dx > 30.0 or dy > 30.0:
                print(
                    f"error: jump {index} started before the previous flight landed "
                    f"(from=({cur.get('from_x')},{cur.get('from_y')}) "
                    f"prev_to=({prev.get('to_x')},{prev.get('to_y')}))",
                    file=sys.stderr,
                )
                return 1

        # The first tween frame after a jump must not inherit pre-press idle
        # time. With the old 100ms cap this was 0.1 (74% of the tween).
        for index, jump in enumerate(timed):
            if jump.get("event") != "cursor.jump":
                continue
            first = next(
                (row for row in timed[index + 1 :] if row.get("event") == "cursor.frame"),
                None,
            )
            if first is None:
                print("error: jump had no tween frames", file=sys.stderr)
                return 1
            elapsed = float(first.get("elapsed", 1.0))
            print(
                f"jump dir={jump.get('dir')} first_frame_elapsed={elapsed:.3f}s "
                f"dt={float(first.get('dt', 0)):.3f}s"
            )
            if elapsed > 0.05:
                print(
                    "error: first nudge frame started with stale idle time "
                    f"(elapsed={elapsed:.3f}s)",
                    file=sys.stderr,
                )
                return 1

        if not offscreen.report():
            return 1
        print("cursor nudge passed")
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
