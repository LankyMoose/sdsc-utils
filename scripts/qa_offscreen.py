#!/usr/bin/env python3
"""Offscreen QA contract.

Every automated/quiet QA session must place its windows fully outside the
virtual desktop. Windows stay ``WS_VISIBLE`` so Win32 still paints them and
``iced::window::screenshot`` keeps working, but no rectangle may intersect a
monitor.

Use :class:`OffscreenWatch` around a launched app process: it samples visible
top-level windows and fails the run if one ever overlaps a monitor. The same
check runs in Rust as a log witness (``ui-diag: offscreen violation``), but
this external checker is the hard gate.
"""

from __future__ import annotations

import ctypes
import os
import sys
import threading
import time
from ctypes import wintypes

IS_WINDOWS = os.name == "nt"

if IS_WINDOWS:
    _user32 = ctypes.WinDLL("user32", use_last_error=True)

    class _RECT(ctypes.Structure):
        _fields_ = [
            ("left", ctypes.c_long),
            ("top", ctypes.c_long),
            ("right", ctypes.c_long),
            ("bottom", ctypes.c_long),
        ]

    class _MONITORINFO(ctypes.Structure):
        _fields_ = [
            ("cbSize", ctypes.c_ulong),
            ("rcMonitor", _RECT),
            ("rcWork", _RECT),
            ("dwFlags", ctypes.c_ulong),
        ]

    _MONITORENUMPROC = ctypes.WINFUNCTYPE(
        ctypes.c_int,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.POINTER(_RECT),
        ctypes.c_double,
    )
    _ENUMWINDOWSPROC = ctypes.WINFUNCTYPE(
        ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p
    )

    _user32.EnumDisplayMonitors.argtypes = [
        ctypes.c_void_p,
        ctypes.c_void_p,
        _MONITORENUMPROC,
        ctypes.c_double,
    ]
    _user32.EnumDisplayMonitors.restype = ctypes.c_bool
    _user32.GetMonitorInfoW.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(_MONITORINFO),
    ]
    _user32.GetMonitorInfoW.restype = ctypes.c_bool
    _user32.EnumWindows.argtypes = [_ENUMWINDOWSPROC, ctypes.c_void_p]
    _user32.EnumWindows.restype = ctypes.c_bool
    _user32.GetWindowThreadProcessId.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(wintypes.DWORD),
    ]
    _user32.GetWindowThreadProcessId.restype = wintypes.DWORD
    _user32.IsWindowVisible.argtypes = [ctypes.c_void_p]
    _user32.IsWindowVisible.restype = ctypes.c_bool
    _user32.IsIconic.argtypes = [ctypes.c_void_p]
    _user32.IsIconic.restype = ctypes.c_bool
    _user32.GetWindowRect.argtypes = [ctypes.c_void_p, ctypes.POINTER(_RECT)]
    _user32.GetWindowRect.restype = ctypes.c_bool
    _user32.GetClassNameW.argtypes = [
        ctypes.c_void_p,
        ctypes.c_wchar_p,
        ctypes.c_int,
    ]
    _user32.GetClassNameW.restype = ctypes.c_int


def apply_env(env: dict[str, str]) -> None:
    """Apply the standard automated-QA environment to a launch env."""
    env["SDSC_DEBUG_QUIET"] = "1"
    env["SDSC_DEBUG_INPUT"] = "0"
    env["SDSC_DEBUG_OFFSCREEN"] = "1"


def monitor_rects() -> list[tuple[int, int, int, int]]:
    """Physical rectangles of every monitor (left, top, right, bottom)."""
    if not IS_WINDOWS:
        return []
    out: list[tuple[int, int, int, int]] = []

    def collect(hmonitor, _hdc, _rect, _data) -> int:
        info = _MONITORINFO()
        info.cbSize = ctypes.sizeof(_MONITORINFO)
        if _user32.GetMonitorInfoW(hmonitor, ctypes.byref(info)):
            r = info.rcMonitor
            out.append((r.left, r.top, r.right, r.bottom))
        return 1

    _user32.EnumDisplayMonitors(0, None, _MONITORENUMPROC(collect), 0)
    return out


