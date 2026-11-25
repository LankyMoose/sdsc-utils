#!/usr/bin/env python3
"""Install the local Browser add-on next to the app data.

Copies `sdsc-browser` into `<data-dir>/browser-addon/` and writes `addon.json`.
This is the local-install step before MSIX/Store packaging.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import time
from pathlib import Path


def default_data_dir() -> Path:
    override = os.environ.get("SDSC_DATA_DIR")
    if override:
        return Path(override)
    if os.name == "nt" and os.environ.get("APPDATA"):
        return Path(os.environ["APPDATA"]) / "sdsc-utils"
    return Path.home() / ".local" / "share" / "sdsc-utils"


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=None)
    parser.add_argument("--data-dir", type=Path, default=None)
    parser.add_argument("--version", default=None)
    parser.add_argument("--remove", action="store_true")
    args = parser.parse_args()
    data_dir = args.data_dir or default_data_dir()
    target_dir = data_dir / "browser-addon"
    exe_name = "sdsc-browser.exe" if os.name == "nt" else "sdsc-browser"
    target = target_dir / exe_name

    if args.remove:
        if target_dir.exists():
            shutil.rmtree(target_dir)
            print(f"removed {target_dir}")
        else:
            print(f"nothing to remove at {target_dir}")
        return 0

    source = args.source
    if source is None:
        candidates = [
            root / "target" / "release" / exe_name,
            root / "target" / "debug" / exe_name,
        ]
        source = next((path for path in candidates if path.is_file()), candidates[0])
    if not source.is_file():
        print(f"error: add-on executable not found: {source}", file=sys.stderr)
        return 2
    target_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)
    version = (
        args.version
        or os.environ.get("SDSC_BROWSER_ADDON_VERSION")
        or "dev"
    )
    meta = {
        "version": version,
        "source": str(source),
        "installed_at": int(time.time()),
        "size": target.stat().st_size,
        "source_modified": int(source.stat().st_mtime),
    }
    (target_dir / "addon.json").write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    print(f"installed {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
