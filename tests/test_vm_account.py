"""Exercise the guest account helper with an isolated socket and a simulated NixOS user database."""
import os
import runpy
import socket
import sys
import tempfile
import threading
import unittest
from contextlib import ExitStack
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

HELPER = Path(__file__).resolve().parents[1] / "crates/atomics/src/vm/account.py"
GENERATION = "/nix/store/" + "1" * 32 + "-running"


class GuestAccountTests(unittest.TestCase):
    def run_helper(self, login, *, ordinary_wheel=False, sudo_user=None, admin_uid=1001, broker=True):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory, ExitStack() as scope:
            path = str(Path(directory) / "account")
            scope.enter_context(patch.object(sys, "argv", ["account", path, "worker", login, "broker" if broker else "probe"]))
            scope.enter_context(patch.dict(os.environ, SUDO_USER=sudo_user or login))
            accounts = {"worker": SimpleNamespace(pw_uid=1000, pw_gid=100), "admin": SimpleNamespace(pw_uid=1001, pw_gid=100)}
            scope.enter_context(patch("pwd.getpwnam", side_effect=accounts.__getitem__))
            scope.enter_context(patch("grp.getgrnam", return_value=SimpleNamespace(gr_gid=1)))
            scope.enter_context(patch("os.getgrouplist", return_value=[100, 1] if ordinary_wheel else [100]))
            scope.enter_context(patch("os.path.realpath", return_value=GENERATION))
            scope.enter_context(patch.object(Path, "is_file", return_value=admin_uid is not None))
            scope.enter_context(patch.object(Path, "read_text", return_value=f"ExecStart=/nix/store/test/bin/atomics secret-broker --admin-uid {admin_uid}\n"))
            scope.enter_context(patch("os.chown"))
            switches = [scope.enter_context(patch("os." + name)) for name in ("initgroups", "setgid", "setuid")]
            # Keep the runner's stdin. Read from the descriptor the helper gave its broker.
            dup = scope.enter_context(patch("os.dup2"))
            def accept_mutation(program, name, command):
                self.assertEqual((program, name, command), (GENERATION + "/sw/bin/atomics", "atomics", "secret-broker-admin"))
                fd, dest = dup.call_args.args
                self.assertEqual(dest, 0)
                self.assertEqual(os.read(fd, 128), b"test mutation")
            execute = scope.enter_context(patch("os.execl", side_effect=accept_mutation))

            # Connect only when the production helper has checked the running account.
            answer = []
            def connect():
                with socket.socket(socket.AF_UNIX) as peer:
                    peer.settimeout(3)
                    peer.connect(path)
                    answer.append(peer.makefile("rb").readline())
                    if broker:
                        peer.sendall(b"test mutation")
            reader = threading.Thread(target=connect)
            def ready(*args, **kwargs):
                self.assertEqual(args, ("atomics-account-ready",))
                self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
                reader.start()
            scope.enter_context(patch("builtins.print", side_effect=ready))
            runpy.run_path(str(HELPER), run_name="__main__")
            reader.join(3)
            self.assertEqual(answer, [GENERATION.encode() + b"\n"])
            self.assertFalse(Path(path).exists())
            self.assertEqual(execute.called, broker)
            for switch in switches:
                self.assertEqual(switch.called, broker and login != "worker")

    def test_running_admin_and_ordinary_sudo_routes(self):
        self.run_helper("admin")
        self.run_helper("worker", ordinary_wheel=True)
        self.run_helper("admin", broker=False, admin_uid=None)

    def test_staged_roles_and_unverified_broker_are_refused(self):
        for login, conditions in [
            ("admin", {"sudo_user": "worker"}), ("admin", {"ordinary_wheel": True}),
            ("worker", {}), ("admin", {"admin_uid": 1002}), ("admin", {"admin_uid": None}),
        ]:
            with self.subTest(login=login, conditions=conditions), self.assertRaises(SystemExit):
                self.run_helper(login, **conditions)


if __name__ == "__main__":
    unittest.main()
