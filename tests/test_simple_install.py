"""Run the installer with local release assets and a program stand-in; never install an app."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class SimpleInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.record = self.root / 'record'
        name = 'atomics-1.2.3-aarch64-apple-darwin.tar.gz'
        archive = self.root / name
        program = b'#!/bin/sh\nif [ "$1" = --version ]; then echo "atomics 1.2.3 fixture"; else printf "%s\\n" "$*" >> "$RECORD"; fi\n'
        with tarfile.open(archive, 'w:gz') as tar:
            info = tarfile.TarInfo('ATOMICS.app/Contents/MacOS/atomics')
            info.mode = 0o755
            info.size = len(program)
            tar.addfile(info, io.BytesIO(program))
        (self.root / (name + '.sha256')).write_text(f'{hashlib.sha256(archive.read_bytes()).hexdigest()}  {name}\n')
        (self.root / (name + '.sig')).write_bytes(b'fixture')
        self.tool('uname', '#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        self.tool('curl', '#!/bin/sh\nwhile [ "$1" != -o ]; do shift; done\nout=$2; shift 2\ncp "$ASSETS/${1##*/}" "$out"\n')
        self.tool('openssl', '#!/bin/sh\nif [ "$1" = version ]; then echo "OpenSSL 3.0"; else exit 0; fi\n')
        for name in ['release-signing-key.pub', 'release-signing-key-offline.pub']:
            (self.root / name).write_text('fixture public key')
        self.env = dict(os.environ, PATH=f'{self.bin}:{os.environ["PATH"]}', ATOMICS_VERSION='1.2.3',
                        ASSETS=str(self.root), RECORD=str(self.record))

    def tool(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)

    def run_installer(self, *args):
        return subprocess.run(['sh', str(ROOT / 'scripts/install.sh'), *args], env=self.env, text=True, capture_output=True)

    def test_default_keeps_the_existing_install_only(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.record.read_text(), 'install\n')

    def test_simple_is_forwarded_only_when_requested(self):
        result = self.run_installer('--simple')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.record.read_text(), 'install --simple\n')

    def test_unknown_flags_stop_before_installing(self):
        self.assertNotEqual(self.run_installer('--unknown').returncode, 0)
        self.assertFalse(self.record.exists())


if __name__ == '__main__':
    unittest.main()
