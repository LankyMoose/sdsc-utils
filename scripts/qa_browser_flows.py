#!/usr/bin/env python3
"""Realistic Browser sessions: search, click, history, URL edit, Circle close.

Runs a deterministic local Google-like fixture through replay with real input
locked, captures a screenshot after each behavioral step, and asserts both the
trace state (via replay `requires`) and the pixels (header + page identity).

No OS input is injected and no network is used.
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

SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))
import qa_offscreen  # noqa: E402
from qa_browser_visual import near, pixel, read_png  # noqa: E402

# Header geometry mirrors src/ui/browser/state.rs.
CARD_MARGIN = 50.0
CARD_PAD = 4.0
HEADER_H = 44.0
HEADER_RULE = 1.0
HEADER_TOTAL = HEADER_H + HEADER_RULE
HEADER_PAD = 12.0
HEADER_GROUP_GAP = 8.0
HEADER_SIDE_W = 136.0
URL_W_MIN = 420.0
URL_W_MAX = 680.0
URL_BAR_H = 32.0
HEADER_V_PAD = (HEADER_H - URL_BAR_H) * 0.5
ACCENT = (0x41, 0x41, 0xFB)
LINE = (55, 57, 62)

PAGE = {
    "google": {
        "color": (246, 247, 251),
        "html": """<!doctype html><html><head><meta charset="utf-8"><title>Google</title>
<style>
html,body{margin:0;height:100%;background:#f6f7fb;font-family:Arial,sans-serif}
#logo{position:absolute;left:180px;top:120px;font-size:56px;color:#4285f4;font-weight:700}
#q{position:absolute;left:180px;top:260px;width:720px;height:56px;font-size:28px;padding:0 16px;border:1px solid #9aa0a6;border-radius:28px;box-sizing:border-box}
#hint{position:absolute;left:200px;top:340px;font-size:16px;color:#5f6368}
</style></head><body>
<div id="logo">Google</div>
<input id="q" placeholder="Search" autocomplete="off">
<div id="hint">Search the web</div>
<script>
const input = document.getElementById('q');
input.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    location.href = 'results.html?q=' + encodeURIComponent(input.value);
  }
});
</script>
</body></html>""",
    },
    "results": {
        "color": (232, 240, 254),
        "html": """<!doctype html><html><head><meta charset="utf-8"><title>Results</title>
