#!/usr/bin/env python3
"""Synthetic popup-denial smoke test (no OS keyboard/mouse injection).

Opens a local page with a target=_blank link, clicks it through the replay
driver, and verifies v0 denies popups: no second tab is created, the popup
page never loads, and the source page stays current. The app runs with
SDSC_DEBUG_INPUT=0 and is killed as soon as replay.done is observed.
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

MAIN = """<!doctype html>
<html><head><meta charset="utf-8"><title>popup main</title></head>
<body style="margin:0">
<a id="pop" href="popup.html" target="_blank"
   style="position:fixed;left:40px;top:40px;padding:20px;font:24px sans-serif">open popup</a>
</body></html>
"""

POPUP = """<!doctype html>
<html><head><meta charset="utf-8"><title>popup ok</title></head>
<body><h1>popup ok</h1></body></html>
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
    session = Path(tempfile.gettempdir()) / "sdsc-browser-popup-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    main_page = session / "main.html"
    popup_page = session / "popup.html"
    main_page.write_text(MAIN, encoding="utf-8")
    popup_page.write_text(POPUP, encoding="utf-8")

    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": main_page.as_uri()}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 60}},
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
    # Replay QA still targets the transitional in-process Browser.
    env["SDSC_BROWSER_REMOTE"] = "0"
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

        close_deadline = time.time() + 5
        while time.time() < close_deadline:
            app_log = sandbox / "app.log" if sandbox else None
            if app_log and app_log.is_file() and "browser window close" in app_log.read_text(
                encoding="utf-8", errors="replace"
            ):
                break
            time.sleep(0.2)

        tabs_path = sandbox / "tabs.json" if sandbox else None
        tabs = []
        if tabs_path and tabs_path.is_file():
            tabs = json.loads(tabs_path.read_text(encoding="utf-8")).get("tabs") or []
        print(f"sandbox={sandbox}")
        print(f"tabs={json.dumps(tabs, ensure_ascii=False)}")
        popup = [tab for tab in tabs if str(tab.get("url", "")).endswith("/popup.html")]
        if popup:
            print("error: target=_blank opened a popup tab (v0 denies popups)", file=sys.stderr)
            return 1
        if not any(str(tab.get("url", "")).endswith("/main.html") for tab in tabs):
            print("error: source page was lost", file=sys.stderr)
            return 1
        browser_log = next(sandbox.glob("trace/session-*/browser.jsonl"), None) if sandbox else None
        text = (
            browser_log.read_text(encoding="utf-8", errors="replace")
            if browser_log and browser_log.is_file()
            else ""
        )
        if "browser.popup_denied" not in text:
            print("error: popup denial was not traced", file=sys.stderr)
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
