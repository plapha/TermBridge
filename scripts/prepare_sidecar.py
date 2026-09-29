"""Build the shared CLI and prepare Tauri's per-target sidecar.

Default target is this machine. `--target universal-apple-darwin` builds both macOS
architectures and also writes the lipo'd universal binary: tauri-build checks the
per-arch sidecars while compiling each arch, and the bundler uses the universal one.
"""
from __future__ import annotations
import argparse
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
BINARIES = ROOT / "apps" / "desktop" / "src-tauri" / "binaries"
UNIVERSAL = "universal-apple-darwin"
MAC_ARCHES = ["aarch64-apple-darwin", "x86_64-apple-darwin"]


def build(target: str | None, release: bool) -> Path:
    command = ["cargo", "build", "-p", "termbridge"]
    if release:
        command.append("--release")
    if target:
        command += ["--target", target]
    subprocess.run(command, cwd=ROOT, check=True)
    profile = "release" if release else "debug"
    out = ROOT / "target" / target / profile if target else ROOT / "target" / profile
    extension = ".exe" if sys.platform == "win32" else ""
    return out / f"termbridge{extension}"


def install(source: Path, triple: str) -> None:
    extension = ".exe" if sys.platform == "win32" else ""
    dest = BINARIES / f"termbridge-{triple}{extension}"
    BINARIES.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, dest)
    print(f"Prepared CLI sidecar for {triple}: {dest}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--target", help="Rust target triple, or universal-apple-darwin")
    args = parser.parse_args()
    if args.target == UNIVERSAL:
        built = []
        for arch in MAC_ARCHES:
            path = build(arch, args.release)
            install(path, arch)
            built.append(str(path))
        BINARIES.mkdir(parents=True, exist_ok=True)
        dest = BINARIES / f"termbridge-{UNIVERSAL}"
        subprocess.run(["lipo", "-create", "-output", str(dest), *built], check=True)
        print(f"Prepared CLI sidecar for {UNIVERSAL}: {dest}")
        return
    host = subprocess.check_output(["rustc", "--print", "host-tuple"], text=True).strip()
    target = args.target or host
    install(build(args.target, args.release), target)


if __name__ == "__main__":
    main()
