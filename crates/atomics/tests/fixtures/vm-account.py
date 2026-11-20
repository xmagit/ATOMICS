"""One private SSH forward with no guest or real credentials."""
import json
import os
from pathlib import Path
import socket
import sys
import time

args = sys.argv[1:]
local, remote = args[args.index("-L") + 1].split(":")
assert remote.startswith("/run/atomics-account-")
assert "ControlMaster=no" in args and "ControlPath=none" in args
assert "/run/wrappers/bin/sudo -n" in args[-1]
record = Path(os.environ["RECORD"])
gen = record / "running"
system = gen.read_text() if gen.exists() else "/nix/store/" + "2" * 32 + "-original"
running_admin = record / "running-admin"
if running_admin.exists() and args[args.index("-l") + 1] != running_admin.read_text():
    sys.exit(1)
with socket.socket(socket.AF_UNIX) as listener:
    listener.bind(local)
    listener.listen()
    fault = os.environ.get("ACCOUNT_FAULT")
    if fault == "slow-ready":
        for char in "atomics-account-ready\n":
            print(char, end="", flush=True)
            time.sleep(0.08)
    else:
        print("atomics-account-ready", flush=True)
    conn, _ = listener.accept()
    with conn, conn.makefile("rb") as request:
        if fault == "no-generation":
            sys.exit(0)
        conn.sendall(system.encode() + b"\n")
        value = request.read()
        if fault == "stall-after-write":
            time.sleep(4)
        if args[-1].endswith(" localhost-tls"):
            cert = record / "localhost-tls-cert.pem"
            if value:
                (record / "localhost-tls-request").write_bytes(value)
                cert.write_text(json.loads(value)["cert"])
                with (record / "log").open("a") as log:
                    print("localhost-tls install as", args[args.index("-l") + 1], file=log)
            elif cert.exists():
                conn.sendall(cert.read_bytes())
            sys.exit(0)
        if value:
            (record / "argv").write_text("\n".join(args))
            (record / "stdin").write_bytes(value)
            if (record / "offline").exists():
                sys.exit(23)