<style>
html,body{margin:0;height:100%;background:#e8f0fe;font-family:Arial,sans-serif}
#query{position:absolute;left:180px;top:120px;font-size:36px;color:#202124}
#link{position:absolute;left:180px;top:260px;font-size:28px;color:#1a0dab}
</style></head><body>
<div id="query"></div>
<a id="link" href="article.html">Meerkat facts</a>
<script>
const q = new URLSearchParams(location.search).get('q') || '';
document.title = 'Results for ' + q;
document.getElementById('query').textContent = 'Results for ' + q;
</script>
</body></html>""",
    },
    "article": {
        "color": (253, 242, 233),
        "html": """<!doctype html><html><head><meta charset="utf-8"><title>Meerkat article</title>
<style>
html,body{margin:0;height:100%;background:#fdf2e9;font-family:Arial,sans-serif}
h1{position:absolute;left:180px;top:140px;font-size:48px;color:#3c2a1e}
p{position:absolute;left:180px;top:260px;font-size:22px;color:#5b4636}
</style></head><body>
<h1>Meerkat article</h1>
<p>Meerkats stand sentry while the group forages.</p>
</body></html>""",
    },
    "manual": {
        "color": (234, 247, 234),
        "html": """<!doctype html><html><head><meta charset="utf-8"><title>Manual destination</title>
<style>
html,body{margin:0;height:100%;background:#eaf7ea;font-family:Arial,sans-serif}
h1{position:absolute;left:180px;top:140px;font-size:48px;color:#1d3b22}
p{position:absolute;left:180px;top:260px;font-size:22px;color:#365a3d}
</style></head><body>
<h1>Manual destination</h1>
<p>Entered through the header URL well.</p>
</body></html>""",
    },
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, help="debug sdsc-utils executable")
    parser.add_argument("--keep", action="store_true", help="keep the app running")
    return parser.parse_args()


def card_geometry(width: int, height: int) -> dict[str, float]:
    """Same 16:9 card math as browser_layout()."""
    avail_w = max(width - 2.0 * CARD_MARGIN, 320.0)
    avail_h = max(height - 2.0 * CARD_MARGIN, 240.0)
    card_h = avail_h
    card_w = card_h * 16.0 / 9.0
    if card_w > avail_w:
        card_w = avail_w
        card_h = card_w * 9.0 / 16.0
    page_w = card_w - 2.0 * CARD_PAD
    page_h = card_h - 2.0 * CARD_PAD - HEADER_TOTAL
    card_x = (width - card_w) * 0.5
    card_y = (height - card_h) * 0.5
    content_w = max(page_w - 2.0 * HEADER_PAD, 1.0)
    available = content_w - 2.0 * HEADER_GROUP_GAP - 2.0 * HEADER_SIDE_W
    url_w = min(max(available, URL_W_MIN), URL_W_MAX)
    return {
        "card_x": card_x,
        "card_y": card_y,
        "card_w": card_w,
        "card_h": card_h,
        "page_x": card_x + CARD_PAD,
        "page_y": card_y + CARD_PAD + HEADER_TOTAL,
        "page_w": page_w,
        "page_h": page_h,
        "header_top": card_y + CARD_PAD,
        "header_mid": card_y + CARD_PAD + HEADER_H * 0.5,
        "url_x": card_x + CARD_PAD + (page_w - url_w) * 0.5,
        "url_y": card_y + CARD_PAD + HEADER_V_PAD,
        "url_w": url_w,
    }


def assert_header(
    rgba: bytes,
    width: int,
    geometry: dict[str, float],
    *,
    editing: bool,
    forward_disabled: bool = False,
    label: str,
) -> None:
    header_top = int(geometry["header_top"])
    header_check_y = header_top + 2
    url_x = int(geometry["url_x"])
    url_y = int(geometry["url_y"])
    url_w = int(geometry["url_w"])

    # The header strip is dark card material across its full width.
    dark = 0
    samples = 0
    for x in range(
        int(geometry["card_x"]) + 20,
        int(geometry["card_x"] + geometry["card_w"]) - 20,
        12,
    ):
        c = pixel(rgba, width, x, header_check_y)
        samples += 1
        if sum(c) < 240:
            dark += 1
    if dark < samples * 0.9:
        raise AssertionError(f"{label}: header strip is not dark ({dark}/{samples})")

    # The URL well border is LINE when idle and ACCENT while editing.
    border = None
    want = ACCENT if editing else LINE
    for y in range(url_y - 2, url_y + 3):
        c = pixel(rgba, width, url_x + url_w // 2, y)
        if near(c, want, 45):
            border = c
            break
    if border is None:
        got = pixel(rgba, width, url_x + url_w // 2, url_y)
        raise AssertionError(f"{label}: URL border missing at y={url_y}, got {got}")

    # Action prompts / URL border carry the accent. A handful of antialiased
    # pixels is enough; a missing header would be near zero.
    accent = 0
    for y in range(header_top + 2, header_top + int(HEADER_H) - 2, 2):
        for x in range(int(geometry["card_x"]) + 20, int(geometry["card_x"] + geometry["card_w"]) - 20, 2):
            if near(pixel(rgba, width, x, y), ACCENT, 70):
                accent += 1
    if accent < 8:
        raise AssertionError(f"{label}: header action prompts missing (accent={accent})")

    # A disabled forward capsule must dim its key and chevron together. The
    # SVG rasterizer ignores the tint color's alpha, so icons need the widget
    # opacity; without it the chevron stayed full-brightness next to a dim key.
    if forward_disabled:
        x0 = int(url_x + url_w) + 8
        mid = int(geometry["header_mid"])
        peak = 0
        for y in range(mid - 12, mid + 12):
            for x in range(x0, x0 + 80):
                peak = max(peak, sum(pixel(rgba, width, x, y)))
        if peak >= 280:
            raise AssertionError(
                f"{label}: disabled forward hint is not dim (peak={peak})"
            )


def assert_page_color(
    rgba: bytes,
    width: int,
    geometry: dict[str, float],
    expected: tuple[int, int, int],
    label: str,
) -> None:
    x = int(geometry["page_x"]) + 40
    y = int(geometry["page_y"]) + 40
    got = pixel(rgba, width, x, y)
    if not near(got, expected, 18):
        raise AssertionError(f"{label}: page color {got} != {expected}")


def req(path: str, value: object) -> dict[str, object]:
    return {"path": path, "value": value}


def build_plan(session: Path) -> tuple[list[dict[str, object]], list[dict[str, object]], dict[str, str]]:
    paths = {}
    for name, spec in PAGE.items():
        path = session / f"{name}.html"
        path.write_text(spec["html"], encoding="utf-8")
        paths[name] = path.as_uri()
    results_url = paths["results"] + "?q=meerkats"
    manual_url = paths["manual"]

    steps: list[dict[str, object]] = []
    shots: list[dict[str, object]] = []

    def action(name: str, params: dict[str, object] | None = None) -> dict[str, object]:
        return {"app": "browser", "action": name, "params": params or {}}

    def shot(
        label: str,
        requires: list[dict[str, object]],
        page: str,
        editing: bool,
        forward_disabled: bool = False,
    ) -> None:
        # The first capture settles the compositor (and can still read the
        # previous frame after a commit); the second is the asserted shot.
        settle = action("screenshot")
        settle["requires"] = requires
        steps.append(settle)
        step = action("screenshot")
        step["requires"] = requires
        steps.append(step)
        shots.append(
            {
                "label": label,
                "requires": requires,
                "color": PAGE[page]["color"],
                "editing": editing,
                "forward_disabled": forward_disabled,
            }
        )

    steps.append(action("navigate", {"url": paths["google"]}))
    shot(
        "google",
        [
            req("ui.url", paths["google"]),
            req("ui.title", "Google"),
            req("ui.url_editing", False),
        ],
        "google",
        False,
        forward_disabled=True,
    )

    # Search: click the field, type, submit.
    steps.append(action("mouse.click", {"x": 400, "y": 288}))
    steps.append(action("text", {"text": "meerkats"}))
    steps.append(action("key", {"key": "Enter"}))
    shot(
        "results",
        [
            req("ui.url", results_url),
            req("ui.title", "Results for meerkats"),
            req("engine.can_back", True),
        ],
        "results",
        False,
    )

    # Click the result link.
    steps.append(action("mouse.click", {"x": 300, "y": 275}))
    shot(
        "article",
        [
            req("ui.url", paths["article"]),
            req("ui.title", "Meerkat article"),
            req("engine.can_back", True),
        ],
        "article",
        False,
    )

    # Back to the results page.
    steps.append(action("pad.tap", {"button": "l1"}))
    shot(
        "back",
        [
            req("ui.url", results_url),
            req("ui.title", "Results for meerkats"),
            req("engine.can_forward", True),
        ],
        "results",
        False,
    )

    # Manual URL: Triangle opens the editor, type the destination, Enter.
    steps.append(action("pad.tap", {"button": "triangle"}))
    shot(
        "url_open",
        [
            req("ui.url", results_url),
            req("ui.url_editing", True),
            req("ui.url_draft_len", len(results_url)),
        ],
        "results",
        True,
    )
    # A hardware key closes the system keyboard and hands the character back
    # to the URL draft; the rest of the URL is typed with real-key semantics.
    steps.append(action("key", {"key": manual_url[0]}))
    steps.append(action("text", {"text": manual_url[1:]}))
    shot(
        "url_typed",
        [
            req("ui.url_editing", True),
            req("ui.url_draft_len", len(manual_url)),
        ],
        "results",
        True,
    )
    steps.append(action("key", {"key": "Enter"}))
    shot(
        "manual",
        [
            req("ui.url", manual_url),
            req("ui.title", "Manual destination"),
            req("ui.url_editing", False),
            req("ui.url_draft_len", 0),
        ],
        "manual",
        False,
    )

    # Circle: page -> tabs view, then tabs view -> Home.
    for _ in range(2):
        steps.append(
            {
                "app": "browser",
                "action": "pad.tap",
                "params": {"button": "circle"},
                "requires": [
                    req("ui.url", manual_url),
                    req("ui.title", "Manual destination"),
                    req("ui.url_editing", False),
                ],
            }
        )
    return steps, shots, paths


def run_replay(exe: Path, session: Path, keep: bool) -> Path:
    root = SCRIPT_DIR.parent
    env = os.environ.copy()
    qa_offscreen.apply_env(env)
    replay_root = Path(tempfile.gettempdir()) / "sdsc-replay"
    prefix = session.name
    before = set(replay_root.glob(f"{prefix}-*")) if replay_root.is_dir() else set()
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
    try:
        deadline = time.time() + 90
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
        if not done or sandbox is None:
            raise AssertionError("replay.done not observed before timeout")
        if not offscreen.report():
            raise AssertionError("offscreen QA failed")
        return sandbox
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


def parse_trace(sandbox: Path) -> list[dict[str, object]]:
    events: list[dict[str, object]] = []
    session_dir = next(sandbox.glob("trace/session-*"), None)
    if not session_dir:
        return events
    for path in sorted(session_dir.glob("*.jsonl")):
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(record, dict) and isinstance(record.get("event"), str):
                events.append(record)
    return events


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    exe = args.exe or root / "target" / "debug" / "sdsc-utils.exe"
    if not exe.is_file():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    session = Path(tempfile.gettempdir()) / "sdsc-browser-flows-qa" / stamp
    session.mkdir(parents=True, exist_ok=True)
    steps, shots, _paths = build_plan(session)
    with (session / "actions.jsonl").open("w", encoding="utf-8") as out:
        for index, step in enumerate(steps, 1):
            out.write(
                json.dumps(
                    {
                        "kind": "action",
                        "flow": 1,
                        "seq": index,
                        "repro": {
                            "app": step["app"],
                            "action": step["action"],
                            "params": step["params"],
                        },
                        "requires": step.get("requires", []),
                        "pre": {},
                    }
                )
                + "\n"
            )

    sandbox = run_replay(exe, session, args.keep)
    print(f"sandbox={sandbox}")

    events = parse_trace(sandbox)
    mismatches = [event for event in events if event.get("event") == "replay.mismatch"]
    if mismatches:
        for event in mismatches:
            print(
                f"error: replay mismatch action={event.get('action')} checks={event.get('checks')}",
                file=sys.stderr,
            )
        return 1
    if not any(
        event.get("event") == "browser.grid.open" and event.get("source") == "circle"
        for event in events
    ):
        print("error: Circle did not open the tabs view from the page", file=sys.stderr)
        return 1
    if not any(event.get("event") == "screen.home" for event in events):
        print("error: Circle in tabs view did not return Home", file=sys.stderr)
        return 1

    app_log = sandbox / "app.log"
    deadline = time.time() + 5
    while time.time() < deadline:
        if app_log.is_file() and "browser window close" in app_log.read_text(
            encoding="utf-8", errors="replace"
        ):
            break
        time.sleep(0.2)
    else:
        print("error: Browser window did not close after Circle", file=sys.stderr)
        return 1

    expected_shots = 2 * len(shots)
    shot_files = sorted(
        (sandbox / "shots").glob("shot-*.png"),
        key=lambda path: path.stat().st_mtime_ns,
    )
    deadline = time.time() + 10
    while len(shot_files) < expected_shots and time.time() < deadline:
        time.sleep(0.25)
        shot_files = sorted(
            (sandbox / "shots").glob("shot-*.png"),
            key=lambda path: path.stat().st_mtime_ns,
        )
    if len(shot_files) != expected_shots:
        print(
            f"error: expected {expected_shots} screenshots, got {len(shot_files)}",
            file=sys.stderr,
        )
        return 1
    asserted_shots = shot_files[1::2]

    for index, (shot, spec) in enumerate(zip(asserted_shots, shots), 1):
        width, height, rgba = read_png(shot)
        geometry = card_geometry(width, height)
        try:
            assert_header(
                rgba,
                width,
                geometry,
                editing=bool(spec["editing"]),
                forward_disabled=bool(spec["forward_disabled"]),
                label=f"step {index} {spec['label']}",
            )
            assert_page_color(
                rgba,
                width,
                geometry,
                spec["color"],
                f"step {index} {spec['label']}",
            )
        except AssertionError as err:
            print(f"error: {err} shot={shot}", file=sys.stderr)
            return 1
        print(
            f"ok step {index} {spec['label']} shot={shot.name} "
            f"url_w={geometry['url_w']:.0f}"
        )

    print(f"flows passed: {len(shots)} screenshots, {len(steps)} steps")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
