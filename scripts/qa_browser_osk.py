#!/usr/bin/env python3
"""Auto-OSK focus smoke test (no OS input).

Loads a page with an autofocused text field, then clicks/blurs/refocuses a
text input, a contenteditable, and an ARIA textbox. The Browser must auto-show
the on-screen keyboard for each editable target and auto-dismiss it on blur;
a button must not trigger it. Page-load autofocus must stay closed until a
pointer click. The replay trace records each transition as `browser.osk_focus`.
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

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>osk focus qa</title>
<style>
  html, body { margin: 0; height: 2000px; font: 20px/1.5 sans-serif; }
  .field { position: absolute; width: 300px; height: 60px; font-size: 26px; }
  #q { left: 40px; top: 40px; }
  #ce { left: 420px; top: 40px; border: 1px solid #888; }
  #ar { left: 40px; top: 140px; border: 1px solid #888; }
  #btn { left: 420px; top: 140px; width: 300px; height: 60px; }
  #text { position: absolute; left: 40px; top: 240px; width: 700px; }
</style></head>
<body>
<input id="q" class="field" autofocus>
<div id="ce" class="field" contenteditable="true">editable div</div>
<div id="ar" class="field" role="textbox" tabindex="0">aria textbox</div>
<button id="btn">not a text field</button>
<p id="text">click here to blur the field</p>
</body></html>
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-osk-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page = session / "osk.html"
    page.write_text(PAGE, encoding="utf-8")

    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page.as_uri()}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 270}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 70}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 270}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 500, "y": 170}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 500, "y": 70}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 270}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 170}},
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

        # Let the final poll and the trace flush land after the last click.
        time.sleep(0.6)
        browser_log = (
            next(sandbox.glob("trace/session-*/browser.jsonl"), None) if sandbox else None
        )
        text = (
            browser_log.read_text(encoding="utf-8", errors="replace")
            if browser_log and browser_log.is_file()
            else ""
        )
        focus = []
        first_press_ts = None
        first_focus_ts = None
        for line in text.splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get("event") == "browser.page_press" and first_press_ts is None:
                first_press_ts = record.get("ts")
            if record.get("event") == "browser.osk_focus":
                if first_focus_ts is None:
                    first_focus_ts = record.get("ts")
                focus.append(
                    (
                        bool(record.get("editable")),
                        bool(record.get("open")),
                        bool(record.get("auto")),
                    )
                )
        print(f"sandbox={sandbox}")
        print(f"osk_focus={focus}")
        if first_focus_ts is not None and (
            first_press_ts is None or first_focus_ts < first_press_ts
        ):
            print(
                "error: page-load autofocus opened the OSK before any user input",
                file=sys.stderr,
            )
            return 1
        expected = [
            (True, True, True),  # text input
            (False, False, True),  # paragraph blur
            (True, True, True),  # contenteditable (button did not open)
            (False, False, True),  # paragraph blur
            (True, True, True),  # ARIA textbox
        ]
        if focus != expected:
            print(
                "error: expected auto-OSK on editable focus and close on blur",
                file=sys.stderr,
            )
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
