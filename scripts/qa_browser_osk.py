#!/usr/bin/env python3
"""System OSK behaviour test (no OS input).

One simple QWERTY layout: the first Cross on a page text field opens the
system OSK, D-pad moves the single focus, Cross types the focused key,
Triangle backspaces, Square switches symbols, Options enters, real mouse
input dismisses it, and keyboard input closes + hands off the key.

The app runs with SDSC_DEBUG_INPUT=0 and is killed as soon as replay.done is
observed. The OSK is a separate topmost no-activate overlay; Browser-owned
OSK code no longer exists.
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
<html><head><meta charset="utf-8"><title>system osk qa</title>
<style>
  html, body { margin: 0; height: 2000px; font: 20px/1.5 sans-serif; }
  #q { position: absolute; left: 40px; top: 40px; width: 400px; height: 60px; font-size: 30px; }
  #text { position: absolute; left: 40px; top: 240px; width: 700px; }
</style></head>
<body>
<input id="q" autofocus>
<p id="text">click here to leave the field</p>
<script>
  const q = document.getElementById('q');
  let typed = '';
  function render() { document.title = 'typed=' + typed; }
  q.addEventListener('input', () => { typed = q.value; render(); });
  render();
</script>
</body></html>
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def write_plan(session: Path, actions: list[dict]) -> None:
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


def run(exe: Path, root: Path, page: Path, keep: bool) -> tuple[bool, str]:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-system-osk-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page.as_uri()}},
        {"app": "browser", "action": "cursor.dock", "params": {"x": 100, "y": 70}},
        # First Cross on the text field: focus + open the system OSK.
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        # Focus starts on Q; Cross types it.
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        # Move right Q -> W and type W.
        {"app": "browser", "action": "pad.dir", "params": {"dir": "right"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        # Move down W -> S and type S.
        {"app": "browser", "action": "pad.dir", "params": {"dir": "down"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        # Triangle backspaces the S.
        {"app": "browser", "action": "pad.tap", "params": {"button": "triangle"}},
        # Symbols: the S slot is '/'. Then back to alpha and type S again.
        {"app": "browser", "action": "pad.tap", "params": {"button": "square"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "square"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        # Control row, same column: pass X, then Space. Options is Enter.
        {"app": "browser", "action": "pad.dir", "params": {"dir": "down"}},
        {"app": "browser", "action": "pad.dir", "params": {"dir": "down"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "options"}},
        # Real keyboard closes the OSK, then hands 'a' to the page field.
        {"app": "browser", "action": "key", "params": {"key": "a"}},
        # Reopen with Cross, then dismiss with real mouse input.
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "browser", "action": "mouse.click", "params": {"x": 100, "y": 270}},
        # Reopen and close with Escape (consumed by the OSK).
        {"app": "browser", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "browser", "action": "key", "params": {"key": "escape"}},
    ]
    expected = "qw/s a"
    write_plan(session, actions)
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
            return False, "replay.done not observed before timeout"

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
            tabs = json.loads(tabs_path.read_text(encoding="utf-8")).get("tabs") or []
            if tabs:
                title = str(tabs[0].get("title") or "")
        trace_dir = next(sandbox.glob("trace/session-*"), None) if sandbox else None
        platform = read_jsonl(trace_dir / "platform.jsonl") if trace_dir else []
        opens = [row for row in platform if row.get("event") == "osk.open"]
        closes = [row for row in platform if row.get("event") == "osk.close"]
        enters = [row for row in platform if row.get("event") == "osk.enter"]
        pad_taps = [row for row in platform if row.get("event") == "replay.pad_tap"]
        print(f"sandbox={sandbox}")
        print(
            f"title={title!r} opens={len(opens)} closes={len(closes)} "
            f"enters={len(enters)}"
        )
        if title != f"typed={expected}":
            return False, f"expected typed={expected}, got {title!r}"
        if not opens:
            return False, "OSK never opened"
        if not pad_taps:
            return False, "no Cross pad tap was recorded"
        first_open = min(row.get("ts", 0) for row in opens)
        first_tap = min(row.get("ts", 0) for row in pad_taps)
        if first_open < first_tap:
            return False, "OSK opened before Cross (focus auto-open regression)"
        if not enters:
            return False, "Options never emitted osk.enter"
        if not any(row.get("reason") == "keyboard" for row in closes):
            return False, "keyboard did not close the OSK"
        if not any(row.get("reason") == "mouse" for row in closes):
            return False, "real mouse did not close the OSK"
        if not offscreen.report():
            return False, "offscreen QA failed"
        return True, ""
    finally:
        offscreen.stop()
        if not keep and proc.poll() is None:
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


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2
    page = Path(tempfile.gettempdir()) / "sdsc-system-osk-qa.html"
    page.write_text(PAGE, encoding="utf-8")
    ok, error = run(exe, root, page, args.keep)
    if not ok:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print("system OSK passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
