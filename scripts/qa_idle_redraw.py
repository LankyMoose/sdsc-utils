#!/usr/bin/env python3
"""Idle-redraw measurement for the per-window refresh work order (debug only).

Launches a debug build with ``SDSC_DEBUG_REDRAW=1`` and a chosen
``SDSC_DEBUG_OPEN`` scenario, waits, then compares the cumulative
``ui-redraw:`` snapshots in ``app.log``. The counter thread logs every 2s
without touching the iced event loop, so a quiet delta is real.

Scenarios:
  osk       Browser + system OSK open (no pad connected: pad counters stay 0)
  toast     resting toast card
  launcher  idle immersive Launcher
  browser   idle standalone Browser

Exit status is 0 for a measurement run; pass ``--assert`` to enforce the
post-migration idle budgets (no OSK/toast/Launcher/Browser frame traffic).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import qa_offscreen

OPEN = {
    "osk": "browser,osk",
    "toast": "toast",
    "launcher": "launcher",
    "browser": "browser",
}

# Cumulative counters that must not advance while the scenario is idle.
ASSERT_MSGS = {
    "osk": ("pad_input", "osk", "browser_frame", "toast_frame", "launcher_frame"),
    "toast": ("toast_frame",),
    "launcher": ("launcher_frame",),
    "browser": ("browser_frame",),
}
TARGET_VIEW = {
    "osk": "osk",
    "toast": "toast",
    "launcher": "launcher",
    "browser": "browser",
}

TOKEN = re.compile(r"([a-z_]+)=(\d+)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument(
        "--scenario",
        choices=sorted(OPEN),
        default="osk",
        help="which window set to open",
    )
    parser.add_argument(
        "--seconds",
        type=float,
        default=None,
        help="run time (default: 7s toast, 8s others)",
    )
    parser.add_argument(
        "--assert",
        dest="assert_idle",
        action="store_true",
        help="fail when the scenario's idle counters advance",
    )
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def parse_snapshot(line: str) -> dict[str, int] | None:
    if "ui-redraw:" not in line:
        return None
    views = re.search(r"views\[([^\]]*)\]", line)
    redraws = re.search(r"redraws\[([^\]]*)\]", line)
    msgs = re.search(r"msgs\[([^\]]*)\]", line)
    if not views or not redraws or not msgs:
        return None
    out = {"t_ms": 0}
    t = re.search(r"t_ms=(\d+)", line)
    if t:
        out["t_ms"] = int(t.group(1))
    for prefix, body in (
        ("view", views.group(1)),
        ("redraw", redraws.group(1)),
        ("msg", msgs.group(1)),
    ):
        for name, value in TOKEN.findall(body):
            out[f"{prefix}.{name}"] = int(value)
    return out


def wait_for_log(log: Path, deadline: float) -> list[dict[str, int]]:
    # Always wait the whole measurement window: early lines can belong to a
    # still-booting app (and the last two must both be post-open).
    while time.time() < deadline:
        time.sleep(0.25)
    if not log.is_file():
        return []
    return [
        snap
        for snap in (
            parse_snapshot(line)
            for line in log.read_text(encoding="utf-8", errors="replace").splitlines()
        )
        if snap is not None
    ]


def main() -> int:
    args = parse_args()
    seconds = args.seconds if args.seconds is not None else {
        "toast": 7.0,
        "launcher": 11.0,
    }.get(args.scenario, 9.0)
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    data = Path(tempfile.gettempdir()) / "sdsc-idle-redraw" / f"{args.scenario}-{stamp}"
    data.mkdir(parents=True, exist_ok=True)
    # Keep the measurement free of real background work: no boot update check
    # and a local blank page instead of the network default page.
    (data / "prefs.json").write_text('{"update_boot_check": false}', encoding="utf-8")
    page = data / "idle.html"
    page.write_text("<!doctype html><title>idle</title>", encoding="utf-8")
    tabs = {"tabs": [{"url": page.as_uri(), "title": "idle"}], "active": 0}
    (data / "tabs.json").write_text(json.dumps(tabs), encoding="utf-8")
    log = data / "app.log"
    env = os.environ.copy()
    env["SDSC_DATA_DIR"] = str(data)
    qa_offscreen.apply_env(env)
    env["SDSC_DEBUG_OPEN"] = OPEN[args.scenario]
    env["SDSC_DEBUG_REDRAW"] = "1"
    print(f"session={data}")
    print(f"command={exe} SDSC_DEBUG_OPEN={OPEN[args.scenario]}")
    proc = subprocess.Popen(
        [str(exe)],
        cwd=str(root),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    offscreen = qa_offscreen.watch(proc.pid)
    try:
        snaps = wait_for_log(log, time.time() + seconds)
        if len(snaps) < 2:
            print("error: fewer than two ui-redraw snapshots", file=sys.stderr)
            return 1
        # Compare the last two snapshots: both are after the boot/open burst.
        before, after = snaps[-2], snaps[-1]
        delta = {
            key: after[key] - before.get(key, 0)
            for key in after
            if key != "t_ms" and key.startswith(("view.", "redraw.", "msg."))
        }
        window = (after["t_ms"] - before["t_ms"]) / 1000.0
        print(json.dumps({"scenario": args.scenario, "window_s": window, "delta": delta}, indent=2))
        if args.assert_idle:
            failures = []
            for key in ASSERT_MSGS[args.scenario]:
                if delta.get(f"msg.{key}", 0) > 0:
                    failures.append(f"msg.{key}={delta[f'msg.{key}']}")
            # Every dispatched message rebuilds every window, so the target
            # window's view count must equal the total message count. An extra
            # view means an uncounted redraw source remains.
            total_messages = sum(
                value for key, value in delta.items() if key.startswith("msg.")
            )
            target = TARGET_VIEW[args.scenario]
            target_view = delta.get(f"view.{target}", 0)
            if target_view > total_messages:
                failures.append(f"view.{target}={target_view} > msgs={total_messages}")
            # Browser CEF presentation and Launcher ambient/ken-burns are
            # intentionally per-window 60Hz clocks; their acceptance is the
            # message counters above. OSK/toast must not repaint beyond global
            # messages at rest.
            if target in ("osk", "toast"):
                target_redraws = delta.get(f"redraw.{target}", 0)
                if target_redraws > total_messages:
                    failures.append(
                        f"redraw.{target}={target_redraws} > msgs={total_messages}"
                    )
            if failures:
                print("error: idle traffic: " + ", ".join(failures), file=sys.stderr)
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
