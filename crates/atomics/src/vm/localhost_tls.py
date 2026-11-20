# Loaded by account.py on the verified administrator connection.
import json
import os
from pathlib import Path
import shutil
import ssl
import stat
import tempfile

TLS_LIMIT = 64 * 1024
TLS_STORE = Path("/var/lib/atomics-localhost-tls")


def tls_status(store, user):
    """Return only the public certificate of an installed, usable pair."""
    try:
        generation = (store / "current").resolve(strict=True)
        cert, key = generation / "cert.pem", generation / "key.pem"
        for path, uid, mode in ((cert, os.geteuid(), 0o644), (key, user[0], 0o600)):
            info = path.lstat()
            if (info.st_uid != uid or info.st_mode != (stat.S_IFREG | mode)
                    or info.st_size > TLS_LIMIT):
                return b""
        if key.stat().st_gid != user[1]:
            return b""
        ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER).load_cert_chain(cert, key, password="")
        return cert.read_bytes()
    except OSError:
        return b""


def tls_install(store, user, request):
    material = json.loads(request)
    try:
        store.mkdir(mode=0o755)
        store.chmod(0o755)
    except FileExistsError:
        pass
    info = store.lstat()
    if info.st_mode != (stat.S_IFDIR | 0o755) or info.st_uid != os.geteuid():
        raise PermissionError("invalid localhost TLS store permissions")

    # Stage under a directory only the administrator can change. Rename a
    # symlink to publish the whole pair, leaving the old pair on any failure.
    generation = Path(tempfile.mkdtemp(prefix="leaf-", dir=store))
    current = store / "current"
    try:
        cert, key = generation / "cert.pem", generation / "key.pem"
        cert.write_text(material["cert"], encoding="ascii")
        with os.fdopen(os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as handle:
            handle.write(material["key"])
        ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER).load_cert_chain(cert, key, password="")
        cert.chmod(0o644)
        os.chown(key, *user)
        generation.chmod(0o755)
        link = generation / "next"
        link.symlink_to(generation.name)
        link.replace(current)
    except Exception:
        shutil.rmtree(generation)
        raise
    for old in store.glob("leaf-*"):
        if old != generation:
            shutil.rmtree(old, ignore_errors=True)


def localhost_tls(peer, ordinary):
    user = ordinary.pw_uid, ordinary.pw_gid
    with peer.makefile("rb") as stream:
        data = stream.read(TLS_LIMIT + 1)
    if not data:
        peer.sendall(tls_status(TLS_STORE, user))
    elif len(data) <= TLS_LIMIT:
        tls_install(TLS_STORE, user, data)
    else:
        raise ValueError("localhost TLS payload exceeds its limit")
