# Run by the Mac through sudo on the candidate login. sshd connects to this
# socket on the Mac's behalf. Only a privileged process can bind its name in
# /run, even if an obsolete bundle sent ssh to the ordinary user's login shell.
import grp
import os
import pwd
import re
import socket
import sys
from pathlib import Path

path, user, account, operation = sys.argv[1:]
login = pwd.getpwnam(account)
if os.environ.get("SUDO_USER") != account:
    sys.exit(1)
wheel = grp.getgrnam("wheel").gr_gid
ordinary = pwd.getpwnam(user)
if (wheel in os.getgrouplist(user, ordinary.pw_gid)) != (account == user):
    sys.exit(1)

# An administrator name can change independently of hardened mode. The legacy
# broker unit pins the UID it accepts even on generations without a Mac record.
system = os.path.realpath("/run/current-system")
unit = Path(system) / "etc/systemd/system/atomics-secret-broker.service"
if unit.is_file():
    if account != user:
        match = re.search(r"^ExecStart=.*? --admin-uid (\d+)(?:\s|$)", unit.read_text(), re.MULTILINE)
        if not match or int(match[1]) != login.pw_uid:
            sys.exit(1)
elif operation == "broker":
    sys.exit(1)

with socket.socket(socket.AF_UNIX) as server:
    server.bind(path)
    try:
        os.chown(path, login.pw_uid, login.pw_gid)
        os.chmod(path, 0o600)
        server.listen(1)
        server.settimeout(20)
        print("atomics-account-ready", flush=True)
        peer, _ = server.accept()
    finally:
        os.unlink(path)

with peer:
    peer.sendall(system.encode() + b"\n")
    if operation == "broker":
        if account != user:
            os.initgroups(account, login.pw_gid)
            os.setgid(login.pw_gid)
            os.setuid(login.pw_uid)
        os.dup2(peer.fileno(), 0)
        os.execl(system + "/sw/bin/atomics", "atomics", "secret-broker-admin")
