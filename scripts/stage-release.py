"""Check each producer's release files, then move them into a fresh directory for signing."""

import hashlib
from pathlib import Path
import stat
import sys


def stage_release(version, unsigned, staged):
    producers = {
        "atomics-web": [f"atomics-web-{version}.tar.gz"],
        "atomics-unsigned-vm-standard": [f"atomics-vm-{version}-aarch64-linux.{suffix}"
                                       for suffix in ("raw.zst", "bundle.json")],
    }
    producers["atomics-unsigned-vm-hardened"] = [f"atomics-vm-hardened-{version}-aarch64-linux.{suffix}"
                                                for suffix in ("raw.zst", "bundle.json")]
    for target in ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin"):
        producers[f"atomics-unsigned-{target}"] = [f"atomics-{version}-{target}.tar.gz"]

    if {path.name for path in unsigned.iterdir()} != producers.keys():
        raise SystemExit(f"Expected producer directories: {', '.join(producers)}")
    files = {}
    for producer, archives in producers.items():
        directory = unsigned / producer
        if directory.is_symlink() or not directory.is_dir():
            raise SystemExit(f"Expected a directory for {producer}")
        expected = {name + suffix for name in archives for suffix in ("", ".sha256")}
        actual = {path.name for path in directory.iterdir()}
        if expected != actual:
            raise SystemExit(f"{producer}: missing {sorted(expected - actual)}, unexpected {sorted(actual - expected)}")
        for name in sorted(expected):
            path = directory / name
            if name in files:
                raise SystemExit(f"Both {files[name].parent.name} and {producer} supply {name}")
            metadata = path.lstat()
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise SystemExit(f"Expected a regular file: {producer}/{name}")
            files[name] = path
        for name in archives:
            with files[name].open("rb") as archive:
                digest = hashlib.file_digest(archive, "sha256").hexdigest()
            line = f"{digest}  {name}\n".encode()
            with files[name + ".sha256"].open("rb") as sidecar:
                if sidecar.read(len(line) + 1) != line:
                    raise SystemExit(f"Incorrect checksum for {producer}/{name}")

    staged.mkdir()
    for name, path in files.items():
        path.rename(staged / name)


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit("Usage: python3 scripts/stage-release.py VERSION UNSIGNED_DIR STAGING_DIR")
    stage_release(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]))
