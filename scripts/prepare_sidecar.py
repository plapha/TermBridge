"""Build the shared CLI for this machine and prepare Tauri's per-target sidecar."""
from __future__ import annotations
import argparse
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    target = subprocess.check_output(["rustc", "--print", "host-tuple"], text=True).strip()
    profile = "release" if args.release else "debug"
    command = ["cargo", "build", "-p", "termbridge"]
    if args.release:
        command.append("--release")
    subprocess.run(command, cwd=ROOT, check=True)
    extension = ".exe" if sys.platform == "win32" else ""
    source = ROOT / "target" / profile / f"termbridge{extension}"
    dest = ROOT / "apps" / "desktop" / "src-tauri" / "binaries" / f"termbridge-{target}{extension}"
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, dest)
    print(f"Prepared CLI sidecar for {target}: {dest}")


if __name__ == "__main__":
    main()
