#!/usr/bin/env python3
"""Local install-flow QA console for SDSC Utils.

One loopback process serves:

    /                                   control page
    /api/state                          JSON snapshot
    /api/config                         POST versions/latest/toggles
    /api/build                          POST cargo build --release --features qa-feed
    /api/stage                          POST stage a portable folder + zip
    /api/install                        POST extract a staged portable into an install dir
    /api/uninstall                      POST remove an install + its QA appdata
    /api/launch                         POST launch debug/release/installed with QA env
    /api/stop                           POST stop the launched app
    /api/engine                         POST build the CEF DLC zip + sha256
    /api/clear                          POST clear QA appdata / installs
    /repos/LankyMoose/sdsc-utils/releases/latest
                                        fake GitHub release API
    /downloads/<file>                   staged portable / engine zips

Run:

    python scripts/install_qa_web.py
    python scripts/install_qa_web.py --port 8765 --open

The app reads `SDSC_UPDATE_API_URL` in debug builds or release builds
compiled with `--features qa-feed`; `SDSC_CEF_DLC_URL`/`SHA256` drive the
BrowserEngine install. QA launches also set `APPDATA` to a private data dir
and `SDSC_CEF_NO_BUILD_FALLBACK=1`, so a portable install without the `cef\\`
sidecar behaves like a clean machine and the engine must be installed from
the catalog.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import traceback
import webbrowser
import zipfile
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlparse

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "target" / "install-qa"
DOWNLOADS = OUT / "downloads"
PORTABLE = OUT / "portable"
INSTALLS = OUT / "installs"
APPDATA_ROOT = OUT / "appdata"
STATE_PATH = OUT / "state.json"

CEF_FILES = [
    "libcef.dll",
    "chrome_elf.dll",
    "d3dcompiler_47.dll",
    "dxcompiler.dll",
    "vk_swiftshader.dll",
    "vk_swiftshader_icd.json",
    "vulkan-1.dll",
    "icudtl.dat",
    "resources.pak",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "v8_context_snapshot.bin",
    "CREDITS.html",
]
CEF_DIRS = ["locales", "swiftshader"]
TAG_RE = re.compile(r"^v?\d+\.\d+\.\d+$")


def cargo_version() -> str:
    try:
        text = (REPO / "Cargo.toml").read_text(encoding="utf-8")
    except OSError:
        return "?"
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    return match.group(1) if match else "?"


def git_rev() -> tuple[str, bool]:
    try:
        rev = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=str(REPO),
            capture_output=True,
            text=True,
        )
        dirty = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=str(REPO),
            capture_output=True,
            text=True,
        )
        return (
            rev.stdout.strip() if rev.returncode == 0 else "?",
            bool(dirty.stdout.strip()),
        )
    except OSError:
        return "?", False


def sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def find_cef_dir() -> Path | None:
    candidates = []
    env = os.environ.get("CEF_PATH")
    if env:
        raw = Path(env)
        candidates.extend([raw / "cef_windows_x86_64", raw / "cef", raw])
    for profile in ("release", "debug"):
        build = REPO / "target" / profile / "build"
        if build.is_dir():
            candidates.extend(sorted(build.glob("cef-dll-sys-*/out/cef_windows_x86_64")))
    for path in candidates:
        if (path / "libcef.dll").is_file() and (path / "resources.pak").is_file():
            return path
    return None


def normalize_tag(raw: str) -> str | None:
    raw = raw.strip()
    if not TAG_RE.fullmatch(raw):
        return None
    return raw if raw.startswith("v") else f"v{raw}"


def version_key(tag: str) -> tuple[int, int, int]:
    parts = tag.lstrip("v").split(".")
    return int(parts[0]), int(parts[1]), int(parts[2])


def build_env() -> dict:
    """Environment for cargo builds: winget Links carries cmake/ninja on the
    usual Windows dev box, which a console launched outside that shell may not
    have inherited."""
    env = os.environ.copy()
    links = Path(os.environ.get("LOCALAPPDATA", "")) / "Microsoft" / "WinGet" / "Links"
    if links.is_dir():
        env["PATH"] = str(links) + os.pathsep + env.get("PATH", "")
    return env


def exe_for(flavor: str) -> Path:
    name = "sdsc-utils.exe" if os.name == "nt" else "sdsc-utils"
    if flavor == "debug":
        return REPO / "target" / "debug" / name
    return REPO / "target" / "release" / name


def appdata_for(name: str) -> Path:
    return APPDATA_ROOT / name


class Console:
    def __init__(self, port: int):
        self.port = port
        self.lock = threading.RLock()
        self.proc: subprocess.Popen | None = None
        self.running_flavor: str | None = None
        self.running_pid: int | None = None
        self.building = False
        self.building_since: float | None = None
        self.build_error: str | None = None
        self.staging = False
        self.stage_error: str | None = None
        self.engine: dict | None = None
        self.requests = deque(maxlen=60)
        self.installed: dict[str, dict] = {}
        self.server: ThreadingHTTPServer | None = None
        self.versions, self.latest, self.cef_mode = self.load_state()
        OUT.mkdir(parents=True, exist_ok=True)
        DOWNLOADS.mkdir(parents=True, exist_ok=True)
        PORTABLE.mkdir(parents=True, exist_ok=True)
        INSTALLS.mkdir(parents=True, exist_ok=True)
        APPDATA_ROOT.mkdir(parents=True, exist_ok=True)
        self.refresh_installed()

    # -- state -----------------------------------------------------------

    def default_versions(self):
        current = cargo_version()
        parts = current.split(".")
        try:
            major, minor, patch = (int(p) for p in parts[:3])
            nxt = f"v{major}.{minor}.{patch + 1}"
        except (ValueError, IndexError):
            nxt = "v1.7.4"
        return [
            {"tag": f"v{current}", "enabled": True},
            {"tag": nxt, "enabled": True},
        ], nxt, "full"

    def load_state(self):
        versions, latest, cef_mode = self.default_versions()
        try:
            raw = json.loads(STATE_PATH.read_text(encoding="utf-8"))
            loaded = [
                {"tag": normalize_tag(str(v.get("tag", ""))), "enabled": bool(v.get("enabled", True))}
                for v in raw.get("versions", [])
            ]
            loaded = [v for v in loaded if v["tag"]]
            if loaded:
                versions = loaded
            latest = normalize_tag(str(raw.get("latest", ""))) or latest
            cef_mode = str(raw.get("cef_mode", cef_mode))
            if cef_mode not in ("full", "loader", "none"):
                cef_mode = "full" if bool(raw.get("ship_cef", True)) else "none"
        except (OSError, ValueError):
            pass
        return versions, latest, cef_mode

    def save_state(self):
        with self.lock:
            STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
            STATE_PATH.write_text(
                json.dumps(
                    {
                        "versions": self.versions,
                        "latest": self.latest,
                        "cef_mode": self.cef_mode,
                    },
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )

    def refresh_installed(self):
        with self.lock:
            found: dict[str, dict] = {}
            for entry in INSTALLS.iterdir() if INSTALLS.is_dir() else []:
                if not entry.is_dir():
                    continue
                exe = entry / ("sdsc-utils.exe" if os.name == "nt" else "sdsc-utils")
                if exe.is_file():
                    found[entry.name] = {
                        "tag": entry.name,
                        "path": str(entry),
                        "exe": str(exe),
                        "sidecar": (entry / "cef" / "libcef.dll").is_file(),
                        "appdata": str(appdata_for(f"portable-{entry.name}")),
                    }
            self.installed = found

    def latest_enabled(self):
        with self.lock:
            enabled = [v["tag"] for v in self.versions if v["enabled"]]
            if not enabled:
                return None
            if self.latest in enabled:
                return self.latest
            return max(enabled, key=version_key)

    def feed_url(self):
        return f"http://127.0.0.1:{self.port}/repos/LankyMoose/sdsc-utils/releases/latest"

    def api_state(self):
        with self.lock:
            self.refresh_installed()
            running = False
            pid = None
            if self.proc is not None:
                if self.proc.poll() is None:
                    running = True
                    pid = self.proc.pid
                else:
                    self.proc = None
                    self.running_flavor = None
                    self.running_pid = None
            rev, dirty = git_rev()
            cef = find_cef_dir()
            latest = self.latest_enabled()
            return {
                "version": cargo_version(),
                "git": rev,
                "dirty": dirty,
                "feed_url": self.feed_url(),
                "latest": latest,
                "versions": self.versions,
                "cef_mode": self.cef_mode,
                "running": running,
                "running_pid": pid,
                "running_flavor": self.running_flavor,
                "building": self.building,
                "building_since": self.building_since,
                "build_error": self.build_error,
                "staging": self.staging,
                "stage_error": self.stage_error,
                "cef": str(cef) if cef else None,
                "engine": self.engine,
                "installed": sorted(self.installed.values(), key=lambda x: version_key(x["tag"])),
                "requests": list(self.requests),
                "downloads": f"http://127.0.0.1:{self.port}/downloads/",
            }

    # -- actions ---------------------------------------------------------

    def set_config(self, payload):
        with self.lock:
            if "cef_mode" in payload:
                mode = str(payload["cef_mode"])
                if mode in ("full", "loader", "none"):
                    self.cef_mode = mode
            if "versions" in payload:
                cleaned = []
                for item in payload["versions"]:
                    tag = normalize_tag(str(item.get("tag", "")))
                    if tag:
                        cleaned.append({"tag": tag, "enabled": bool(item.get("enabled", True))})
                if cleaned:
                    self.versions = cleaned
            if "latest" in payload:
                tag = normalize_tag(str(payload["latest"]))
                if tag:
                    self.latest = tag
            if "add" in payload:
                tag = normalize_tag(str(payload["add"]))
                if tag and all(v["tag"] != tag for v in self.versions):
                    self.versions.append({"tag": tag, "enabled": True})
            if "remove" in payload:
                tag = normalize_tag(str(payload["remove"]))
                self.versions = [v for v in self.versions if v["tag"] != tag]
                if self.latest == tag:
                    self.latest = self.latest_enabled() or ""
            self.save_state()
        return self.api_state()

    def build(self):
        with self.lock:
            if self.building:
                return
            self.building = True
            self.building_since = time.time()
            self.build_error = None
        threading.Thread(target=self._build_worker, daemon=True).start()

    def _build_worker(self):
        try:
            env = build_env()
            # Do not set CEF_PATH here: cef-dll-sys expects its binary
            # distribution/archive layout, not the extracted runtime dir.
            # An existing build's OUT_DIR is reused automatically.
            cmd = ["cargo", "build", "--release", "--features", "qa-feed"]
            self.log(f"$ {' '.join(cmd)}")
            result = subprocess.run(cmd, cwd=str(REPO), env=env)
            if result.returncode != 0:
                raise RuntimeError(f"cargo build failed ({result.returncode})")
            self.log("build: release qa-feed ready")
        except Exception as err:  # noqa: BLE001 - surfaced to the web UI
            with self.lock:
                self.build_error = str(err)
            self.log(f"build error: {err}")
        finally:
            with self.lock:
                self.building = False

    def stage(self):
        with self.lock:
            if self.staging:
                return
            self.staging = True
            self.stage_error = None
        threading.Thread(target=self._stage_worker, daemon=True).start()

    def _stage_worker(self):
        try:
            exe = exe_for("release")
            if not exe.is_file():
                raise RuntimeError("release exe missing; build release (qa-feed) first")
            version = cargo_version()
            out = PORTABLE / version
            if out.exists():
                shutil.rmtree(out)
            out.mkdir(parents=True)
            shutil.copy2(exe, out / exe.name)
            if self.cef_mode != "none":
                cef = find_cef_dir()
                if not cef:
                    raise RuntimeError("CEF runtime not found; build the project or set CEF_PATH")
                # Full: a runnable portable bundle in cef\ (resolver-valid).
                # Loader: the DLLs libcef needs at process load go next to the
                # exe, but resources/locales are omitted, so the resolver sees
                # "not installed" and the DLC flow can be exercised.
                loader_files = {
                    "libcef.dll",
                    "chrome_elf.dll",
                    "d3dcompiler_47.dll",
                    "dxcompiler.dll",
                    "vulkan-1.dll",
                    "vk_swiftshader.dll",
                }
                if self.cef_mode == "full":
                    dest = out / "cef"
                    dest.mkdir()
                    for name in CEF_FILES:
                        src = cef / name
                        if src.is_file():
                            shutil.copy2(src, dest / name)
                    for name in CEF_DIRS:
                        src = cef / name
                        if src.is_dir():
                            shutil.copytree(src, dest / name)
                else:
                    for name in loader_files:
                        src = cef / name
                        if src.is_file():
                            shutil.copy2(src, out / name)
            archive = DOWNLOADS / f"sdsc-utils-{version}-portable.zip"
            self._zip_dir(out, archive)
            self.log(f"stage: {archive.name} ({archive.stat().st_size:,} bytes)")
        except Exception as err:  # noqa: BLE001
            with self.lock:
                self.stage_error = str(err)
            self.log(f"stage error: {err}")
        finally:
            with self.lock:
                self.staging = False

    @staticmethod
    def _zip_dir(root: Path, archive: Path):
        if archive.exists():
            archive.unlink()
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=1) as zf:
            for path in root.rglob("*"):
                if path.is_file():
                    zf.write(path, path.relative_to(root))

    def install(self, tag: str):
        tag = normalize_tag(tag) or tag
        archive = DOWNLOADS / f"sdsc-utils-{tag}-portable.zip"
        if not archive.is_file():
            # The staged archive is named by Cargo version; allow installing
            # the current stage under the feed tag being tested.
            archives = sorted(DOWNLOADS.glob("sdsc-utils-*-portable.zip"))
            if not archives:
                raise RuntimeError("no staged portable zip; Stage first")
            archive = archives[-1]
        dest = INSTALLS / tag
        if dest.exists():
            shutil.rmtree(dest)
        dest.mkdir(parents=True)
        with zipfile.ZipFile(archive) as zf:
            for info in zf.infolist():
                target = (dest / info.filename).resolve()
                if not str(target).startswith(str(dest.resolve())):
                    raise RuntimeError("archive entry escapes install dir")
                zf.extract(info, dest)
        appdata = appdata_for(f"portable-{tag}")
        if appdata.is_dir():
            shutil.rmtree(appdata, ignore_errors=True)
        appdata.mkdir(parents=True, exist_ok=True)
        self.log(f"install: {tag} -> {dest}")
        self.refresh_installed()
        return self.api_state()

    def uninstall(self, tag: str):
        tag = normalize_tag(tag) or tag
        dest = INSTALLS / tag
        if dest.is_dir():
            shutil.rmtree(dest, ignore_errors=True)
        appdata = appdata_for(f"portable-{tag}")
        if appdata.is_dir():
            shutil.rmtree(appdata, ignore_errors=True)
        self.log(f"uninstall: {tag}")
        self.refresh_installed()
        return self.api_state()

    def stop(self):
        with self.lock:
            proc = self.proc
            self.proc = None
            self.running_flavor = None
            self.running_pid = None
        if proc is not None and proc.poll() is None:
            if os.name == "nt":
                subprocess.run(
                    ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                    capture_output=True,
                )
            else:
                proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
            self.log("launch: stopped")
        return self.api_state()

    def launch(self, flavor: str, debug_open: str = ""):
        self.stop()
        if flavor.startswith("installed:"):
            tag = normalize_tag(flavor.split(":", 1)[1]) or flavor.split(":", 1)[1]
            entry = self.installed.get(tag)
            if not entry:
                raise RuntimeError(f"{tag} is not installed")
            exe = Path(entry["exe"])
            cwd = Path(entry["path"])
            appdata = Path(entry["appdata"])
        else:
            exe = exe_for(flavor)
            cwd = REPO
            appdata = appdata_for(flavor)
            if not exe.is_file():
                raise RuntimeError(f"{flavor} exe missing: {exe}")
        appdata.mkdir(parents=True, exist_ok=True)
        env = os.environ.copy()
        env["APPDATA"] = str(appdata)
        env["SDSC_UPDATE_API_URL"] = self.feed_url()
        env["SDSC_CEF_NO_BUILD_FALLBACK"] = "1"
        if self.engine:
            env["SDSC_CEF_DLC_URL"] = self.engine["url"]
            env["SDSC_CEF_DLC_SHA256"] = self.engine["sha256"]
        if debug_open:
            env["SDSC_DEBUG_OPEN"] = debug_open
        flags = 0
        if os.name == "nt":
            flags = subprocess.CREATE_NEW_PROCESS_GROUP
        with self.lock:
            self.proc = subprocess.Popen(
                [str(exe)],
                cwd=str(cwd),
                env=env,
                creationflags=flags,
            )
            self.running_flavor = flavor
            self.running_pid = self.proc.pid
        self.log(f"launch: {flavor} pid={self.proc.pid} data={appdata}")
        return self.api_state()

    def build_engine(self):
        threading.Thread(target=self._engine_worker, daemon=True).start()
        return {"building": True}

    def _engine_worker(self):
        try:
            cef = find_cef_dir()
            if not cef:
                raise RuntimeError("CEF runtime not found; build the project or set CEF_PATH")
            archive = DOWNLOADS / "cef-engine.zip"
            tmp = DOWNLOADS / "cef-engine.zip.tmp"
            with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED, compresslevel=1) as zf:
                for name in CEF_FILES:
                    src = cef / name
                    if src.is_file():
                        zf.write(src, name)
                for name in CEF_DIRS:
                    src = cef / name
                    if src.is_dir():
                        for path in src.rglob("*"):
                            if path.is_file():
                                zf.write(path, path.relative_to(cef))
            os.replace(tmp, archive)
            with self.lock:
                self.engine = {
                    "path": str(archive),
                    "url": f"http://127.0.0.1:{self.port}/downloads/cef-engine.zip",
                    "sha256": sha256_file(archive),
                    "bytes": archive.stat().st_size,
                    "built_at": time.strftime("%Y-%m-%d %H:%M:%S"),
                }
            self.log(f"engine: {archive.name} ({archive.stat().st_size:,} bytes)")
        except Exception as err:  # noqa: BLE001
            self.log(f"engine error: {err}")

    def clear(self):
        self.stop()
        with self.lock:
            for path in (INSTALLS, APPDATA_ROOT, PORTABLE):
                if path.is_dir():
                    shutil.rmtree(path, ignore_errors=True)
                path.mkdir(parents=True, exist_ok=True)
            self.installed = {}
        self.log("clear: QA installs/appdata removed")
        return self.api_state()

    def log(self, line: str):
        with self.lock:
            self.requests.append(
                {"at": time.strftime("%H:%M:%S"), "line": line}
            )


class Handler(BaseHTTPRequestHandler):
    console: Console
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quieter default access log
        pass

    def _json(self, payload, status=200):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _html(self, html: str):
        body = html.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _body(self):
        length = int(self.headers.get("Content-Length", "0") or 0)
        if length <= 0:
            return {}
        try:
            return json.loads(self.rfile.read(length).decode("utf-8"))
        except ValueError:
            return {}

    def _download(self, name: str):
        path = DOWNLOADS / name
        if not path.is_file():
            self._json({"error": "not found"}, 404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/zip")
        self.send_header("Content-Length", str(path.stat().st_size))
        self.send_header("Content-Disposition", f'attachment; filename="{name}"')
        self.end_headers()
        with path.open("rb") as handle:
            shutil.copyfileobj(handle, self.wfile, 1 << 20)

    def do_GET(self):
        path = urlparse(self.path).path
        try:
            if path in ("/", "/index.html"):
                self._html(PAGE)
            elif path == "/api/state":
                self._json(self.console.api_state())
            elif path == "/repos/LankyMoose/sdsc-utils/releases/latest":
                latest = self.console.latest_enabled()
                if not latest:
                    self._json({"error": "no enabled release"}, 404)
                    return
                self.console.log(f"feed: latest {latest}")
                self._json(
                    {
                        "tag_name": latest,
                        "name": latest,
                        "html_url": f"https://github.com/LankyMoose/sdsc-utils/releases/tag/{latest}",
                        "assets": [],
                    }
                )
            elif path.startswith("/downloads/"):
                self._download(unquote(path[len("/downloads/"):]))
            else:
                self._json({"error": "not found"}, 404)
        except Exception as err:  # noqa: BLE001
            self.console.log(f"GET error: {err}")
            self._json({"error": str(err)}, 500)

    def do_POST(self):
        path = urlparse(self.path).path
        payload = self._body()
        try:
            if path == "/api/config":
                self._json(self.console.set_config(payload))
            elif path == "/api/build":
                self.console.build()
                self._json(self.console.api_state())
            elif path == "/api/stage":
                if "ship_cef" in payload:
                    self.console.ship_cef = bool(payload["ship_cef"])
                    self.console.save_state()
                self.console.stage()
                self._json(self.console.api_state())
            elif path == "/api/install":
                self._json(self.console.install(str(payload.get("tag", ""))))
            elif path == "/api/uninstall":
                self._json(self.console.uninstall(str(payload.get("tag", ""))))
            elif path == "/api/launch":
                self._json(
                    self.console.launch(
                        str(payload.get("flavor", "debug")),
                        str(payload.get("debug_open", "")),
                    )
                )
            elif path == "/api/stop":
                self._json(self.console.stop())
            elif path == "/api/engine":
                self._json(self.console.build_engine())
            elif path == "/api/clear":
                self._json(self.console.clear())
            else:
                self._json({"error": "not found"}, 404)
        except Exception as err:  # noqa: BLE001 - show the real error in the UI
            self.console.log(f"POST {path} error: {err}")
            self._json({"error": str(err)}, 500)


PAGE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SDSC Utils — install QA</title>
<style>
  :root { color-scheme: dark; --bg:#0d0f14; --panel:#161a22; --line:#2a3040;
          --ink:#e8eaf0; --muted:#8b93a7; --accent:#3d7dff; --ok:#38c172; --warn:#e0a030; }
  * { box-sizing: border-box; }
  body { margin:0; font:14px/1.45 system-ui, Segoe UI, sans-serif; background:var(--bg); color:var(--ink); }
  main { max-width: 1100px; margin: 0 auto; padding: 28px 22px 60px; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  .sub { color: var(--muted); margin-bottom: 22px; }
  .grid { display:grid; grid-template-columns: repeat(2, minmax(0,1fr)); gap: 14px; }
  .card { background: var(--panel); border:1px solid var(--line); border-radius: 14px; padding: 16px; }
  .card h2 { font-size: 14px; text-transform: uppercase; letter-spacing: .08em; color: var(--muted); margin: 0 0 12px; }
  .row { display:flex; gap:8px; align-items:center; flex-wrap:wrap; }
  .row + .row { margin-top: 8px; }
  button { background:#222939; color:var(--ink); border:1px solid var(--line); border-radius: 9px;
           padding: 7px 11px; font: inherit; cursor: pointer; }
  button:hover { border-color:#3a445c; background:#293247; }
  button.primary { background: var(--accent); border-color: transparent; color:#fff; }
  button.danger { color:#ffb4b4; border-color:#5a3030; }
  input, select { background:#0f131b; color:var(--ink); border:1px solid var(--line); border-radius:9px;
                  padding: 7px 9px; font: inherit; min-width: 0; }
  input[type=text] { flex:1 1 110px; }
  table { width:100%; border-collapse: collapse; }
  th, td { text-align:left; padding: 7px 4px; border-bottom:1px solid #202635; }
  th { color:var(--muted); font-weight: 500; font-size: 12px; }
  .tag { font-family: ui-monospace, Consolas, monospace; }
  .pill { display:inline-block; padding: 1px 7px; border-radius: 999px; border:1px solid var(--line); color:var(--muted); font-size: 12px; }
  .pill.ok { color: var(--ok); border-color:#234d35; }
  .pill.warn { color: var(--warn); border-color:#4d3f20; }
  .muted { color: var(--muted); }
  .mono { font-family: ui-monospace, Consolas, monospace; font-size: 12px; word-break:break-all; }
  pre { margin:0; max-height: 220px; overflow:auto; color: var(--muted); font-size:12px; }
  .full { grid-column: 1 / -1; }
  a { color:#8fb4ff; }
</style>
</head>
<body><main>
  <h1>SDSC Utils <span class="muted">install QA</span></h1>
  <div class="sub" id="status">loading…</div>
  <div class="grid">
    <section class="card">
      <h2>Release feed</h2>
      <div class="row"><span class="muted">Latest</span><span id="latest" class="tag">—</span></div>
      <div class="row"><span class="muted">API</span><span id="feed" class="mono">—</span></div>
      <table><thead><tr><th>Version</th><th>Enabled</th><th></th></tr></thead><tbody id="versions"></tbody></table>
      <div class="row"><input id="new-tag" type="text" placeholder="v1.7.4"><button onclick="addVersion()">Add</button>
        <button onclick="setLatest()">Set latest</button></div>
      <div class="row"><button onclick="saveConfig()">Save versions</button>
        <span class="muted">Current app version <span id="app-version">—</span></span></div>
    </section>
    <section class="card">
      <h2>Build &amp; stage</h2>
      <div class="row"><button class="primary" onclick="post('/api/build')">Build release (qa-feed)</button>
        <button onclick="post('/api/stage', {cef_mode: cefMode()})">Stage portable</button></div>
      <div class="row"><label class="muted">CEF sidecar</label>
        <select id="cef-mode" onchange="saveConfig()">
          <option value="full">full (runnable portable)</option>
          <option value="loader">loader-only (DLC install flow)</option>
          <option value="none">none (does not start)</option>
        </select></div>
      <div class="row"><span id="build" class="muted">—</span></div>
      <div class="row"><button onclick="post('/api/engine')">Build engine DLC zip</button></div>
      <div id="engine" class="muted mono"></div>
    </section>
    <section class="card">
      <h2>Installations</h2>
      <table><thead><tr><th>Version</th><th>CEF</th><th></th></tr></thead><tbody id="installs"></tbody></table>
      <div class="row"><button onclick="installStaged()">Install staged portable</button>
        <button class="danger" onclick="post('/api/clear')">Clear QA installs/appdata</button></div>
    </section>
    <section class="card">
      <h2>Launch</h2>
      <div class="row">
        <select id="flavor"><option value="debug">debug (target/debug)</option><option value="release">release qa-feed (target/release)</option></select>
        <input id="debug-open" type="text" placeholder="SDSC_DEBUG_OPEN (e.g. hub)">
        <button class="primary" onclick="launch()">Launch</button>
        <button onclick="post('/api/stop')">Stop</button>
      </div>
      <div class="row muted" id="running">—</div>
      <div class="row"><span class="muted">APPDATA is rewritten to a private QA dir; the update API + engine DLC env point here.</span></div>
    </section>
    <section class="card full">
      <h2>Requests / events</h2>
      <pre id="log">—</pre>
    </section>
  </div>
</main>
<script>
let S = null;
const $ = (id) => document.getElementById(id);
async function getState(){ S = await (await fetch('/api/state')).json(); render(); }
function cefMode(){ return $('cef-mode').value; }
function render(){
  $('status').textContent = `v${S.version} · ${S.git}${S.dirty ? ' dirty' : ''} · ${S.cef ? 'CEF found' : 'CEF missing'}`;
  $('latest').textContent = S.latest || '—';
  $('feed').textContent = S.feed_url;
  $('app-version').textContent = S.version;
  $('cef-mode').value = S.cef_mode || 'full';
  $('versions').innerHTML = S.versions.map((v) =>
    `<tr><td class="tag">${v.tag}</td><td>${v.enabled ? '<span class="pill ok">on</span>' : '<span class="pill">off</span>'}</td>
     <td><button onclick="toggleVersion('${v.tag}')">Toggle</button>
         <button onclick="removeVersion('${v.tag}')">Remove</button></td></tr>`).join('');
  $('installs').innerHTML = S.installed.map((i) =>
    `<tr><td class="tag">${i.tag}</td><td>${i.sidecar ? '<span class="pill ok">sidecar</span>' : '<span class="pill warn">missing</span>'}</td>
     <td><button onclick="post('/api/install', {tag:'${i.tag}'})">Reinstall</button>
         <button onclick="post('/api/uninstall', {tag:'${i.tag}'})">Uninstall</button>
         <button onclick="launchInstalled('${i.tag}')">Launch</button></td></tr>`).join('');
  $('build').textContent = S.building ? 'building release…' : (S.build_error ? `build error: ${S.build_error}` : (S.stage_error ? `stage error: ${S.stage_error}` : 'ready'));
  $('engine').textContent = S.engine ? `${S.engine.url}\\n${S.engine.sha256} (${S.engine.bytes} bytes)` : 'engine zip not built';
  $('running').textContent = S.running ? `running ${S.running_flavor} pid ${S.running_pid}` : 'not running';
  $('log').textContent = S.requests.map((r) => `${r.at}  ${r.line}`).join('\\n') || '—';
  const select = $('flavor');
  const saved = select.value;
  select.innerHTML = '<option value="debug">debug (target/debug)</option><option value="release">release qa-feed (target/release)</option>' +
    S.installed.map((i) => `<option value="installed:${i.tag}">installed ${i.tag}</option>`).join('');
  if ([...select.options].some((o) => o.value === saved)) select.value = saved;
}
function toggleVersion(tag){ const v = S.versions.find((x) => x.tag === tag); post('/api/config', {versions: S.versions.map((x) => x.tag === tag ? {tag, enabled: !x.enabled} : x), latest: S.latest}); }
function removeVersion(tag){ post('/api/config', {remove: tag}); }
function addVersion(){ const tag = $('new-tag').value.trim(); if (tag) { post('/api/config', {add: tag}); $('new-tag').value = ''; } }
function installStaged(){ post('/api/install', {tag: S.latest || ('v' + S.version)}); }
function saveConfig(){ post('/api/config', {versions: S.versions, latest: S.latest, cef_mode: cefMode()}); }
function launch(){ post('/api/launch', {flavor: $('flavor').value, debug_open: $('debug-open').value.trim()}); }
function launchInstalled(tag){ $('flavor').value = 'installed:' + tag; post('/api/launch', {flavor: 'installed:' + tag, debug_open: $('debug-open').value.trim()}); }
async function post(path, body){ try { const r = await fetch(path, {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body || {})}); const j = await r.json(); if (j.error) alert(j.error); } catch(e){ alert(e); } await getState(); }
function setLatest(){ const tag = prompt('Latest version tag', S.latest || ''); if (tag) post('/api/config', {latest: tag}); }
setInterval(getState, 2000); getState();
</script>
</body></html>"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--open", action="store_true", dest="open_browser")
    args = parser.parse_args()
    console = Console(args.port)
    Handler.console = console
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    console.server = server
    console.log(f"console: http://127.0.0.1:{args.port}/")
    print(f"install QA console: http://127.0.0.1:{args.port}/")
    if args.open_browser:
        threading.Timer(0.4, lambda: webbrowser.open(f"http://127.0.0.1:{args.port}/")).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        console.stop()
        server.server_close()


if __name__ == "__main__":
    try:
        main()
    except Exception:  # pragma: no cover - CLI surface
        traceback.print_exc()
        sys.exit(1)
