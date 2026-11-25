#!/usr/bin/env python3
"""Home Browser add-on lifecycle QA (no OS input).

Drives the real Home/app lifecycle through replay:
install -> launch -> update -> launch -> uninstall.

The sandbox starts without an installed add-on. Replay params point at two
local source executables/versions, so the test does not depend on a real feed.
Every step uses `SDSC_DEBUG_INPUT=0` and no OS input is injected.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>{name}</title></head>
<body style="margin:0;font:24px sans-serif"><h1>{name}</h1></body></html>
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--browser-exe", type=Path, help="sdsc-browser executable")
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
    browser_exe = args.browser_exe or root / "target" / "debug" / "sdsc-browser.exe"
    if not exe.is_file() or not browser_exe.is_file():
        print("error: build sdsc-utils and sdsc-browser first", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-app-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    page_a = session / "install.html"
    page_b = session / "update.html"
    page_a.write_text(PAGE.format(name="installed build"), encoding="utf-8")
    page_b.write_text(PAGE.format(name="updated build"), encoding="utf-8")
    source_one = session / "browser-1.exe"
    source_two = session / "browser-2.exe"
    shutil.copy2(browser_exe, source_one)
    shutil.copy2(browser_exe, source_two)

    actions = [
        {"app": "home", "action": "open", "params": {}},
        {
            "app": "home",
            "action": "app.install",
            "params": {"source": str(source_one), "version": "1.0"},
        },
        {"app": "home", "action": "app.launch", "params": {}},
        {"app": "browser", "action": "navigate", "params": {"url": page_a.as_uri()}},
        {"app": "home", "action": "app.close", "params": {}},
        {"app": "home", "action": "open", "params": {}},
        {
            "app": "home",
            "action": "app.update",
            "params": {"source": str(source_two), "version": "2.0"},
        },
        {"app": "home", "action": "app.launch", "params": {}},
        {"app": "browser", "action": "navigate", "params": {"url": page_b.as_uri()}},
        {"app": "home", "action": "app.close", "params": {}},
        {"app": "home", "action": "open", "params": {}},
        {"app": "home", "action": "app.uninstall", "params": {}},
    ]
    with (session / "actions.jsonl").open("w", encoding="utf-8") as out:
        for index, action in enumerate(actions, 1):
            requires = []
            if action["action"] in {"app.update", "app.uninstall"}:
                requires = [{"path": "addon.installed", "value": True}]
            if action["action"] == "app.update":
                requires.append({"path": "addon.version", "value": "1.0"})
            out.write(
                json.dumps(
                    {
                        "kind": "action",
                        "flow": 1,
                        "seq": index,
                        "repro": action,
                        "requires": requires,
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

        trace_dir = next(sandbox.glob("trace/session-*"), None) if sandbox else None
        actions_rows = read_jsonl(trace_dir / "actions.jsonl") if trace_dir else []
        home_events = read_jsonl(trace_dir / "home.jsonl") if trace_dir else []
        manifest_rows = read_jsonl(trace_dir / "manifest.jsonl") if trace_dir else []

        outcomes = {
            row.get("action"): row
            for row in actions_rows
            if row.get("kind") == "outcome"
            and row.get("action") in {
                "home.app.install",
                "home.app.update",
                "home.app.uninstall",
                "home.app.launch",
            }
        }
        print(f"sandbox={sandbox}")
        print(
            "outcomes="
            + json.dumps(
                {name: row.get("outcome") for name, row in outcomes.items()},
                sort_keys=True,
            )
        )
        expected = {
            "home.app.install": "installed",
            "home.app.update": "updated",
            "home.app.uninstall": "uninstalled",
            "home.app.launch": "launched",
        }
        for name, want in expected.items():
            got = outcomes.get(name, {}).get("outcome")
            if got != want:
                print(f"error: {name} outcome {got!r}, want {want!r}", file=sys.stderr)
                return 1

        install_post = outcomes["home.app.install"].get("post") or {}
        update_post = outcomes["home.app.update"].get("post") or {}
        if install_post.get("addon", {}).get("version") != "1.0":
            print("error: install did not publish addon version 1.0", file=sys.stderr)
            return 1
        if update_post.get("addon", {}).get("version") != "2.0":
            print("error: update did not publish addon version 2.0", file=sys.stderr)
            return 1
        if not any(
            row.get("event") == "browser.addon.install.phase" for row in home_events
        ):
            print("error: add-on install phases were not traced", file=sys.stderr)
            return 1
        launch_count = sum(
            1
            for row in actions_rows
            if row.get("kind") == "outcome" and row.get("action") == "home.app.launch"
        )
        if launch_count < 2:
            print("error: both launch flows were not traced", file=sys.stderr)
            return 1
        states = [
            row.get("value", {}).get("state")
            for row in manifest_rows
            if row.get("kind") == "browser_addon"
        ]
        if "installed" not in states or "uninstalled" not in states:
            print(f"error: manifest did not record add-on states: {states}", file=sys.stderr)
            return 1
        tabs = []
        tabs_path = sandbox / "tabs.json" if sandbox else None
        if tabs_path and tabs_path.is_file():
            tabs = json.loads(tabs_path.read_text(encoding="utf-8")).get("tabs") or []
        if not any(str(tab.get("url", "")).endswith("/update.html") for tab in tabs):
            print("error: updated build did not reach the second page", file=sys.stderr)
            return 1
        if (sandbox / "browser-addon").exists():
            print("error: uninstall left the add-on directory behind", file=sys.stderr)
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
