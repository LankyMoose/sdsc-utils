#!/usr/bin/env python3
"""Launcher Settings system-OSK proof (no OS input).

Opens Add shortcut in Launcher edit mode, pads focus between Title and
Arguments, opens the system OSK with Cross on each field, types via the
Quadrant gesture machine, saves with Enter, and verifies the committed manual
entry in games.json. Cross must never save while the form is open.
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
    session = Path(tempfile.gettempdir()) / "sdsc-launcher-osk-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    actions = [
        {
            "app": "launcher",
            "action": "manual_add",
            "params": {"target": r"C:\Windows\notepad.exe", "title": ""},
        },
        # Cross on focused Title opens the system OSK.
        {"app": "launcher", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.0, "y": 0.0}},
        # F then T -> "ft".
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.65, "y": 0.0}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.0, "y": 0.0}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.0, "y": 0.0}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.65, "y": -0.6}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.0, "y": 0.0}},
        # Circle closes the OSK; Down moves pad focus to Arguments; Cross opens
        # the OSK for it again.
        {"app": "launcher", "action": "pad.tap", "params": {"button": "circle"}},
        {"app": "launcher", "action": "pad.dir", "params": {"dir": "down"}},
        {"app": "launcher", "action": "pad.tap", "params": {"button": "cross"}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.0, "y": 0.0}},
        # X: left stick down quadrant outer-left.
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": -0.55, "y": 0.8}},
        {"app": "launcher", "action": "osk.stick", "params": {"side": "left", "x": 0.0, "y": 0.0}},
        # Enter closes the OSK and hands off to the form's Save/Add path.
        {"app": "launcher", "action": "key", "params": {"key": "enter"}},
        {"app": "launcher", "action": "commit", "params": {}},
        {"app": "launcher", "action": "close", "params": {}},
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

        games_path = sandbox / "games.json" if sandbox else None
        entries = []
        if games_path and games_path.is_file():
            entries = json.loads(games_path.read_text(encoding="utf-8")).get("entries") or []
        manual = [
            entry
            for entry in entries
            if entry.get("kind") == "manual"
            and entry.get("target", "").lower().endswith("notepad.exe")
        ]
        print(f"sandbox={sandbox}")
        print(f"manual={json.dumps(manual, ensure_ascii=False)}")
        if not manual:
            print("error: Add shortcut did not commit a manual entry", file=sys.stderr)
            return 1
        if manual[0].get("title") != "ft":
            print("error: Title draft did not receive the OSK text", file=sys.stderr)
            return 1
        if manual[0].get("args") != "x":
            print("error: Arguments draft did not receive the OSK text", file=sys.stderr)
            return 1

        trace_dir = next(sandbox.glob("trace/session-*"), None) if sandbox else None
        platform = read_jsonl(trace_dir / "platform.jsonl") if trace_dir else []
        opens = [row for row in platform if row.get("event") == "osk.open"]
        targets = {row.get("target") for row in opens}
        if "Shortcut title" not in targets or "Shortcut arguments" not in targets:
            print(f"error: OSK did not open for both fields: {targets}", file=sys.stderr)
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
