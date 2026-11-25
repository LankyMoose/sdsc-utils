#!/usr/bin/env python3
"""Synthetic browser input smoke test (no OS keyboard/mouse injection).

Builds a local test page, writes a replay plan using the debug-only `text`,
`key`, `mouse.click`, `mouse.drag`, and `mouse.wheel` steps, runs the debug
app in its replay sandbox, and verifies the page-observed state. The app runs
with SDSC_DEBUG_INPUT=0, so real input is locked out for the whole run.

Usage:
    python scripts/qa_browser_input.py [--exe PATH] [--keep]

Exit code 0 means the final page title contains the typed text, a nonzero
wheel scroll, and nonempty text selection from the drag/double-click.
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
<html><head><meta charset="utf-8"><title>input qa</title>
<style>
  html, body { margin: 0; height: 4000px; font: 22px/1.5 sans-serif; }
  #q { position: absolute; left: 40px; top: 40px; width: 400px; height: 60px; font-size: 30px; }
  #text { position: fixed; margin: 0; left: 40px; top: 140px; width: 700px; }
</style></head>
<body>
<input id="q" autofocus>
<p id="text">alpha bravo charlie delta echo foxtrot golf hotel india juliet</p>
<script>
  const q = document.getElementById('q');
  let typed = '';
  let selected = '';
  let selected_max = 0;
  function render() {
    document.title = 'typed=' + typed + '|scroll=' + Math.round(window.scrollY)
      + '|sel=' + selected + '|selmax=' + selected_max;
  }
  q.addEventListener('input', () => { typed = q.value; render(); });
  window.addEventListener('scroll', render, { passive: true });
  document.addEventListener('selectionchange', () => {
    selected = String(window.getSelection()).slice(0, 40);
    selected_max = Math.max(selected_max, selected.length);
    render();
  });
  document.addEventListener('mouseup', () => {
    setTimeout(() => {
      selected = String(window.getSelection()).slice(0, 40);
      selected_max = Math.max(selected_max, selected.length);
      render();
    }, 50);
  });
  render();
</script>
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
    session = Path(tempfile.gettempdir()) / "sdsc-browser-input-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page = session / "input.html"
    page.write_text(PAGE, encoding="utf-8")

    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page.as_uri()}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 120, "y": 70}},
        {"app": "browser", "action": "text", "params": {"text": "hello"}},
        {"app": "browser", "action": "ime.preedit", "params": {"text": "x"}},
        {"app": "browser", "action": "ime.commit", "params": {"text": "é"}},
        {"app": "browser", "action": "mouse.wheel", "params": {"x": 300, "y": 400, "dy": -240}},
        {"app": "browser", "action": "mouse.drag", "params": {"x1": 45, "y1": 150, "x2": 600, "y2": 150}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 150, "count": 2}},
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
    # Debug replay prepares these itself; setting them here documents the
    # contract and keeps the script safe if replay behavior changes.
    env["SDSC_DEBUG_QUIET"] = "1"
    env["SDSC_DEBUG_INPUT"] = "0"
    # Replay QA still targets the transitional in-process Browser.
    env["SDSC_BROWSER_REMOTE"] = "0"
    print(f"session={session}")
    print(f"exe={exe}")
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
                    if manifests:
                        text = manifests[0].read_text(encoding="utf-8", errors="replace")
                        if '"replay.done"' in text:
                            done = True
                            break
            time.sleep(0.5)
        if not done:
            print("error: replay.done not observed before timeout", file=sys.stderr)
            return 1

        # The driver closes the Browser at replay.done; allow the close marker
        # to land, then collect the final page state.
        close_deadline = time.time() + 5
        while time.time() < close_deadline:
            app_log = sandbox / "app.log" if sandbox else None
            if app_log and app_log.is_file() and "browser window close" in app_log.read_text(
                encoding="utf-8", errors="replace"
            ):
                break
            time.sleep(0.2)

        tabs_path = sandbox / "tabs.json" if sandbox else None
        title = ""
        if tabs_path and tabs_path.is_file():
            tabs = json.loads(tabs_path.read_text(encoding="utf-8"))
            tabs_list = tabs.get("tabs") or []
            if tabs_list:
                title = str(tabs_list[0].get("title") or "")
        engine_log = next(sandbox.glob("trace/session-*/engine.jsonl"), None) if sandbox else None
        engine_text = (
            engine_log.read_text(encoding="utf-8", errors="replace")
            if engine_log and engine_log.is_file()
            else ""
        )
        print(f"sandbox={sandbox}")
        print(f"title={title}")
        if "typed=helloé" not in title:
            print("error: page did not receive the synthetic/IME text", file=sys.stderr)
            return 1
        if "|scroll=" not in title or "|scroll=0" in title:
            print("error: synthetic wheel did not scroll the page", file=sys.stderr)
            return 1
        fields = {
            key: value
            for part in title.split("|")
            if "=" in part
            for key, value in [part.split("=", 1)]
        }
        if int(fields.get("selmax", "0") or "0") < 10:
            print(
                "error: drag selection did not select a text range",
                file=sys.stderr,
            )
            return 1
        if '"cursor":"Text"' not in engine_text:
            print("error: CEF cursor shape did not reach the UI", file=sys.stderr)
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
