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
        self.last_flavor: str | None = None
        self.last_debug_open: str = ""
        self.preparing = False
        self.versions, self.latest, self.cef_mode, self.manifest = self.load_state()
        OUT.mkdir(parents=True, exist_ok=True)
        DOWNLOADS.mkdir(parents=True, exist_ok=True)
        PORTABLE.mkdir(parents=True, exist_ok=True)
        INSTALLS.mkdir(parents=True, exist_ok=True)
        APPDATA_ROOT.mkdir(parents=True, exist_ok=True)
        self.refresh_installed()
        self.load_engine_state()

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

    def default_manifest(self, version: str):
        return {
            "tag_name": version,
            "name": version,
            "html_url": f"https://github.com/LankyMoose/sdsc-utils/releases/tag/{version}",
            "body": "",
            "assets": [],
        }

    def load_state(self):
        versions, latest, cef_mode = self.default_versions()
        manifest = self.default_manifest(latest)
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
            saved_manifest = raw.get("manifest")
            if isinstance(saved_manifest, dict):
                manifest.update(saved_manifest)
        except (OSError, ValueError):
            pass
        tag = normalize_tag(str(manifest.get("tag_name", "")))
        if tag:
            manifest["tag_name"] = tag
            latest = tag
        else:
            manifest["tag_name"] = latest
        if not str(manifest.get("html_url", "")).strip():
            manifest["html_url"] = (
                f"https://github.com/LankyMoose/sdsc-utils/releases/tag/{latest}"
            )
        return versions, latest, cef_mode, manifest

    def save_state(self):
        with self.lock:
            STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
            STATE_PATH.write_text(
                json.dumps(
                    {
                        "versions": self.versions,
                        "latest": self.latest,
                        "manifest": self.manifest,
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
                "latest": self.manifest.get("tag_name", latest),
                "versions": self.versions,
                "manifest": self.manifest,
                "cef_mode": self.cef_mode,
                "running": running,
                "running_pid": pid,
                "running_flavor": self.running_flavor,
                "last_flavor": self.last_flavor,
                "last_debug_open": self.last_debug_open,
                "preparing": self.preparing,
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

    def _stage_install_sync(self, flavor: str, cef_mode: str, tag: str):
        exe = exe_for(flavor)
        if not exe.is_file():
            raise RuntimeError(f"{flavor} exe missing: {exe}")
        root = PORTABLE / f"{tag}-{flavor}-{cef_mode}"
        if root.exists():
            shutil.rmtree(root)
        root.mkdir(parents=True)
        shutil.copy2(exe, root / exe.name)
        if cef_mode != "none":
            cef = find_cef_dir()
            if not cef:
                raise RuntimeError("CEF runtime not found; build the project or set CEF_PATH")
            loader_files = {
                "libcef.dll",
                "chrome_elf.dll",
                "d3dcompiler_47.dll",
                "dxcompiler.dll",
                "vulkan-1.dll",
                "vk_swiftshader.dll",
            }
            if cef_mode == "full":
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
            else:
                for name in loader_files:
                    src = cef / name
                    if src.is_file():
                        shutil.copy2(src, root / name)
        archive = DOWNLOADS / f"sdsc-utils-{tag}-{flavor}-{cef_mode}-portable.zip"
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
        (dest / ".qa-mode").write_text(f"{flavor}:{cef_mode}", encoding="utf-8")
        appdata = appdata_for(f"portable-{tag}")
        if appdata.is_dir():
            shutil.rmtree(appdata, ignore_errors=True)
        appdata.mkdir(parents=True, exist_ok=True)
        self.refresh_installed()
        self.log(f"install: {tag} ({flavor}, cef={cef_mode})")

    def prepare_launch(self, payload):
        self.preparing = True
        try:
            if isinstance(payload.get("manifest"), dict):
                self.set_config({"manifest": payload["manifest"]})
            if payload.get("version"):
                self.set_config({"version": str(payload["version"])})
            if payload.get("html_url"):
                self.set_config({"html_url": str(payload["html_url"])})
            target = str(payload.get("target", "direct"))
            flavor = str(payload.get("flavor", "debug"))
            cef = str(payload.get("cef", "full"))
            open_hub = bool(payload.get("open_hub", True))
            restage = bool(payload.get("restage", False))
            if flavor not in ("debug", "release"):
                raise RuntimeError(f"unknown flavor {flavor!r}")
            if cef not in ("full", "loader", "none"):
                raise RuntimeError(f"unknown CEF mode {cef!r}")
            if not (target == "portable" and cef == "full"):
                self.ensure_engine()
            if target == "portable":
                tag = str(self.manifest.get("tag_name", ""))
                marker = INSTALLS / tag / ".qa-mode"
                want = f"{flavor}:{cef}"
                if (
                    restage
                    or not marker.is_file()
                    or marker.read_text(encoding="utf-8").strip() != want
                ):
                    self._stage_install_sync(flavor, cef, tag)
                return self.launch(
                    f"installed:{tag}",
                    "hub" if open_hub and flavor == "debug" else "",
                )
            return self.launch(
                flavor, "hub" if open_hub and flavor == "debug" else ""
            )
        finally:
            self.preparing = False

    def relaunch(self):
        if not self.last_flavor:
            raise RuntimeError("nothing has been launched yet")
        return self.launch(self.last_flavor, self.last_debug_open)

    def set_config(self, payload):
        with self.lock:
            if "cef_mode" in payload:
                mode = str(payload["cef_mode"])
                if mode in ("full", "loader", "none"):
                    self.cef_mode = mode
            if isinstance(payload.get("manifest"), dict):
                manifest = dict(payload["manifest"])
                tag = normalize_tag(str(manifest.get("tag_name", "")))
                if tag:
                    manifest["tag_name"] = tag
                    self.manifest = manifest
                    self.latest = tag
                    if all(v["tag"] != tag for v in self.versions):
                        self.versions.append({"tag": tag, "enabled": True})
            if "version" in payload:
                tag = normalize_tag(str(payload["version"]))
                if tag:
                    self.manifest["tag_name"] = tag
                    self.latest = tag
                    if all(v["tag"] != tag for v in self.versions):
                        self.versions.append({"tag": tag, "enabled": True})
            if "html_url" in payload and str(payload["html_url"]).strip():
                self.manifest["html_url"] = str(payload["html_url"]).strip()
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
            self.last_flavor = flavor
            self.last_debug_open = debug_open
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
            elif path == "/api/build":
                self.console.build()
                self._json(self.console.api_state())
            elif path == "/api/stage":
                if "cef_mode" in payload and str(payload["cef_mode"]) in (
                    "full",
                    "loader",
                    "none",
                ):
                    self.console.cef_mode = str(payload["cef_mode"])
                elif "ship_cef" in payload:
                    self.console.cef_mode = "full" if bool(payload["ship_cef"]) else "none"
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
<title>SDSC Utils — app lab</title>
<style>
  :root { color-scheme: dark; --bg:#0d0f14; --panel:#161a22; --line:#2a3040;
          --ink:#e8eaf0; --muted:#8b93a7; --accent:#3d7dff; --ok:#38c172; --warn:#e0a030; }
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
  .ok { color: var(--ok); }
  .warn { color: var(--warn); }
  details { margin-top: 10px; }
  summary { cursor:pointer; color: var(--muted); }
  pre { margin:0; max-height: 220px; overflow:auto; color: var(--muted); font-size:12px; }
  .pill { display:inline-block; padding: 1px 7px; border-radius: 999px; border:1px solid var(--line); color:var(--muted); font-size: 12px; }
  .pill.ok { color: var(--ok); border-color:#234d35; }
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
    <div class="row">
      <label class="muted">The release URL must stay on https://github.com/ for the app's validator.</label>
    </div>
    <details>
      <summary>Manifest JSON (tag, URL, notes, assets)</summary>
      <div class="row"><textarea id="manifest"></textarea></div>
    </details>
    <div class="row"><button onclick="saveManifest()">Save manifest</button>
      <span class="muted" id="manifest-note"></span></div>
  </section>

  <section class="card">
    <h2>Launch</h2>
    <div class="row">
      <label>Target <select id="target" onchange="syncTarget()">
        <option value="direct">Direct app</option>
        <option value="portable">Installed portable</option>
      </select></label>
      <label>Build <select id="flavor">
        <option value="debug">Debug</option>
        <option value="release">Release (needs qa-feed)</option>
      </select></label>
      <label>CEF <select id="cef">
        <option value="full">Full bundle</option>
        <option value="loader">Loader-only (DLC test)</option>
      </select></label>
      <label><input id="open-hub" type="checkbox" checked> Open hub</label>
      <label><input id="restage" type="checkbox"> Rebuild portable</label>
    </div>
    <div class="row">
      <button class="primary" id="go" onclick="prepare()">Launch</button>
      <button id="relaunch" onclick="relaunch()" disabled>Relaunch</button>
      <button onclick="stop()">Stop</button>
      <button class="ghost" onclick="buildRelease()">Build release qa-feed</button>
    </div>
    <div class="row muted" id="run">—</div>
    <div class="row muted">Direct debug is fastest and still tests the hub/app/engine flow. Portable stages a private install; loader-only omits resources/locales so the Browser engine must be installed from the hub. Release only honors the mock update feed when built with qa-feed.</div>
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
  $('status').textContent =
    `app ${S.version} · feed ${(S.manifest && S.manifest.tag_name) || S.latest} · ` +
    (S.running ? `running ${S.running_flavor}` : 'not running') +
    (S.building ? ' · building release…' : '') +
    (S.preparing ? ' · preparing…' : '');
  if (!initialized) {
    $('version').value = (S.manifest && S.manifest.tag_name) || S.latest || '';
    $('html-url').value = (S.manifest && S.manifest.html_url) || '';
    $('manifest').value = JSON.stringify(S.manifest || {}, null, 2);
    $('cef').value = S.cef_mode || 'full';
    initialized = true;
  }
  $('relaunch').disabled = !S.last_flavor;
  syncTarget();
  $('run').textContent = S.running
    ? `running ${S.running_flavor} pid ${S.running_pid}`
    : (S.last_flavor ? `last: ${S.last_flavor}` : 'not launched yet');
  const engine = S.engine ? `DLC ${Math.round(S.engine.bytes / 1048576)} MB` : 'DLC not built';
  $('info').textContent =
    `feed ${S.feed_url || ''}\\napp ${S.version} · CEF ${S.cef ? 'found' : 'missing'} · ${engine}\\n` +
    `installs: ${(S.installed || []).map((i) => i.tag + (i.sidecar ? ' (full)' : ' (loader)')).join(', ') || '—'}`;
  $('log').textContent = (S.requests || []).map((r) => `${r.at}  ${r.line}`).join('\\n') || '—';
}
function syncTarget(){
  const portable = $('target').value === 'portable';
  $('cef').disabled = !portable;
  $('restage').disabled = !portable;
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
    await post('/api/config', {manifest: manifestValue(), cef_mode: $('cef').value});
    $('manifest-note').textContent = 'saved';
    initialized = false;
    await getState();
  } catch (e) { $('manifest-note').textContent = String(e); }
}
async function prepare(){
  const go = $('go');
  go.disabled = true;
  const old = go.textContent;
  go.textContent = 'Preparing…';
  try {
    await post('/api/prepare', {
      manifest: manifestValue(),
      target: $('target').value,
      flavor: $('flavor').value,
      cef: $('cef').value,
      open_hub: $('open-hub').checked,
      restage: $('restage').checked
    });
    initialized = false;
    await getState();
  } catch (e) {
    alert(String(e));
  } finally {
    go.disabled = false;
    go.textContent = old;
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
async function buildRelease(){
  try { await post('/api/build', {}); await getState(); }
  catch (e) { alert(String(e)); }
}
setInterval(getState, 2000);
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
