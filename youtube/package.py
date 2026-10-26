#!/usr/bin/env python3
"""Package the cross-built YouTube add-on binary into the installer's tarball.

Produces dist/spotiya-youtube-armv7.tar.gz and dist/YOUTUBE_SHA256SUMS. The archive
layout and manifest match what src/addons.rs validates for the "youtube" add-on.
"""
from __future__ import annotations

import gzip
import hashlib
import io
import json
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSET = "spotiya-youtube-armv7.tar.gz"
PREFIX = "spotiya-youtube/youtube/"
TARGET = "armv7-unknown-linux-gnueabihf"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def arm_binary(data: bytes) -> None:
    if len(data) < 20 or data[:6] != b"\x7fELF\x01\x01" or data[18:20] != b"\x28\x00":
        raise ValueError("expected an ARMv7 little-endian ELF32 binary")


def main() -> None:
    binary_path, version = Path(sys.argv[1]), sys.argv[2]
    binary = binary_path.read_bytes()
    arm_binary(binary)

    files = {
        "bin/spotiya-youtube": binary,
        "licenses/spotiya/LICENSE": (ROOT / "LICENSE").read_bytes(),
    }
    # Third-party code adapted into or linked statically with the binary.
    for notice in sorted((ROOT / "youtube/licenses").glob("*/*")):
        files[f"licenses/{notice.parent.name}/{notice.name}"] = notice.read_bytes()
    manifest = {
        "format": 1,
        "target": TARGET,
        "spotiya_version": version,
        "files": {"bin/spotiya-youtube": sha(binary)},
    }
    files["manifest.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()

    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as tar:
        for name in sorted(files):
            data = files[name]
            info = tarfile.TarInfo(PREFIX + name)
            info.size = len(data)
            info.mode = 0o755 if name == "bin/spotiya-youtube" else 0o644
            info.mtime = 0
            tar.addfile(info, io.BytesIO(data))
    archive = gzip.compress(raw.getvalue(), mtime=0)

    dist = ROOT / "dist"
    dist.mkdir(exist_ok=True)
    (dist / ASSET).write_bytes(archive)
    (dist / "YOUTUBE_SHA256SUMS").write_text(f"{sha(archive)}  {ASSET}\n")
    print(f"{ASSET}: {len(archive)} bytes, spotiya {version}")


if __name__ == "__main__":
    main()
