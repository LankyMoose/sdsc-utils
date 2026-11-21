#!/usr/bin/env python3
"""Local install-flow QA console for SDSC Utils.

One loopback process serves:

    /                                   control page
    /api/state                          JSON snapshot
    /api/config                         POST manifest + engine choice
    /api/prepare                        POST build, stage, launch (async)
    /api/relaunch                       POST rerun the last install
    /api/stop                           POST stop the launched app
    /api/clear                          POST remove QA installs/appdata
    /repos/LankyMoose/sdsc-utils/releases/latest
                                        fake GitHub release API
    /downloads/<file>                   engine DLC zip

The point of the console is to run a standalone install that has **no CEF
engine yet**. `Build & launch` builds the app, copies only the exe into the
QA install, points `APPDATA` at a private directory, and starts it. The hub
then shows Browser as not installed; its Install action downloads the engine
zip from this server and attaches CEF without restarting the app.

Check `Bundle browser engine` to stage `cef\\` beside the exe instead, for the
already-installed path. `Rebuild install` resets the install and its private
appdata, including any engine downloaded during a previous walk-through.

The app reads `SDSC_UPDATE_API_URL` in debug builds or release builds
compiled with `--features qa-feed`; `SDSC_CEF_DLC_URL`/`SHA256` drive the
engine install. `SDSC_CEF_NO_BUILD_FALLBACK=1` stops the app from finding the
build machine's CEF runtime, so a clean install really is clean.
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
GITHUB_RELEASE_PREFIX = "https://github.com/"


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
        self.preparing = False
        self.prepare_step = ""
        self.prepare_error: str | None = None
        self.engine: dict | None = None
        self.requests = deque(maxlen=80)
        self.installed: dict[str, dict] = {}
        self.server: ThreadingHTTPServer | None = None
        self.last_flavor: str | None = None
        self.last_debug_open: str = ""
        self.last_controller_input = True
        self.manifest, self.engine_bundled = self.load_state()
        OUT.mkdir(parents=True, exist_ok=True)
        DOWNLOADS.mkdir(parents=True, exist_ok=True)
        PORTABLE.mkdir(parents=True, exist_ok=True)
        INSTALLS.mkdir(parents=True, exist_ok=True)
        APPDATA_ROOT.mkdir(parents=True, exist_ok=True)
        self.refresh_installed()
        self.load_engine_state()

    # -- state -----------------------------------------------------------

    def default_manifest(self):
        current = cargo_version()
        parts = current.split(".")
        try:
            major, minor, patch = (int(p) for p in parts[:3])
            tag = f"v{major}.{minor}.{patch + 1}"
        except (ValueError, IndexError):
            tag = "v1.7.4"
        return {
            "tag_name": tag,
            "name": tag,
            "html_url": f"https://github.com/LankyMoose/sdsc-utils/releases/tag/{tag}",
            "body": "",
            "assets": [],
        }

    def load_state(self):
        manifest = self.default_manifest()
        engine_bundled = False
        try:
            raw = json.loads(STATE_PATH.read_text(encoding="utf-8"))
            saved = raw.get("manifest")
            if isinstance(saved, dict):
                manifest.update(saved)
            engine_bundled = bool(raw.get("engine_bundled", False))
        except (OSError, ValueError):
            pass
        tag = normalize_tag(str(manifest.get("tag_name", "")))
        if not tag:
            tag = normalize_tag(str(manifest.get("name", "")))
        if not tag:
            tag = self.default_manifest()["tag_name"]
        manifest["tag_name"] = tag
        if not str(manifest.get("name", "")).strip():
            manifest["name"] = tag
        if not str(manifest.get("body", "")).strip():
            manifest["body"] = ""
        if not isinstance(manifest.get("assets"), list):
            manifest["assets"] = []
        url = str(manifest.get("html_url", "")).strip()
        if not url:
            url = f"https://github.com/LankyMoose/sdsc-utils/releases/tag/{tag}"
        manifest["html_url"] = url
        return manifest, engine_bundled

    def save_state(self):
        with self.lock:
            STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
            STATE_PATH.write_text(
                json.dumps(
                    {
                        "manifest": self.manifest,
                        "engine_bundled": self.engine_bundled,
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
                if not exe.is_file():
                    continue
                marker = entry / ".qa-mode"
                mode = marker.read_text(encoding="utf-8").strip() if marker.is_file() else ""
                appdata = appdata_for(f"portable-{entry.name}")
                engine_data = appdata / "sdsc-utils" / "cef-engine"
                found[entry.name] = {
                    "tag": entry.name,
                    "path": str(entry),
                    "exe": str(exe),
                    "sidecar": (entry / "cef" / "libcef.dll").is_file(),
                    "engine_appdata": (engine_data / "libcef.dll").is_file(),
                    "mode": ":".join(mode.split(":")[:2]),
                    "appdata": str(appdata),
                }
            self.installed = found

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
            return {
                "version": cargo_version(),
                "git": rev,
                "dirty": dirty,
                "feed_url": self.feed_url(),
                "latest": self.manifest.get("tag_name", ""),
                "manifest": self.manifest,
                "engine_bundled": self.engine_bundled,
                "running": running,
                "running_pid": pid,
                "running_flavor": self.running_flavor,
                "last_flavor": self.last_flavor,
                "last_debug_open": self.last_debug_open,
                "last_controller_input": self.last_controller_input,
                "preparing": self.preparing,
                "prepare_step": self.prepare_step,
                "prepare_error": self.prepare_error,
                "building": self.building,
                "building_since": self.building_since,
                "build_error": self.build_error,
                "cef": str(cef) if cef else None,
                "engine": self.engine,
                "installed": sorted(self.installed.values(), key=lambda x: version_key(x["tag"])),
                "requests": list(self.requests),
                "downloads": f"http://127.0.0.1:{self.port}/downloads/",
            }

    # -- actions ---------------------------------------------------------

    def load_engine_state(self):
        archive = DOWNLOADS / "cef-engine.zip"
        if archive.is_file():
            self.engine = {
                "path": str(archive),
                "url": f"http://127.0.0.1:{self.port}/downloads/cef-engine.zip",
                "sha256": sha256_file(archive),
                "bytes": archive.stat().st_size,
                "built_at": time.strftime(
                    "%Y-%m-%d %H:%M:%S", time.localtime(archive.stat().st_mtime)
                ),
            }

    def ensure_engine(self):
        with self.lock:
            if self.engine and Path(self.engine.get("path", "")).is_file():
                return
        self._engine_worker()
        with self.lock:
            if not self.engine:
                raise RuntimeError("engine DLC zip unavailable (CEF runtime not found)")

    def _engine_worker(self):
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

    def build_flavor(self, flavor: str):
        with self.lock:
            self.building = True
            self.building_since = time.time()
            self.build_error = None
        try:
            cmd = ["cargo", "build", "--bin", "sdsc-utils"]
            if flavor == "release":
                cmd.extend(["--release", "--features", "qa-feed"])
            self.log(f"$ {' '.join(cmd)}")
            result = subprocess.run(cmd, cwd=str(REPO), env=build_env())
            if result.returncode != 0:
                raise RuntimeError(f"cargo build failed ({result.returncode})")
            self.log(f"build: {flavor} ready")
        except Exception as err:  # noqa: BLE001 - surfaced in the web UI
            with self.lock:
                self.build_error = str(err)
            raise
        finally:
            with self.lock:
                self.building = False
                self.building_since = None

    def _stage_install_sync(
        self,
        flavor: str,
        engine_bundled: bool,
        tag: str,
        marker: str,
        reset_appdata: bool,
    ):
        exe = exe_for(flavor)
        if not exe.is_file():
            raise RuntimeError(f"{flavor} exe missing: {exe}")
        label = "engine" if engine_bundled else "clean"
        root = PORTABLE / f"{tag}-{flavor}-{label}"
        if root.exists():
            shutil.rmtree(root)
        root.mkdir(parents=True)
        shutil.copy2(exe, root / exe.name)
        if engine_bundled:
            cef = find_cef_dir()
            if not cef:
                raise RuntimeError("CEF runtime not found; build the project or set CEF_PATH")
            dest = root / "cef"
            dest.mkdir()
            for name in CEF_FILES:
                src = cef / name
                if src.is_file():
                    shutil.copy2(src, dest / name)
            for name in CEF_DIRS:
                src = cef / name
                if src.is_dir():
                    shutil.copytree(src, dest / name)
        archive = DOWNLOADS / f"sdsc-utils-{tag}-{flavor}-{label}-portable.zip"
        self._zip_dir(root, archive)
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
        (dest / ".qa-mode").write_text(marker, encoding="utf-8")
        appdata = appdata_for(f"portable-{tag}")
        if reset_appdata and appdata.is_dir():
            shutil.rmtree(appdata, ignore_errors=True)
        appdata.mkdir(parents=True, exist_ok=True)
        self.refresh_installed()
        self.log(f"install: {tag} ({flavor}, engine={'bundled' if engine_bundled else 'missing'})")

    @staticmethod
    def _zip_dir(root: Path, archive: Path):
        if archive.exists():
            archive.unlink()
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=1) as zf:
            for path in root.rglob("*"):
                if path.is_file():
                    zf.write(path, path.relative_to(root))

    def prepare_launch(self, payload):
        with self.lock:
            if self.preparing:
                return self.api_state()
        config = {
            key: payload[key]
            for key in ("manifest", "version", "html_url", "engine_bundled")
            if key in payload
        }
        if config:
            self.set_config(config)
        with self.lock:
            if self.preparing:
                return self.api_state()
            self.preparing = True
            self.prepare_step = "starting"
            self.prepare_error = None
        threading.Thread(
            target=self._prepare_worker, args=(dict(payload),), daemon=True
        ).start()
        return self.api_state()

    def _prepare_worker(self, payload):
        try:
            flavor = str(payload.get("flavor", "debug"))
            if flavor not in ("debug", "release"):
                raise RuntimeError(f"unknown flavor {flavor!r}")
            bundle = bool(payload.get("engine_bundled", self.engine_bundled))
            open_hub = bool(payload.get("open_hub", True))
            restage = bool(payload.get("restage", False))
            controller_input = bool(payload.get("controller_input", True))
            self.log(
                f"prepare: {flavor}, engine={'bundled' if bundle else 'missing'}"
                + (", reset" if restage else "")
                + (", controller on" if controller_input else ", controller off")
            )
            self.stop()

            self._set_prepare_step("building")
            self.build_flavor(flavor)
            if not bundle:
                self._set_prepare_step("packaging engine")
                self.ensure_engine()

            tag = normalize_tag(str(self.manifest.get("tag_name", "")))
            if not tag:
                raise RuntimeError("version must look like v1.2.3")
            source = exe_for(flavor)
            if not source.is_file():
                raise RuntimeError(f"{flavor} exe missing: {source}")
            label = "engine" if bundle else "clean"
            want = f"{flavor}:{label}"
            marker_path = INSTALLS / tag / ".qa-mode"
            old_marker = (
                marker_path.read_text(encoding="utf-8").strip()
                if marker_path.is_file()
                else ""
            )
            old_mode = ":".join(old_marker.split(":")[:2])
            stamp = f"{want}:{source.stat().st_mtime_ns}:{source.stat().st_size}"
            if restage or old_marker != stamp:
                self._set_prepare_step("installing")
                self._stage_install_sync(
                    flavor,
                    bundle,
                    tag,
                    stamp,
                    reset_appdata=restage or old_mode != want,
                )

            self._set_prepare_step("launching")
            self.launch(
                f"installed:{tag}",
                "hub" if open_hub and flavor == "debug" else "",
                controller_input,
            )
        except Exception as err:  # noqa: BLE001 - surfaced in the web UI
            with self.lock:
                self.prepare_error = str(err)
            self.log(f"prepare error: {err}")
        finally:
            with self.lock:
                self.preparing = False
                self.prepare_step = ""

    def _set_prepare_step(self, step: str):
        with self.lock:
            self.prepare_step = step

    def relaunch(self):
        if not self.last_flavor:
            raise RuntimeError("nothing has been launched yet")
        return self.launch(
            self.last_flavor,
            self.last_debug_open,
            self.last_controller_input,
        )

    def set_config(self, payload):
        with self.lock:
            if isinstance(payload.get("manifest"), dict):
                manifest = dict(payload["manifest"])
                tag = normalize_tag(str(manifest.get("tag_name", "")))
                if not tag:
                    raise RuntimeError("manifest tag_name must look like v1.2.3")
                manifest["tag_name"] = tag
                if not str(manifest.get("name", "")).strip():
                    manifest["name"] = tag
                if not str(manifest.get("body", "")).strip():
                    manifest["body"] = ""
                if not isinstance(manifest.get("assets"), list):
                    manifest["assets"] = []
                self.manifest = manifest
            if "version" in payload:
                tag = normalize_tag(str(payload["version"]))
                if not tag:
                    raise RuntimeError("version must look like v1.2.3")
                before = str(self.manifest.get("tag_name", ""))
                if str(self.manifest.get("name", "")) in ("", before):
                    self.manifest["name"] = tag
                url = str(self.manifest.get("html_url", ""))
                if not url or "/releases/tag/" in url:
                    self.manifest["html_url"] = (
                        f"https://github.com/LankyMoose/sdsc-utils/releases/tag/{tag}"
                    )
                self.manifest["tag_name"] = tag
            if "html_url" in payload:
                url = str(payload["html_url"]).strip()
                if url and not url.startswith(GITHUB_RELEASE_PREFIX):
                    raise RuntimeError(
                        f"release URL must start with {GITHUB_RELEASE_PREFIX}"
                    )
                if url:
                    self.manifest["html_url"] = url
            if "engine_bundled" in payload:
                self.engine_bundled = bool(payload["engine_bundled"])
            self.save_state()
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

    def launch(
        self,
        flavor: str,
        debug_open: str = "",
        controller_input: bool = True,
    ):
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
        # Always use debug quiet placement: secondary monitor, no focus steal.
        # Manual testing may opt pad edges back in; automation must not.
        env["SDSC_DEBUG_QUIET"] = "1"
        env["SDSC_DEBUG_INPUT"] = "1" if controller_input else "0"
        if self.engine:
            env["SDSC_CEF_DLC_URL"] = self.engine["url"]
            env["SDSC_CEF_DLC_SHA256"] = self.engine["sha256"]
        if debug_open:
            env["SDSC_DEBUG_OPEN"] = debug_open
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        with self.lock:
            self.proc = subprocess.Popen(
                [str(exe)],
                cwd=str(cwd),
                env=env,
                creationflags=flags,
            )
            self.running_flavor = flavor
            self.running_pid = self.proc.pid
            self.last_flavor = flavor
            self.last_debug_open = debug_open
            self.last_controller_input = controller_input
        self.log(
            f"launch: {flavor} pid={self.proc.pid} "
            f"controller={'on' if controller_input else 'off'} data={appdata}"
        )
        return self.api_state()

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
            self.requests.append({"at": time.strftime("%H:%M:%S"), "line": line})


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
                manifest = dict(self.console.manifest)
                manifest.setdefault("name", manifest.get("tag_name", ""))
                manifest.setdefault("assets", [])
                self.console.log(f"feed: {manifest.get('tag_name', '?')}")
                self._json(manifest)
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
            elif path == "/api/prepare":
                self._json(self.console.prepare_launch(payload))
            elif path == "/api/relaunch":
                self._json(self.console.relaunch())
            elif path == "/api/stop":
                self._json(self.console.stop())
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
<title>SDSC Utils — app lab</title>
<style>
  :root { color-scheme: dark; --bg:#0d0f14; --panel:#161a22; --line:#2a3040;
          --ink:#e8eaf0; --muted:#8b93a7; --accent:#3d7dff; --ok:#38c172; --warn:#e0a030; --bad:#ff6b6b; }
  * { box-sizing: border-box; }
  body { margin:0; font:14px/1.45 system-ui, Segoe UI, sans-serif; background:var(--bg); color:var(--ink); }
  main { max-width: 780px; margin: 0 auto; padding: 30px 22px 60px; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  h2 { font-size: 12px; text-transform: uppercase; letter-spacing: .08em; color: var(--muted); margin: 0 0 12px; }
  .sub { color: var(--muted); margin-bottom: 22px; }
  .card { background: var(--panel); border:1px solid var(--line); border-radius: 14px; padding: 16px; margin-bottom: 14px; }
  .row { display:flex; gap:8px; align-items:center; flex-wrap:wrap; }
  .row + .row { margin-top: 10px; }
  label { color: var(--muted); display:flex; gap:6px; align-items:center; }
  input, select, textarea { background:#0f131b; color:var(--ink); border:1px solid var(--line); border-radius:9px;
                  padding: 7px 9px; font: inherit; min-width: 0; }
  input[type=text] { flex:1 1 150px; }
  textarea { width:100%; min-height: 190px; font-family: ui-monospace, Consolas, monospace; font-size: 12px; resize: vertical; }
  button { background:#222939; color:var(--ink); border:1px solid var(--line); border-radius: 9px;
           padding: 8px 12px; font: inherit; cursor: pointer; }
  button:hover { border-color:#3a445c; background:#293247; }
  button.primary { background: var(--accent); border-color: transparent; color:#fff; }
  button.ghost { background: transparent; color: var(--muted); }
  button:disabled { opacity: .5; cursor: default; }
  .mono { font-family: ui-monospace, Consolas, monospace; font-size: 12px; word-break:break-all; }
  .muted { color: var(--muted); }
  .hint { color: var(--muted); font-size: 12px; }
  .error { color: var(--bad); }
  details { margin-top: 10px; }
  summary { cursor:pointer; color: var(--muted); }
  pre { margin:0; max-height: 220px; overflow:auto; color: var(--muted); font-size:12px; }
  #info { white-space: pre-wrap; }
</style>
</head>
<body><main>
  <h1>SDSC Utils <span class="muted">app lab</span></h1>
  <div class="sub" id="status">loading…</div>

  <section class="card">
    <h2>Version &amp; manifest</h2>
    <div class="row">
      <label>App version <input id="version" type="text" placeholder="v1.7.4"></label>
      <label>Release URL <input id="html-url" type="text"></label>
    </div>
    <details>
      <summary>Manifest JSON (tag, URL, notes, assets)</summary>
      <div class="row"><textarea id="manifest"></textarea></div>
    </details>
    <div class="row"><button onclick="saveManifest()">Save manifest</button>
      <span class="muted" id="manifest-note"></span></div>
    <div class="row hint">The mock release feed serves this manifest. The release URL must stay on https://github.com/ for the app's validator.</div>
  </section>

  <section class="card">
    <h2>Build &amp; install</h2>
    <div class="row">
      <label>Build <select id="flavor">
        <option value="debug">Debug</option>
        <option value="release">Release (qa-feed)</option>
      </select></label>
      <label><input id="engine" type="checkbox"> Bundle browser engine</label>
      <label><input id="open-hub" type="checkbox" checked> Open hub (debug)</label>
      <label><input id="controller-input" type="checkbox" checked> Allow controller input</label>
      <label><input id="restage" type="checkbox"> Rebuild install</label>
    </div>
    <div class="row">
      <button class="primary" id="go" onclick="prepare()">Build &amp; launch</button>
      <button id="relaunch" onclick="relaunch()" disabled>Relaunch</button>
      <button onclick="stop()">Stop</button>
    </div>
    <div class="row error" id="error" style="display:none"></div>
    <div class="row muted" id="run">—</div>
    <div class="row hint">Leave <b>Bundle browser engine</b> unchecked to start with no CEF at all: only the exe is staged, and the hub's Apps → Browser → Install walks the DLC flow. Check it to test an already-installed engine. <b>Rebuild install</b> resets the private appdata, including a previously downloaded engine. The app always opens on the secondary monitor without stealing focus; uncheck <b>Allow controller input</b> for automated runs so the physical controller cannot navigate or close it.</div>
  </section>

  <section class="card">
    <h2>Status</h2>
    <div id="info" class="mono muted">—</div>
  </section>

  <details><summary>Log</summary><pre id="log">—</pre></details>
</main>
<script>
let S = null;
let initialized = false;
const $ = (id) => document.getElementById(id);
async function getState(){
  try { S = await (await fetch('/api/state')).json(); render(); }
  catch (e) { $('status').textContent = 'server unavailable: ' + e; }
}
function render(){
  if (!S) return;
  const feed = (S.manifest && S.manifest.tag_name) || S.latest || '?';
  let status = `app ${S.version} · feed ${feed} · `;
  if (S.preparing) status += `working: ${S.prepare_step || 'starting'}`;
  else if (S.running) status += `running ${S.running_flavor}`;
  else status += 'not running';
  $('status').textContent = status;
  if (!initialized) {
    $('version').value = feed;
    $('html-url').value = (S.manifest && S.manifest.html_url) || '';
    $('manifest').value = JSON.stringify(S.manifest || {}, null, 2);
    $('engine').checked = !!S.engine_bundled;
    $('controller-input').checked = S.last_controller_input !== false;
    initialized = true;
  }
  $('go').disabled = !!S.preparing;
  $('go').textContent = S.preparing ? 'Working…' : 'Build & launch';
  $('relaunch').disabled = !!S.preparing || !S.last_flavor;
  $('run').textContent = S.running
    ? `running ${S.running_flavor} pid ${S.running_pid}`
    : (S.last_flavor ? `last: ${S.last_flavor}` : 'not launched yet');
  const err = S.prepare_error || S.build_error || '';
  $('error').textContent = err ? `error: ${err}` : '';
  $('error').style.display = err ? '' : 'none';
  const engine = S.engine ? `DLC ${Math.round(S.engine.bytes / 1048576)} MB` : 'DLC not built';
  $('info').textContent =
    `feed ${S.feed_url || ''}\\n` +
    `app ${S.version} · build CEF ${S.cef ? 'found' : 'missing'} · ${engine}\\n` +
    `installs: ${(S.installed || []).map((i) =>
      i.tag + (i.sidecar ? ' (engine bundled)' : ' (engine missing)') +
      (i.engine_appdata ? ' + downloaded' : '')).join(', ') || '—'}`;
  $('log').textContent = (S.requests || []).map((r) => `${r.at}  ${r.line}`).join('\\n') || '—';
}
function manifestValue(){
  let m = {};
  try { m = JSON.parse($('manifest').value || '{}'); } catch (e) { m = {}; }
  m.tag_name = $('version').value.trim();
  m.html_url = $('html-url').value.trim();
  if (!m.name) m.name = m.tag_name;
  if (!m.body) m.body = '';
  if (!Array.isArray(m.assets)) m.assets = [];
  return m;
}
async function post(path, body){
  const response = await fetch(path, {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body || {})});
  const data = await response.json();
  if (data.error) throw new Error(data.error);
  return data;
}
async function saveManifest(){
  try {
    await post('/api/config', {manifest: manifestValue(), engine_bundled: $('engine').checked});
    $('manifest-note').textContent = 'saved';
    await getState();
  } catch (e) { $('manifest-note').textContent = String(e); }
}
async function prepare(){
  try {
    await post('/api/prepare', {
      manifest: manifestValue(),
      flavor: $('flavor').value,
      engine_bundled: $('engine').checked,
      open_hub: $('open-hub').checked,
      controller_input: $('controller-input').checked,
      restage: $('restage').checked
    });
    await getState();
  } catch (e) {
    alert(String(e));
  }
}
async function relaunch(){
  try { await post('/api/relaunch', {}); await getState(); }
  catch (e) { alert(String(e)); }
}
async function stop(){
  try { await post('/api/stop', {}); await getState(); }
  catch (e) { alert(String(e)); }
}
setInterval(getState, 1000);
getState();
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
        threading.Timer(
            0.4, lambda: webbrowser.open(f"http://127.0.0.1:{args.port}/")
        ).start()
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
