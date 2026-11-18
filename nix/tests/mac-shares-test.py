"""Exercise the boot script against a scratch tree: cidata parsing, the root-owned layout, mounts,
links, the generated guest-check environment, and refusals of links and non-folders anywhere on the
paths root works in. Only mount and mountpoint are stand-ins; every other change is real, so a test
sees exactly what root would have done. Ownership changes between users need root: the NixOS check
nix/tests/mac-shares-boot.nix covers those."""
import grp
import os
from pathlib import Path
import pwd
import shlex
import socket
import stat
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(sys.argv.pop(1)).resolve()
USER = pwd.getpwuid(os.getuid()).pw_name
GROUP = grp.getgrgid(os.getgid()).gr_name


class MacSharesTests(unittest.TestCase):
    def test_attachment_exchange_is_only_the_writable_exchange_mount(self):
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn('ATOMICS_EXCHANGE=\n', self.output.read_text())
        for mode, expected in [('ro', ''), ('rw', '/Users/alice/atomics-exchange')]:
            self.mounts(extra=f'- [mount-tag-3, /Users/alice/atomics-exchange, virtiofs, "{mode},nosuid,nodev", "0", "0"]\n')
            self.assertEqual(self.run_script().returncode, 0)
            self.assertIn(f'ATOMICS_EXCHANGE={expected}\n', self.output.read_text())

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fs = self.root / 'fs'
        self.fs.mkdir()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.log = self.root / 'calls'
        self.output = self.root / 'desktop.env'
        self.cidata = self.root / 'cidata'
        self.cidata.mkdir()
        for command in ('mount', 'mountpoint'):
            stub = self.bin / command
            stub.write_text('#!/bin/sh\nprintf "%s\\n" "$(basename "$0") $*" >> "$CALLS"\n'
                            + ('exit "${MOUNTED:-1}"\n' if command == 'mountpoint' else 'exit 0\n'))
            stub.chmod(0o755)
        self.settings = {
            'dynamic': 'true', 'cidata': str(self.cidata), 'mac_home': '/Users/@ATOMICS_MAC_USER@',
            'socket': '/Users/@ATOMICS_MAC_USER@/.atomics/atomics.sock',
            'forward': '/Users/@ATOMICS_MAC_USER@/.atomics/forward/atomics.sock',
            'executables': '/Users/@ATOMICS_MAC_USER@/Applications/ATOMICS.app/Contents/MacOS/atomics '
                           '/Applications/ATOMICS.app/Contents/MacOS/atomics',
            'shares': '', 'owner': USER, 'group': GROUP, 'guest_home': '/home/atomics',
            'binary': '/run/current-system/sw/bin/atomics', 'host_port': '47800', 'output': str(self.output),
            'noexec': 'false', 'fsroot': str(self.fs), 'root_user': USER, 'root_group': GROUP,
        }
        self.params('alice')
        self.mounts()

    def params(self, user):
        (self.cidata / 'param.env').write_text(f'PARAM_UNRELATED=ignored\nPARAM_ATOMICS_MAC_USER={user}\n')

    def mounts(self, user='alice', extra=''):
        (self.cidata / 'user-data').write_text(
            '#cloud-config\nmounts:\n'
            '- [vz-rosetta, /mnt/lima-rosetta, virtiofs, defaults, "0", "0"]\n'
            f'- [mount-tag-1, /Users/{user}/repos, virtiofs, "rw,nosuid,nodev", "0", "0"]\n'
            f'- [mount-tag-2, /Users/{user}/.atomics/briefs, virtiofs, "ro,nosuid,nodev", "0", "0"]\n'
            + extra)

    def run_script(self, **env):
        config = self.root / 'config'
        config.write_text('\n'.join(f'{key}={shlex.quote(value)}' for key, value in self.settings.items()))
        return subprocess.run(['bash', str(SCRIPT), str(config)], text=True, capture_output=True,
                              env=dict(os.environ, PATH=f'{self.bin}:{os.environ["PATH"]}', CALLS=str(self.log), **env))

    def path(self, guest_path):
        return self.fs / guest_path.lstrip('/')

    def mode(self, guest_path):
        return stat.S_IMODE(os.lstat(self.path(guest_path)).st_mode)

    def calls(self):
        return self.log.read_text() if self.log.exists() else ''

    def snapshot(self):
        """Every entry under the scratch tree with its type, mode and link target."""
        entries = {}
        for directory, folders, files in os.walk(self.fs):
            for name in folders + files:
                full = Path(directory, name)
                info = os.lstat(full)
                entries[str(full.relative_to(self.fs))] = (
                    stat.S_IFMT(info.st_mode), stat.S_IMODE(info.st_mode),
                    os.readlink(full) if full.is_symlink() else None)
        return entries

    def test_layout_is_roots_with_the_socket_folder_the_users_own(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        for folder in ('/Users', '/Users/alice', '/Users/alice/.atomics', '/Users/alice/Applications',
                       '/Users/alice/Applications/ATOMICS.app/Contents/MacOS', '/Applications/ATOMICS.app/Contents/MacOS',
                       '/Users/alice/repos', '/Users/alice/.atomics/briefs'):
            self.assertEqual(self.mode(folder), 0o755, folder)
        self.assertEqual(self.mode('/Users/alice/.atomics/forward'), 0o700)
        self.assertEqual(os.readlink(self.path('/Users/alice/.atomics/atomics.sock')), 'forward/atomics.sock')
        for exe in ('/Users/alice/Applications/ATOMICS.app/Contents/MacOS/atomics',
                    '/Applications/ATOMICS.app/Contents/MacOS/atomics'):
            self.assertEqual(os.readlink(self.path(exe)), '/run/current-system/sw/bin/atomics')
        log = self.calls()
        self.assertIn(f'mount -t virtiofs -o rw,nosuid,nodev mount-tag-1 {self.path("/Users/alice/repos")}', log)
        self.assertIn(f'mount -t virtiofs -o ro,nosuid,nodev mount-tag-2 {self.path("/Users/alice/.atomics/briefs")}', log)
        self.assertNotIn('vz-rosetta', log)
        env = self.output.read_text()
        self.assertIn('ATOMICS_MAC_SOCKET=/Users/alice/.atomics/atomics.sock\n', env)
        self.assertIn('ATOMICS_MAC_SOCKET_FOLDER=/Users/alice/.atomics/forward\n', env)
        self.assertIn('ATOMICS_HOST_FOLDER=/Users/alice/repos', env)
        self.assertNotIn('@ATOMICS_', env)

    def test_hardened_shares_mount_noexec(self):
        self.settings['noexec'] = 'true'
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn('-o rw,nosuid,nodev,noexec mount-tag-1', self.calls())
        self.assertIn('-o ro,nosuid,nodev,noexec mount-tag-2', self.calls())

    def test_running_again_changes_nothing_and_keeps_existing_mounts(self):
        self.assertEqual(self.run_script().returncode, 0)
        before = self.snapshot()
        self.log.unlink()
        self.assertEqual(self.run_script(MOUNTED='0').returncode, 0)
        self.assertNotIn('mount -t', self.calls())
        self.assertEqual(self.snapshot(), before)

    def test_shares_mounted_before_take_this_runs_options(self):
        # A switch to hardened mode without a reboot: the shares are mounted already, and noexec still applies.
        self.settings['noexec'] = 'true'
        self.assertEqual(self.run_script(MOUNTED='0').returncode, 0)
        self.assertIn(f'mount -o remount,rw,nosuid,nodev,noexec {self.path("/Users/alice/repos")}', self.calls())
        self.assertIn(f'mount -o remount,ro,nosuid,nodev,noexec {self.path("/Users/alice/.atomics/briefs")}', self.calls())

    def test_an_owner_cannot_keep_a_loose_mode_through_the_takeover(self):
        # The old owner acts right after root's first change. Root takes the owner first, so in the VM that
        # chmod fails; here (one user) it succeeds, and the mode root sets next must still be what remains.
        chown = self.bin / 'chown'
        chown.write_text('#!/bin/sh\nPATH=${PATH#*:} chown "$@" || exit\n'
                         'for last; do :; done\nchmod 0777 -- "$last"\n')
        chown.chmod(0o755)
        self.assertEqual(self.run_script().returncode, 0)
        os.chmod(self.path('/Users/alice/Applications'), 0o777)
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        for folder in ('/Users/alice', '/Users/alice/.atomics', '/Users/alice/Applications',
                       '/Users/alice/Applications/ATOMICS.app/Contents/MacOS'):
            self.assertEqual(self.mode(folder), 0o755, folder)
        self.assertEqual(self.mode('/Users/alice/.atomics/forward'), 0o700)

    def test_a_takeover_that_does_not_hold_stops_the_script(self):
        # Whatever happens between root's changes, the folder is checked after them.
        chmod = self.bin / 'chmod'
        chmod.write_text('#!/bin/sh\nPATH=${PATH#*:} chmod "$@" || exit\n'
                         'for last; do :; done\n'
                         'case $last in */Applications) PATH=${PATH#*:} chmod 0777 -- "$last";; esac\n')
        chmod.chmod(0o755)
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('/Users/alice/Applications did not become', result.stderr)
        self.assertFalse(self.output.exists())

    def test_checks_a_legacy_child_link_only_after_securing_its_parent(self):
        parent = self.path('/Users/alice')
        parent.mkdir(parents=True)
        parent.chmod(0o777)
        (parent / '.atomics').symlink_to('missing')
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('symbolic link', result.stderr)
        self.assertEqual(self.mode('/Users/alice'), 0o755)
        self.assertFalse(self.output.exists())

    def test_existing_socket_link_is_classified_without_testing_its_referent(self):
        self.assertEqual(self.run_script().returncode, 0)
        link = self.path('/Users/alice/.atomics/atomics.sock')
        guard = self.root / 'guard.sh'
        guard.write_text("""function [ {
            if [[ $2 == "$SOCKET_LINK" && $1 != -L ]]; then
                echo "dereferenced socket link with $1" >&2
                exit 1
            fi
            builtin [ "$@"
        }
""")
        result = self.run_script(BASH_ENV=str(guard), SOCKET_LINK=str(link))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_loose_modes_from_an_earlier_layout_are_tightened(self):
        self.assertEqual(self.run_script().returncode, 0)
        for folder, mode in (('/Users/alice', 0o777), ('/Users/alice/.atomics', 0o700), ('/Users/alice/.atomics/forward', 0o755)):
            os.chmod(self.path(folder), mode)
        try:
            os.chmod(self.path('/Users/alice'), 0o2775)
        except PermissionError:
            # Nix's build sandbox refuses setgid bits; the plain loose mode is still tightened.
            os.chmod(self.path('/Users/alice'), 0o775)
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(self.mode('/Users/alice'), 0o755)
        self.assertEqual(self.mode('/Users/alice/.atomics'), 0o755)
        self.assertEqual(self.mode('/Users/alice/.atomics/forward'), 0o700)

    def test_a_stale_socket_at_the_socket_path_becomes_the_link(self):
        self.path('/Users/alice/.atomics').mkdir(parents=True)
        stale = socket.socket(socket.AF_UNIX)
        stale.bind(str(self.path('/Users/alice/.atomics/atomics.sock')))
        stale.close()
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(os.readlink(self.path('/Users/alice/.atomics/atomics.sock')), 'forward/atomics.sock')

    def test_a_link_anywhere_on_the_paths_fails_and_changes_nothing_in_its_target(self):
        for component in ('/Users/alice/.atomics', '/Users/alice/Applications', '/Users/alice/.atomics/briefs',
                          '/Users/alice/repos', '/Users/alice', '/Users/alice/.atomics/forward',
                          '/Applications/ATOMICS.app'):
            for earlier_boot in (True, False):
                with self.subTest(component=component, earlier_boot=earlier_boot):
                    self.setUp()
                    if earlier_boot:
                        self.assertEqual(self.run_script().returncode, 0)
                    decoy = self.root / 'decoy'
                    decoy.mkdir()
                    (decoy / 'marker').write_text('kept\n')
                    os.chmod(decoy, 0o750)
                    target = self.path(component)
                    if target.exists():
                        target.rename(self.root / 'moved-away')
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.symlink_to(decoy)
                    before = self.snapshot()
                    self.log.unlink(missing_ok=True)
                    result = self.run_script()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('symbolic link', result.stderr)
                    self.assertEqual(stat.S_IMODE(decoy.stat().st_mode), 0o750)
                    self.assertEqual(sorted(p.name for p in decoy.iterdir()), ['marker'])
                    self.assertNotIn(str(decoy), self.calls())
                    self.assertNotIn('mount -t', self.calls())
                    self.assertTrue(target.is_symlink())
                    self.assertEqual(self.snapshot(), before)

    def test_a_file_where_a_folder_belongs_fails_and_stays(self):
        for component in ('/Users/alice/.atomics', '/Users/alice/repos', '/Users/alice/Applications'):
            with self.subTest(component=component):
                self.setUp()
                target = self.path(component)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text('a file\n')
                result = self.run_script()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('not a folder', result.stderr)
                self.assertEqual(target.read_text(), 'a file\n')
                self.assertNotIn('mount -t', self.calls())

    def test_a_folder_at_the_socket_or_executable_path_is_refused(self):
        self.path('/Users/alice/.atomics/atomics.sock').mkdir(parents=True)
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('neither a link nor a stale socket', result.stderr)
        self.assertFalse(self.path('/Users/alice/.atomics/forward').exists())

    def test_custom_share_mount_point_comes_from_cidata(self):
        self.mounts(extra='- [external, /Volumes/work, virtiofs, "rw", "0", "0"]\n')
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn(f'external {self.path("/Volumes/work")}', self.calls())
        self.assertIn('/Volumes/work:rw', self.output.read_text())
        self.assertEqual(self.mode('/Volumes'), 0o755)

    def test_build_time_identity_works_without_param_env(self):
        self.settings = {key: value.replace('@ATOMICS_MAC_USER@', 'alice') for key, value in self.settings.items()}
        self.settings.update(dynamic='false', shares='/Users/alice/repos:rw /Users/alice/.atomics/briefs:ro')
        (self.cidata / 'param.env').unlink()
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn('/Users/alice/repos', self.output.read_text())

    def test_missing_or_malicious_identity_is_never_evaluated(self):
        for value in ('', '../alice', 'alice/other', '$(touch /tmp/never-run)', 'alice\nPARAM_ATOMICS_MAC_USER=alice'):
            self.params(value)
            self.assertNotEqual(self.run_script().returncode, 0, value)
            self.assertEqual(self.calls(), '')
            self.assertEqual(list(self.fs.iterdir()), [])
        (self.cidata / 'param.env').unlink()
        self.assertNotEqual(self.run_script().returncode, 0)

    def test_bad_mount_paths_fail_before_any_mutation(self):
        for path in ('/etc', '/Users/alice/../root', '/Users/alice/repos;false', '/Users/alice', '/Users/alice/repos/inner',
                     '/Users/alice//repos', '/Users/alice/.atomics/.', '/Users/alice/.atomics/forward',
                     '/Users/alice/.atomics/forward/inner'):
            with self.subTest(path=path):
                self.mounts(extra=f'- [bad, {path}, virtiofs, "rw", "0", "0"]\n')
                self.assertNotEqual(self.run_script().returncode, 0, path)
                self.assertEqual(self.calls(), '')
                self.assertEqual(list(self.fs.iterdir()), [])

    def test_paths_outside_the_mac_trees_are_refused(self):
        for key, value in (('executables', '/etc/atomics'), ('executables', '/home/atomics/bin/atomics'),
                           ('mac_home', '/home/@ATOMICS_MAC_USER@')):
            with self.subTest(key=key, value=value):
                self.setUp()
                self.settings[key] = value
                if key == 'mac_home':
                    self.settings.update(socket='/home/@ATOMICS_MAC_USER@/.atomics/atomics.sock',
                                         forward='/home/@ATOMICS_MAC_USER@/.atomics/forward/atomics.sock')
                    self.mounts(extra='')
                self.assertNotEqual(self.run_script().returncode, 0)
                self.assertEqual(list(self.fs.iterdir()), [])

    def test_the_socket_folder_must_sit_beside_the_socket(self):
        for forward in ('/Users/@ATOMICS_MAC_USER@/.atomics/atomics.sock', '/Users/@ATOMICS_MAC_USER@/elsewhere/atomics.sock',
                        '/Users/@ATOMICS_MAC_USER@/.atomics/a/b/atomics.sock'):
            with self.subTest(forward=forward):
                self.settings['forward'] = forward
                self.assertNotEqual(self.run_script().returncode, 0)
                self.assertEqual(list(self.fs.iterdir()), [])

    def test_links_inside_shares_and_missing_mount_tags_are_refused(self):
        for exe in ('/Users/@ATOMICS_MAC_USER@/repos/atomics', '/Users/@ATOMICS_MAC_USER@/.atomics/forward/atomics'):
            with self.subTest(exe=exe):
                self.settings['executables'] = exe
                self.assertNotEqual(self.run_script().returncode, 0)
                self.assertEqual(self.calls(), '')
                self.assertEqual(list(self.fs.iterdir()), [])
        self.settings.update(dynamic='false', mac_home='/Users/alice', socket='/Users/alice/.atomics/atomics.sock',
                             forward='/Users/alice/.atomics/forward/atomics.sock',
                             executables='/Applications/ATOMICS.app/Contents/MacOS/atomics', shares='/Users/alice/missing:rw')
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse(self.output.exists())


unittest.main()
