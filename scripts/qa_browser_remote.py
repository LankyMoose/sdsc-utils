#!/usr/bin/env python3
"""Remote Browser add-on QA (no OS input).

1. Speaks the protocol directly to `sdsc-browser`: Hello -> Ready, pause/resume,
   reload action, Shutdown -> exit 0.
2. Launches the main app with the default remote path and verifies it starts the
   add-on process.
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
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--browser-exe", type=Path, help="sdsc-browser executable")
    return parser.parse_args()


def send(proc: subprocess.Popen, obj: dict) -> None:
    data = json.dumps(obj, separators=(",", ":")).encode()
    proc.stdin.write(struct.pack("<I", len(data)) + data)
    proc.stdin.flush()


def recv(proc: subprocess.Popen) -> dict | None:
    header = proc.stdout.read(4)
    if len(header) < 4:
        return None
    size = struct.unpack("<I", header)[0]
    return json.loads(proc.stdout.read(size))


def recv_until(proc: subprocess.Popen, kind: str, timeout: float = 10.0) -> dict | None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        event = recv(proc)
        if event is None:
            return None
        if event.get("kind") == kind:
            return event
    return None


def protocol_check(exe: Path, data_dir: Path) -> tuple[bool, str]:
    env = os.environ.copy()
    env["SDSC_DEBUG_QUIET"] = "1"
    env["SDSC_DATA_DIR"] = str(data_dir)
    proc = subprocess.Popen(
        [str(exe)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    try:
        send(
            proc,
            {
                "kind": "hello",
                "protocol": 1,
                "app": "browser",
                "host_pid": os.getpid(),
                "data_dir": str(data_dir),
            },
        )
        # Blocking read is fine; the child writes Ready after opening.
        ready = recv(proc)
        if not ready or ready.get("kind") != "ready":
            return False, f"browser did not return Ready: {ready!r}"
        send(proc, {"kind": "lifecycle", "pause": None})
        paused = recv_until(proc, "lifecycle")
        if not paused or paused.get("paused") != None:
            return False, f"browser did not report Paused: {paused!r}"
        send(proc, {"kind": "lifecycle", "resume": None})
        resumed = recv_until(proc, "lifecycle")
        if not resumed or resumed.get("running") != None:
            return False, f"browser did not report Running: {resumed!r}"

        page = data_dir / "remote.html"
        page.write_text("<title>remote qa</title><h1>remote</h1>", encoding="utf-8")
        send(
            proc,
            {
                "kind": "replay",
                "id": 1,
                "action": "navigate",
                "params": {"url": page.as_uri()},
            },
        )
        state = recv_until(proc, "replay_state")
        if not state or not str(state.get("state", {}).get("ui", {}).get("url", "")).endswith(
            "/remote.html"
        ):
            return False, f"replay navigate did not reach the add-on: {state!r}"
        send(proc, {"kind": "text", "insert": "ok"})
        send(
            proc,
            {
                "kind": "action",
                "app": "browser",
                "action": "reload",
            },
        )
        send(proc, {"kind": "lifecycle", "shutdown": None})
        proc.wait(timeout=15)
        if proc.returncode != 0:
            return False, f"browser exited {proc.returncode}"
        tabs = data_dir / "tabs.json"
        if not tabs.is_file() or "remote.html" not in tabs.read_text(encoding="utf-8"):
            return False, "browser did not persist the replayed page"
        return True, "protocol ok"
    finally:
        if proc.poll() is None:
            proc.kill()


def child_pids(parent_pid: int) -> list[int]:
    if os.name != "nt":
        return []
    command = [
        "powershell",
        "-NoProfile",
        "-Command",
        f"Get-CimInstance Win32_Process -Filter \"ParentProcessId={parent_pid}\" | Select-Object -ExpandProperty ProcessId",
    ]
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=15, check=False
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    pids = []
    for line in result.stdout.splitlines():
        line = line.strip()
        if line.isdigit():
            pids.append(int(line))
    return pids


def host_check(main: Path, data_dir: Path) -> tuple[bool, str]:
    env = os.environ.copy()
    env["SDSC_DEBUG_OPEN"] = "browser"
    env["SDSC_DEBUG_QUIET"] = "1"
    env["SDSC_DEBUG_INPUT"] = "0"
    env["SDSC_DATA_DIR"] = str(data_dir)
    env.pop("SDSC_BROWSER_REMOTE", None)
    proc = subprocess.Popen(
        [str(main)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    try:
        log = data_dir / "app.log"
        deadline = time.time() + 20
        launched = False
        while time.time() < deadline:
            text = log.read_text(encoding="utf-8", errors="replace") if log.is_file() else ""
            if "browser add-on process open" in text:
                launched = True
                break
            if proc.poll() is not None:
                return False, f"main app exited {proc.returncode}"
            time.sleep(0.5)
        if not launched:
            return False, "host did not log add-on launch"

        # Kill the add-on and verify the supervisor notices without exiting.
        deadline = time.time() + 10
        children: list[int] = []
        while time.time() < deadline and not children:
            children = child_pids(proc.pid)
            if not children:
                time.sleep(0.25)
        if not children:
            return False, "add-on child process not found"
        for pid in children:
            subprocess.run(
                ["taskkill", "/PID", str(pid), "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        deadline = time.time() + 15
        while time.time() < deadline:
            text = log.read_text(encoding="utf-8", errors="replace") if log.is_file() else ""
            if "browser add-on closed:" in text:
                if proc.poll() is not None:
                    return False, "main app exited after add-on crash"
                return True, "host launched and recovered from add-on exit"
            if proc.poll() is not None:
                return False, f"main app exited {proc.returncode} after add-on crash"
            time.sleep(0.5)
        return False, "host did not notice add-on exit"
    finally:
        subprocess.run(
            ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def home_check(main: Path, data_dir: Path) -> tuple[bool, str]:
    env = os.environ.copy()
    env["SDSC_DEBUG_OPEN"] = "browser,home"
    env["SDSC_DEBUG_QUIET"] = "1"
    env["SDSC_DEBUG_INPUT"] = "0"
    env["SDSC_DATA_DIR"] = str(data_dir)
    env.pop("SDSC_BROWSER_REMOTE", None)
    proc = subprocess.Popen(
        [str(main)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    try:
        log = data_dir / "app.log"
        deadline = time.time() + 25
        while time.time() < deadline:
            text = log.read_text(encoding="utf-8", errors="replace") if log.is_file() else ""
            if (
                "browser add-on process open" in text
                and "browser add-on lifecycle Paused" in text
            ):
                return True, "Home paused the Browser add-on"
            if proc.poll() is not None:
                return False, f"main app exited {proc.returncode}"
            time.sleep(0.5)
        return False, "Home did not pause the add-on"
    finally:
        subprocess.run(
            ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    main_exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    browser_exe = args.browser_exe or root / "target" / "debug" / "sdsc-browser.exe"
    if not main_exe.is_file() or not browser_exe.is_file():
        print("error: build sdsc-utils and sdsc-browser first", file=sys.stderr)
        return 2
    stamp = time.strftime("%Y%m%d-%H%M%S")
    base = Path(tempfile.gettempdir()) / "sdsc-remote-browser-qa" / stamp
    for name, fn in (("protocol", lambda d: protocol_check(browser_exe, d)),):
        data = base / name
        data.mkdir(parents=True, exist_ok=True)
        ok, message = fn(data)
        print(f"{name}: {message}")
        if not ok:
            return 1
    data = base / "host"
    data.mkdir(parents=True, exist_ok=True)
    ok, message = host_check(main_exe, data)
    print(f"host: {message}")
    if not ok:
        return 1
    data = base / "home"
    data.mkdir(parents=True, exist_ok=True)
    ok, message = home_check(main_exe, data)
    print(f"home: {message}")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