def _intersects(
    rect: tuple[int, int, int, int], monitors: list[tuple[int, int, int, int]]
) -> bool:
    left, top, right, bottom = rect
    return any(
        left < m_right and right > m_left and top < m_bottom and bottom > m_top
        for m_left, m_top, m_right, m_bottom in monitors
    )


def visible_windows(pid: int) -> list[tuple[int, tuple[int, int, int, int]]]:
    """Visible, non-minimized top-level windows owned by ``pid``.

    winit keeps a tiny ``Winit Thread Event Target`` helper window visible at
    (0,0); it is an event target, not app content, so the offscreen contract
    ignores it. Every content window (Browser, OSK, Launcher, Home, toast,
    Settings, popup) must still be fully outside every monitor.
    """
    if not IS_WINDOWS:
        return []
    out: list[tuple[int, tuple[int, int, int, int]]] = []
    ignored_classes = {"Winit Thread Event Target"}

    def collect(hwnd, _data) -> bool:
        owner = wintypes.DWORD()
        _user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value != pid:
            return True
        if not _user32.IsWindowVisible(hwnd) or _user32.IsIconic(hwnd):
            return True
        class_name = ctypes.create_unicode_buffer(256)
        _user32.GetClassNameW(hwnd, class_name, 256)
        if class_name.value in ignored_classes:
            return True
        rect = _RECT()
        if not _user32.GetWindowRect(hwnd, ctypes.byref(rect)):
            return True
        if rect.right <= rect.left or rect.bottom <= rect.top:
            return True
        out.append((int(hwnd), (rect.left, rect.top, rect.right, rect.bottom)))
        return True

    _user32.EnumWindows(_ENUMWINDOWSPROC(collect), 0)
    return out


class OffscreenWatch:
    """Background sampler proving no app window is ever on a monitor."""

    def __init__(self, pid: int, interval: float = 0.05) -> None:
        self.pid = pid
        self.interval = interval
        self.monitors = monitor_rects()
        self.violations: list[tuple[int, tuple[int, int, int, int]]] = []
        self.samples = 0
        self.visible_samples = 0
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        if IS_WINDOWS and self.monitors:
            self._thread = threading.Thread(
                target=self._run, name="qa-offscreen", daemon=True
            )
            self._thread.start()

    def _run(self) -> None:
        while not self._stop.wait(self.interval):
            self.sample()

    def sample(self) -> None:
        windows = visible_windows(self.pid)
        with self._lock:
            self.samples += 1
            if windows:
                self.visible_samples += 1
            for hwnd, rect in windows:
                if _intersects(rect, self.monitors):
                    self.violations.append((hwnd, rect))

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None

    def report(self) -> bool:
        """Stop sampling and print the result. True only when clean."""
        self.stop()
        with self._lock:
            violations = list(self.violations)
            visible_samples = self.visible_samples
        if not IS_WINDOWS or not self.monitors:
            return True
        if violations:
            print(
                f"error: offscreen QA violation: {len(violations)} sample(s) "
                "placed a visible window on a monitor",
                file=sys.stderr,
            )
            for hwnd, rect in violations[:5]:
                print(f"  hwnd=0x{hwnd:x} rect={rect}", file=sys.stderr)
            return False
        if visible_samples == 0:
            print(
                "error: offscreen QA saw no visible app window; "
                "cannot prove it stayed offscreen",
                file=sys.stderr,
            )
            return False
        print(f"offscreen ok: {visible_samples} visible sample(s), 0 on-monitor")
        return True


def watch(pid: int, interval: float = 0.05) -> OffscreenWatch:
    return OffscreenWatch(pid, interval)


def require(pid: int) -> bool:
    """One-shot check for scripts that do not keep a watcher."""
    watcher = OffscreenWatch(pid, interval=0.0)
    watcher.sample()
    return watcher.report()


# CI/integration sanity: the pure geometry helpers are Windows-only.
if __name__ == "__main__":
    if not IS_WINDOWS:
        print("offscreen watch: non-Windows, skipped")
        raise SystemExit(0)
    monitors = monitor_rects()
    print(f"monitors={monitors}")
    pid = int(sys.argv[1]) if len(sys.argv) > 1 else os.getpid()
    time.sleep(0.1)
    print("offscreen" if require(pid) else "on-screen violation")
