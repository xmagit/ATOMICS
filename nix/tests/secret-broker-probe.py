"""Run inside a disposable NixOS guest, including under an unrelated UID."""
import errno
import json
from pathlib import Path
import socket
import sys

operation = sys.argv[1]
if operation == "guess":
    with socket.socket(socket.AF_UNIX) as peer:
        peer.connect("/run/atomics-secret-broker/use.sock")
        peer.sendall(b'{"op":"relay"}\nGET /api.allowed.test/v1 HTTP/1.1\r\nAuthorization: Bearer atomics-' + b"0" * 64 + b"\r\n\r\n")
        status = peer.makefile("rb").readline()
        assert status.startswith(b"HTTP/1.1 401"), repr(status)
elif operation == "private":
    for path in ["/var/lib/atomics-secret-broker/rule-hosts.json", "/run/atomics-secret-broker/admin.sock"]:
        try:
            if path.endswith(".sock"):
                with socket.socket(socket.AF_UNIX) as peer:
                    peer.connect(path)
            else:
                Path(path).read_bytes()
        except PermissionError:
            pass
        else:
            raise AssertionError("private broker resource accessible")
elif operation == "forged-grant":
    with socket.socket(socket.AF_UNIX) as peer:
        try:
            peer.connect("/run/atomics-secret-broker/use.sock")
        except PermissionError:
            pass
        else:
            peer.sendall(json.dumps({"op": "grant", "name": "KEY", "session": "forged", "placeholder": "atomics-" + "0" * 64}).encode() + b"\n")
            assert not json.loads(peer.makefile("rb").readline())["ok"]
elif operation == "use-private":
    with socket.socket(socket.AF_UNIX) as peer:
        try:
            peer.connect("/run/atomics-secret-broker/use.sock")
        except PermissionError:
            pass
        else:
            raise AssertionError("broker use socket accessible to an unrelated user")
elif operation == "connect":
    allowed = sys.argv[2] == "yes"
    try:
        with socket.create_connection((sys.argv[3], int(sys.argv[4])), timeout=3):
            assert allowed, "broker reached an unlisted destination"
    except OSError as error:
        if allowed or error.errno == errno.ECONNREFUSED:
            raise
elif operation == "dns":
    assert socket.gethostbyname("api.allowed.test") == "192.168.1.2"
else:
    raise AssertionError("unknown test operation")
