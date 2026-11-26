#!/usr/bin/env python3
"""Browser semantic-action smoke test (no OS input).

Builds a two-entry history, then fires L1 (back), R1 (forward), and Options
(reload) through the OS binding table. Verifies the `app.action` trace events
were enabled and the Browser ended on the expected page. Tap only: no hold or
history-selector UI exists in v0.
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
<html><head><meta charset="utf-8"><title>{name}</title></head>
<body style="margin:0;font:24px sans-serif"><h1>{name}</h1></body></html>
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
    session = Path(tempfile.gettempdir()) / "sdsc-browser-actions-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page_a = session / "a.html"
    page_b = session / "b.html"
    page_a.write_text(PAGE.format(name="action A"), encoding="utf-8")
    page_b.write_text(PAGE.format(name="action B"), encoding="utf-8")
    actions = [
        {"app": "browser", "action": "navigate", "params": {"url": page_a.as_uri()}},
        {"app": "browser", "action": "navigate", "params": {"url": page_b.as_uri()}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "l1"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "r1"}},
        {"app": "browser", "action": "pad.tap", "params": {"button": "options"}},
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

        tabs_path = sandbox / "tabs.json" if sandbox else None
        url = ""
        if tabs_path and tabs_path.is_file():
            tabs = json.loads(tabs_path.read_text(encoding="utf-8")).get("tabs") or []
            if tabs:
                url = str(tabs[0].get("url") or "")
        trace_dir = next(sandbox.glob("trace/session-*"), None) if sandbox else None
        events = []
        platform = trace_dir / "platform.jsonl" if trace_dir else None
        if platform and platform.is_file():
            for line in platform.read_text(encoding="utf-8", errors="replace").splitlines():
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if record.get("event") == "app.action":
                    events.append(record)
        print(f"sandbox={sandbox}")
        print(f"url={url}")
        print(f"actions={json.dumps(events, ensure_ascii=False)}")
        if not url.endswith("/b.html"):
            print("error: Browser did not settle on the forward page", file=sys.stderr)
            return 1
        seen = {record.get("action"): record for record in events}
        for action in ("back", "forward", "reload"):
            if action not in seen:
                print(f"error: {action} action was not invoked", file=sys.stderr)
                return 1
            if seen[action].get("enabled") is not True:
                print(f"error: {action} was invoked while disabled", file=sys.stderr)
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
