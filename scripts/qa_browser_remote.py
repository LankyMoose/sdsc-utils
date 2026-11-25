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
        send(proc, {"kind": "lifecycle", "resume": None})
        send(proc, {"kind": "lifecycle", "shutdown": None})
        proc.wait(timeout=15)
        if proc.returncode != 0:
            return False, f"browser exited {proc.returncode}"
        return True, "protocol ok"
    finally:
        if proc.poll() is None:
            proc.kill()


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
        while time.time() < deadline:
            if log.is_file() and "browser add-on process open" in log.read_text(
                encoding="utf-8", errors="replace"
            ):
                return True, "host launched add-on"
            if proc.poll() is not None:
                return False, f"main app exited {proc.returncode}"
            time.sleep(0.5)
        return False, "host did not log add-on launch"
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
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
