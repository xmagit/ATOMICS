"""Write Lima's boot-time virtiofs rows into a small ISO for the macOS VM command fixture."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def write_boot(instance, mounts):
    instance.mkdir(parents=True, exist_ok=True)
    marker = instance / "fixture-boot-id"
    boot_id = marker.read_text() if marker.exists() else "iid-first"
    with tempfile.TemporaryDirectory() as scratch:
        root = Path(scratch)
        (root / "lima.env").write_text(
            f"LIMA_CIDATA_IID={boot_id}\nLIMA_CIDATA_MOUNTS={len(mounts)}\nLIMA_CIDATA_MOUNTTYPE=virtiofs\n"
        )
        rows = ["#cloud-config", "mounts:", '- [vz-rosetta, /mnt/lima-rosetta, virtiofs, defaults, "0", "0"]']
        for mount in mounts:
            host = mount["location"]
            guest = mount.get("mountPoint", host)
            tag = hashlib.sha256(f"{host}:{guest}".encode()).hexdigest()[:16]
            mode = "rw" if mount.get("writable") else "ro"
            rows.append(f'- [lima-{tag}, {guest}, virtiofs, "{mode}", "0", "0"]')
        (root / "user-data").write_text("\n".join(rows) + "\n")
        subprocess.run(["/usr/bin/tar", "--format", "iso9660", "-cf", instance / "cidata.iso",
                        "-C", root, "lima.env", "user-data"], check=True)


if __name__ == "__main__":
    write_boot(Path(sys.argv[1]), json.loads(sys.argv[2]))
