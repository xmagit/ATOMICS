"""The release packer, with a Nix stand-in: no VM build, credentials or Nix installation."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(shutil.which('zstd'), 'zstd is required for release packaging')
class VmReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.image = self.root / 'image'
        self.bundle = self.root / 'bundle'
        self.bin = self.root / 'bin'
        for directory in (self.image, self.bundle, self.bin):
            directory.mkdir()
        with (self.image / 'atomics-vm.raw').open('wb') as disk:
            disk.write(b'disk header')
            disk.truncate(8 * 1024 * 1024)
        (self.bundle / 'install.sh').write_text("#!/bin/sh\necho '/Users/@ATOMICS_MAC_USER@'\n")
        (self.bundle / 'install.sh').chmod(0o755)
        (self.bundle / 'lima.yaml').write_text('{"cpus":4,"param":{"ATOMICS_MAC_USER":"@ATOMICS_MAC_USER@"}}')
        nix = self.bin / 'nix'
        nix.write_text('#!/bin/sh\ncase "$*" in\n *release-image*|*release-hardened-image*) echo "$FIXTURE_IMAGE";;\n'
                       ' *release-bundle*|*release-hardened-bundle*) echo "$FIXTURE_BUNDLE";;\n *) exit 1;;\nesac\n')
        nix.chmod(0o755)
        self.out = self.root / 'output with space'
        self.env = dict(os.environ, PATH=f'{self.bin}:{os.environ["PATH"]}',
                        FIXTURE_IMAGE=str(self.image), FIXTURE_BUNDLE=str(self.bundle))
        self.env.pop('GITHUB_REF_NAME', None)
        self.env.pop('GITHUB_REF_TYPE', None)

    def run_script(self, flavor='standard'):
        return subprocess.run(['bash', str(ROOT / 'scripts/release-vm-image.sh'), str(self.out), flavor],
                              env=self.env, text=True, capture_output=True)

    def test_hardened_is_a_separate_release_asset(self):
        result = self.run_script('hardened')
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest_path = next(self.out.glob('*.bundle.json'))
        self.assertIn('atomics-vm-hardened-', manifest_path.name)
        manifest = json.loads(manifest_path.read_text())
        self.assertEqual(manifest['flavor'], 'hardened')
        self.assertIn('atomics-vm-hardened-', manifest['image'])

    def test_unknown_flavor_cannot_publish_standard_assets(self):
        self.assertNotEqual(self.run_script('unknown').returncode, 0)
        self.assertFalse(list(self.out.glob('*.zst')))

    def test_packages_sparse_image_and_generated_template_with_exact_checksums(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        image = next(self.out.glob('*.raw.zst'))
        manifest_path = next(self.out.glob('*.bundle.json'))
        manifest = json.loads(manifest_path.read_text())
        self.assertEqual(manifest['image'], image.name)
        self.assertEqual(manifest['imageBytes'], 8 * 1024 * 1024)
        self.assertEqual(manifest['imageSha256'], hashlib.sha256(image.read_bytes()).hexdigest())
        self.assertEqual(manifest['files']['install.sh']['contents'], (self.bundle / 'install.sh').read_text())
        self.assertTrue(manifest['files']['install.sh']['executable'])
        self.assertFalse(manifest['files']['lima.yaml']['executable'])
        for asset in (image, manifest_path):
            self.assertEqual(Path(str(asset) + '.sha256').read_text(),
                             f'{hashlib.sha256(asset.read_bytes()).hexdigest()}  {asset.name}\n')
        restored = subprocess.check_output(['zstd', '-d', '-c', str(image)])
        self.assertEqual(restored, (self.image / 'atomics-vm.raw').read_bytes())
        self.assertLess(image.stat().st_size, 1024 * 1024)
        self.assertFalse(list(self.out.glob('*.sig')), 'only the isolated signing job holds a key')

    def test_refuses_wrong_release_tag(self):
        self.env['GITHUB_REF_NAME'] = 'v0.0.0'
        self.env['GITHUB_REF_TYPE'] = 'tag'
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse(list(self.out.glob('*.zst')))

    def test_refuses_nix_store_references_in_mac_bundle(self):
        (self.bundle / 'install.sh').write_text('#!/nix/store/example/bin/sh\n')
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Nix store reference', result.stderr)
        self.assertFalse(list(self.out.glob('*.bundle.json')))


if __name__ == '__main__':
    unittest.main()
