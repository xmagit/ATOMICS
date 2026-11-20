"""A login keychain stand-in for the VM CLI tests. Never calls macOS security."""

from base64 import urlsafe_b64decode
import os
from pathlib import Path
import shlex
import sys


args = sys.argv[1:]
root = Path(os.environ["RECORD"])
item = root / "localhost-ca-keychain"
login = str(Path(os.environ["HOME"]) / "login keychain-db")

if args == ["-i"]:
    fields = shlex.split(sys.stdin.read())
    assert fields[0] == "add-generic-password"
    assert fields[fields.index("-s") + 1] == "atomics-localhost-ca"
    assert fields[-1] == login
    value = fields[fields.index("-w") + 1]
    assert urlsafe_b64decode(value + "=" * (-len(value) % 4)).startswith(b"-----BEGIN PRIVATE KEY-----")
    legacy = Path(os.environ["HOME"]) / "state/localhost-tls/ca/key.pem"
    assert set(root.glob("**/localhost-tls/**/key.pem")) <= {legacy}
    if os.environ["VM_MODE"] == "corrupt-keychain-write":
        value = "Y29ycnVwdA"
    item.write_text(value)
else:
    assert args[0] == "find-generic-password"
    assert args[args.index("-s") + 1] == "atomics-localhost-ca"
    assert args[-1] == login
    if not item.exists():
        sys.exit(44)
    print(item.read_text())
