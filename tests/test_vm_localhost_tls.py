import importlib.util
import json
import os
from pathlib import Path
import ssl
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "crates/atomics/src/vm/localhost_tls.py"
SPEC = importlib.util.spec_from_file_location("localhost_tls", SCRIPT)
tls = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tls)


class InstallTests(unittest.TestCase):
    def test_validates_and_publishes_both_files_as_one_generation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cert, key = root / "cert.pem", root / "key.pem"
            subprocess.run(
                ["openssl", "req", "-new", "-x509", "-nodes", "-newkey", "ec",
                 "-pkeyopt", "ec_paramgen_curve:prime256v1", "-subj", "/CN=localhost",
                 "-out", cert, "-keyout", key],
                check=True, capture_output=True,
            )
            user = (os.getuid(), os.getgid())
            target = root / "store"
            material = {"cert": cert.read_text(), "key": key.read_text()}
            self.assertEqual(tls.tls_status(target, user), b"")
            tls.tls_install(target, user, json.dumps(material).encode())
            installed_cert = target / "current/cert.pem"
            installed_key = target / "current/key.pem"
            self.assertEqual(tls.tls_status(target, user), cert.read_bytes())
            self.assertEqual(installed_cert.stat().st_mode & 0o777, 0o644)
            self.assertEqual(installed_key.stat().st_mode & 0o777, 0o600)
            self.assertEqual((installed_key.stat().st_uid, installed_key.stat().st_gid), user)
            self.assertEqual(len(list(target.iterdir())), 2)

            with self.assertRaises(ssl.SSLError):
                tls.tls_install(target, user, json.dumps(dict(material, key="bad")).encode())
            self.assertEqual(tls.tls_status(target, user), cert.read_bytes())
            installed_key.chmod(0o644)
            self.assertEqual(tls.tls_status(target, user), b"")

            tls.tls_install(target, user, json.dumps(material).encode())
            self.assertEqual(tls.tls_status(target, user), cert.read_bytes())
            self.assertEqual(len(list(target.iterdir())), 2)


if __name__ == "__main__":
    unittest.main()
