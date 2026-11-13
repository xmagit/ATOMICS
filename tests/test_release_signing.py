"""Release signatures must authenticate the archive with the installed trust anchor."""

import hashlib
import os
from pathlib import Path
import pty
import re
import shutil
import subprocess
import tempfile
import termios
import threading
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]


class ReleaseSigningTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="atomics-sign-test-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.private = self.directory / "private.pem"
        self.public = self.directory / "public.pem"
        self.archive = self.directory / "archive with spaces.tar.gz"
        self.archive.write_bytes(b"archive bytes to authenticate")
        self.runner_temp = self.directory / "runner-temp"
        self.runner_temp.mkdir()
        self.generate_key(self.private, self.public)

    def generate_key(self, private, public, algorithm="ED25519"):
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", algorithm, "-out", str(private)],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["openssl", "pkey", "-in", str(private), "-pubout", "-out", str(public)],
            check=True, capture_output=True,
        )

    def sign(self, key=None, public=None):
        env = dict(os.environ, RUNNER_TEMP=str(self.runner_temp))
        env.pop("OPENSSL", None)
        env["ATOMICS_RELEASE_SIGNING_KEY"] = self.private.read_text() if key is None else key
        result = subprocess.run(
            ["sh", str(ROOT / "scripts/sign-release.sh"), str(self.archive),
             str(self.public if public is None else public)],
            env=env, capture_output=True, text=True,
        )
        if env["ATOMICS_RELEASE_SIGNING_KEY"]:
            self.assertTrue(
                env["ATOMICS_RELEASE_SIGNING_KEY"] not in result.stdout + result.stderr,
                "signer exposed the private key",
            )
        self.assertEqual(list(self.runner_temp.iterdir()), [], "private-key staging must be removed")
        return result

    def verify(self):
        return subprocess.run(
            ["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(self.public),
             "-in", str(self.archive), "-sigfile", str(self.archive) + ".sig"],
            capture_output=True,
        )

    def test_signature_verifies_the_archive_and_rejects_tampering(self):
        result = self.sign()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(str(self.archive) + ".sig").stat().st_size, 64)
        self.assertEqual(self.verify().returncode, 0)
        self.archive.write_bytes(self.archive.read_bytes() + b"tampered")
        self.assertNotEqual(self.verify().returncode, 0)

    def test_mismatched_key_does_not_publish_a_signature(self):
        other_private = self.directory / "other-private.pem"
        self.generate_key(other_private, self.directory / "other-public.pem")
        self.assertNotEqual(self.sign(key=other_private.read_text()).returncode, 0)
        self.assertFalse(Path(str(self.archive) + ".sig").exists())

    def test_missing_secret_has_a_clear_error(self):
        result = self.sign(key="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ATOMICS_RELEASE_SIGNING_KEY is required", result.stderr)

    def test_missing_public_key_fails_closed(self):
        result = self.sign(public=self.directory / "missing.pem")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Release public key missing", result.stderr)

    def test_other_key_algorithms_are_rejected(self):
        self.generate_key(self.directory / "x25519-private.pem", self.public, "X25519")
        result = self.sign()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be Ed25519", result.stderr)

    def test_malformed_secret_does_not_publish_a_signature(self):
        result = self.sign(key="invalid-private-key")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(Path(str(self.archive) + ".sig").exists())


class ReleaseWorkflowTests(unittest.TestCase):
    """What keeps the signing key away from the build. Text, not YAML: no parser is installed where this runs."""

    @classmethod
    def setUpClass(cls):
        cls.text = (ROOT / ".github/workflows/web.yml").read_text()
        cls.jobs = {}
        name = None
        for line in cls.text[cls.text.index("\njobs:\n"):].splitlines()[2:]:
            match = re.match(r"^  ([a-z-]+):$", line)
            if match:
                name = match.group(1)
                cls.jobs[name] = []
            elif name:
                cls.jobs[name].append(line)
        cls.jobs = {name: "\n".join(lines) for name, lines in cls.jobs.items()}

    def test_only_the_sign_job_sees_a_secret_and_it_waits_for_a_person(self):
        for name, job in self.jobs.items():
            self.assertEqual("secrets." in job, name == "sign", name)
        self.assertIn("\n    environment: release\n", self.jobs["sign"])
        self.assertEqual(self.text.count("secrets."), 1)

    def test_the_sign_job_builds_nothing_and_caches_nothing(self):
        sign = self.jobs["sign"]
        code = "\n".join(line for line in sign.splitlines() if not line.lstrip().startswith("#"))
        for word in ("cargo", "rust-cache", "rust-toolchain", "rustc", "actions/cache", "apt-get", "brew"):
            self.assertNotIn(word, code, word)
        self.assertEqual(re.findall(r"needs: (.*)", sign), ["[preflight, desktop, vm-image]"])
        self.assertIn("contents: read", sign)
        self.assertNotIn("contents: write", sign)

    def test_every_action_in_both_workflows_is_pinned_to_a_commit(self):
        for path in ("web.yml", "ci.yml"):
            text = (ROOT / ".github/workflows" / path).read_text()
            actions = re.findall(r"uses: (\S+)", text)
            self.assertGreaterEqual(len(actions), 3)
            for action in actions:
                self.assertRegex(action, r"^[^@]+@[0-9a-f]{40}$", path)

    def test_preflight_must_pass_before_signing_approval_or_release_upload(self):
        preflight = self.jobs["preflight"]
        self.assertIn("if: startsWith(github.ref, 'refs/tags/')", preflight)
        self.assertIn("fetch-depth: 0", preflight)
        self.assertIn("actions: read", preflight)
        self.assertNotIn("environment:", preflight)
        self.assertNotIn("secrets.", preflight)
        for name in ("sign", "release"):
            needs = re.search(r"needs: \[([^]]+)\]", self.jobs[name]).group(1).split(", ")
            self.assertIn("preflight", needs, name)
            self.assertNotIn("always()", self.jobs[name])

    def test_the_key_reaches_one_step_and_the_checkout_keeps_no_credentials(self):
        sign = self.jobs["sign"]
        steps = sign.split("\n      - ")
        holding = [step for step in steps if "secrets." in step]
        self.assertEqual(len(holding), 1)
        self.assertIn("sign-release.sh", holding[0])
        self.assertIn("persist-credentials: false", sign)
        self.assertNotIn("secrets.", sign[:sign.index("name: Sign and verify each archive")])

    def test_it_signs_each_platform_a_build_makes_and_every_release_needs_both_keys(self):
        built = set(re.findall(r"target: (\S+-\S+)", self.jobs["desktop"]))
        self.assertEqual(len(built), 2)
        for target in built:
            self.assertEqual(self.jobs["sign"].count(target), 2, target)
        for key in ("release-signing-key.pub", "release-signing-key-offline.pub"):
            self.assertIn(key, self.jobs["desktop"])

    def test_a_release_is_the_latest_only_once_it_has_signed_builds(self):
        self.assertIn("--prerelease", self.jobs["release"])
        self.assertIn("--prerelease=false --latest", self.jobs["desktop-release"])
        self.assertIn("needs: [sign, release]", self.jobs["desktop-release"])
        self.assertIn("atomics-signatures", self.jobs["desktop-release"])

    def step_script(self, name, job="sign"):
        """A step's own lines from the workflow, as the runner would hand them to bash."""
        lines = self.jobs[job].splitlines()
        start = next(index for index, line in enumerate(lines) if f"name: {name}" in line)
        run = next(index for index in range(start, len(lines)) if lines[index].strip() == "run: |")
        indent = len(lines[run]) - len(lines[run].lstrip()) + 2
        body = []
        for line in lines[run + 1:]:
            if line.strip() and len(line) - len(line.lstrip()) < indent:
                break
            body.append(line[indent:])
        return "set -eo pipefail\n" + "\n".join(body) + "\n"

    def test_the_sign_jobs_own_lines_check_and_sign_what_a_build_makes(self):
        with tempfile.TemporaryDirectory(prefix="atomics-sign-job-") as temp:
            directory = Path(temp)
            (directory / "scripts").mkdir()
            (directory / "assets").mkdir()
            shutil.copy(ROOT / "scripts/sign-release.sh", directory / "scripts/sign-release.sh")
            private, public = directory / "throwaway.pem", directory / "assets/release-signing-key.pub"
            subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", str(private)], check=True,
                           capture_output=True)
            subprocess.run(["openssl", "pkey", "-in", str(private), "-pubout", "-out", str(public)], check=True,
                           capture_output=True)
            unsigned = directory / "unsigned"
            unsigned.mkdir()
            names = [f"atomics-1.2.3-{target}.tar.gz" for target in
                     ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")]
            names += [f"atomics-vm-1.2.3-aarch64-linux.{suffix}" for suffix in ("raw.zst", "bundle.json")]
            for name in names:
                (unsigned / name).write_bytes(name.encode())
                # As the desktop job writes them, with shasum.
                digest = hashlib.sha256(name.encode()).hexdigest()
                (unsigned / f"{name}.sha256").write_text(f"{digest}  {name}\n")

            def run(step, **extra):
                env = dict(os.environ, GITHUB_REF_NAME="v1.2.3", UNSIGNED=str(unsigned), RUNNER_TEMP=temp, **extra)
                return subprocess.run(["bash", "-c", self.step_script(step)], cwd=directory, env=env,
                                      capture_output=True, text=True)

            check, sign = "Check what the builds made", "Sign and verify each archive"
            self.assertEqual(run(check).returncode, 0, run(check).stderr)
            signed = run(sign, ATOMICS_RELEASE_SIGNING_KEY=private.read_text())
            self.assertEqual(signed.returncode, 0, signed.stderr)
            for name in names:
                verified = subprocess.run(
                    ["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(public),
                     "-in", str(unsigned / name), "-sigfile", str(unsigned / f"{name}.sig")],
                    capture_output=True,
                )
                self.assertEqual(verified.returncode, 0, name)
            self.assertNotIn("PRIVATE", signed.stdout + signed.stderr)

            # No key says what to do, and signs nothing.
            (unsigned / f"{names[0]}.sig").unlink()
            missing_key = run(sign, ATOMICS_RELEASE_SIGNING_KEY="")
            self.assertNotEqual(missing_key.returncode, 0)
            self.assertIn("docs/releasing.md", missing_key.stderr)
            self.assertFalse((unsigned / f"{names[0]}.sig").exists())

            # What the builds left is checked by the names this release should have, and not taken on trust.
            original = (unsigned / names[0]).read_bytes()
            (unsigned / names[0]).write_bytes(b"changed after it was built")
            self.assertNotEqual(run(check).returncode, 0)
            (unsigned / names[0]).unlink()
            self.assertIn("did not make", run(check).stdout)
            (unsigned / "elsewhere").write_bytes(original)
            (unsigned / names[0]).symlink_to(unsigned / "elsewhere")
            self.assertIn("did not make", run(check).stdout, "a link is not an archive")

    def test_the_updater_and_the_install_script_trust_the_same_two_files(self):
        names = ["release-signing-key.pub", "release-signing-key-offline.pub"]
        build = (ROOT / "crates/atomics/build.rs").read_text()
        self.assertIn(f'["{names[0]}", "{names[1]}"]', build)
        script = (ROOT / "scripts/install.sh").read_text()
        self.assertIn(f"key_files='{names[0]} {names[1]}'", script)


class ReleasePreflightTests(unittest.TestCase):
    """Run the workflow's shell against real temporary Git history and a read-only API stand-in."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="atomics-preflight-test-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.git("init", "--initial-branch=main")
        self.git("commit", "--allow-empty", "-m", "Release commit")
        self.commit = self.git("rev-parse", "HEAD")
        self.git("tag", "-a", "v1.2.3", "-m", "Annotated release tag")
        self.tag_object = self.git("rev-parse", "v1.2.3")
        self.git("commit", "--allow-empty", "-m", "Later main commit")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("checkout", "--detach", "v1.2.3")
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" > "$GH_CALLS"\n'
            'printf "%s\\n" "$CI_RESULT"\nexit "${GH_EXIT:-0}"\n'
        )
        gh.chmod(0o755)
        self.calls = self.directory / "gh-calls"
        self.env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                        GH_CALLS=str(self.calls), GITHUB_REPOSITORY="owner/project",
                        GITHUB_SHA=self.tag_object, GH_EXIT="0")
        ReleaseWorkflowTests.setUpClass()
        self.script = ReleaseWorkflowTests().step_script("Check the tagged commit and its CI", "preflight")

    def git(self, *args):
        result = subprocess.run(
            ["git", "-c", "user.name=Test", "-c", "user.email=test@example.com", *args],
            cwd=self.directory, capture_output=True, text=True, check=True,
        )
        return result.stdout.strip()

    def run_preflight(self, commit=None, branch="main", event="push", status="completed", conclusion="success",
                      response=None, **extra):
        result = response if response is not None else "\t".join(
            (commit or self.commit, branch, event, status, conclusion)
        )
        return subprocess.run(["bash", "-c", self.script], cwd=self.directory,
                              env=dict(self.env, CI_RESULT=result, **extra), capture_output=True, text=True)

    def test_accepts_an_annotated_tag_on_a_green_ancestor_of_main(self):
        result = self.run_preflight()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.calls.read_text()
        for arg in ("--method GET", "repos/owner/project/actions/workflows/ci.yml/runs",
                    "-f branch=main", "-f event=push", f"-f head_sha={self.commit}", "-f per_page=1"):
            self.assertIn(arg, calls)
        self.assertNotIn("-f status=", calls, "an older success must not hide a newer failed or running CI")
        self.assertNotEqual(self.commit, self.tag_object)

    def test_refuses_a_commit_outside_main_before_reading_ci(self):
        self.git("commit", "--allow-empty", "-m", "Unmerged release commit")
        result = self.run_preflight()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ancestor of origin/main", result.stdout)
        self.assertFalse(self.calls.exists())

    def test_refuses_a_missing_main_reference(self):
        self.git("update-ref", "-d", "refs/remotes/origin/main")
        self.assertNotEqual(self.run_preflight().returncode, 0)
        self.assertFalse(self.calls.exists())

    def test_refuses_every_non_successful_or_unfinished_ci_result(self):
        for conclusion in ("failure", "cancelled", "skipped", "neutral", "timed_out", ""):
            with self.subTest(conclusion=conclusion):
                result = self.run_preflight(conclusion=conclusion)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("successful CI push run on main", result.stdout)
        for status in ("queued", "in_progress", "waiting"):
            with self.subTest(status=status):
                self.assertNotEqual(self.run_preflight(status=status).returncode, 0)

    def test_refuses_missing_ci_api_errors_and_results_for_other_commits_or_events(self):
        for extra in ({"response": ""}, {"GH_EXIT": "1"}, {"commit": "0" * 40},
                      {"branch": "other"}, {"event": "pull_request"}, {"event": "workflow_dispatch"}):
            with self.subTest(extra=extra):
                self.assertNotEqual(self.run_preflight(**extra).returncode, 0)


class MakeReleaseKeysTests(unittest.TestCase):
    """Throwaway keys in a temporary checkout, with a stand-in for gh: no real key is ever made here."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="atomics-keys-test-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.checkout = self.directory / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        (self.checkout / "assets").mkdir()
        for script in ("make-release-keys.sh", "sign-release.sh"):
            shutil.copy(ROOT / "scripts" / script, self.checkout / "scripts" / script)
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.outside = self.directory / "outside"
        self.outside.mkdir()
        self.offline = self.outside / "offline.pem"
        self.stored = self.directory / "stored-secret"
        self.gh_calls = self.directory / "gh-calls"
        self.gh("printf 'required_reviewers\\n'", "cat > '%s'" % self.stored)

    def gh(self, on_api, on_secret):
        program = self.bin / "gh"
        program.write_text(
            "#!/bin/sh\necho \"$@\" >> '%s'\ncase \"$1\" in\n  api) %s ;;\n  secret) %s ;;\nesac\n"
            % (self.gh_calls, on_api, on_secret)
        )
        program.chmod(0o755)

    def run_it(self, offline=None, answers="a passphrase\na passphrase\n"):
        env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}")
        return subprocess.run(
            ["sh", str(self.checkout / "scripts/make-release-keys.sh"), str(offline or self.offline)],
            env=env, input=answers, capture_output=True, text=True,
        )

    def public_of(self, private, passphrase=None):
        command = ["openssl", "pkey", "-in", str(private), "-pubout"]
        if passphrase:
            command += ["-passin", f"pass:{passphrase}"]
        return subprocess.run(command, check=True, capture_output=True, text=True).stdout

    def nothing_kept(self):
        self.assertEqual(list((self.checkout / "assets").iterdir()), [])
        self.assertEqual(list(self.outside.iterdir()), [])
        self.assertFalse(self.stored.exists())

    def test_it_makes_two_different_keys_and_keeps_each_where_it_belongs(self):
        result = self.run_it()
        self.assertEqual(result.returncode, 0, result.stderr)
        ci_public = (self.checkout / "assets/release-signing-key.pub").read_text()
        offline_public = (self.checkout / "assets/release-signing-key-offline.pub").read_text()
        self.assertNotEqual(ci_public, offline_public)
        # The CI key went to the Environment and nowhere else; the offline key is encrypted and only its owner's.
        self.assertEqual(self.public_of(self.stored), ci_public)
        self.assertIn("--env release", self.gh_calls.read_text())
        self.assertEqual(self.public_of(self.offline, "a passphrase"), offline_public)
        self.assertIn("ENCRYPTED PRIVATE KEY", self.offline.read_text())
        self.assertEqual(self.offline.stat().st_mode & 0o777, 0o600)
        self.assertEqual([path.name for path in self.outside.iterdir()], ["offline.pem"])
        keys = sorted(path.name for path in (self.checkout / "assets").iterdir())
        self.assertEqual(keys, ["release-signing-key-offline.pub", "release-signing-key.pub"])
        said = result.stdout + result.stderr
        self.assertNotIn("PRIVATE KEY", said)
        self.assertNotIn("a passphrase\n", said)

    def test_at_a_real_terminal_it_hides_the_passphrase_and_gives_echo_back(self):
        # What the maintainer will actually run: a pseudo-terminal, and a passphrase a shell could trip on.
        passphrase = 'pa$$ \\word "with" `all` of it'
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        self.addCleanup(os.close, slave)
        env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}")
        process = subprocess.Popen(
            ["sh", str(self.checkout / "scripts/make-release-keys.sh"), str(self.offline)],
            stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
        )
        self.addCleanup(process.stderr.close)
        self.addCleanup(process.stdout.close)
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        said = bytearray()

        def read_stderr():
            while chunk := os.read(process.stderr.fileno(), 4096):
                said.extend(chunk)

        reader = threading.Thread(target=read_stderr)
        reader.start()
        self.addCleanup(reader.join, 30)
        echo = lambda: bool(termios.tcgetattr(slave)[3] & termios.ECHO)

        def until(what, done):
            deadline = time.monotonic() + 30
            while not done():
                self.assertLess(time.monotonic(), deadline, f"never: {what}; it said {bytes(said)!r}")
                time.sleep(0.01)

        for prompt in (b"Passphrase for the offline key", b"The same again"):
            until(prompt, lambda: prompt in said)
            # Each prompt waits with echo off before anything is typed.
            until("echo off", lambda: not echo())
            os.write(master, (passphrase + "\n").encode())
        until("the end", lambda: process.poll() is not None)
        out = process.stdout.read().decode()
        self.assertEqual(process.returncode, 0, bytes(said))
        self.assertTrue(echo(), "echo is back")
        self.assertNotIn(passphrase, out + said.decode())
        self.assertEqual(
            self.public_of(self.offline, passphrase),
            (self.checkout / "assets/release-signing-key-offline.pub").read_text(),
        )

    def test_what_it_makes_is_what_signing_a_release_takes_either_way(self):
        self.assertEqual(self.run_it().returncode, 0)
        archive = self.directory / "archive.tar.gz"
        archive.write_bytes(b"archive bytes")
        sign = ROOT / "scripts/sign-release.sh"
        env = dict(os.environ, ATOMICS_RELEASE_SIGNING_KEY=self.stored.read_text())
        signed = subprocess.run(
            ["sh", str(self.checkout / "scripts/sign-release.sh"), str(archive)],
            env=env, capture_output=True, text=True,
        )
        self.assertEqual(signed.returncode, 0, signed.stderr)
        # The hand-signed way, as docs/releasing.md has it, with the offline key.
        decrypted = subprocess.run(
            ["openssl", "pkey", "-in", str(self.offline), "-passin", "pass:a passphrase"],
            check=True, capture_output=True, text=True,
        ).stdout
        by_hand = subprocess.run(
            ["sh", str(sign), str(archive), str(self.checkout / "assets/release-signing-key-offline.pub")],
            env=dict(os.environ, ATOMICS_RELEASE_SIGNING_KEY=decrypted), capture_output=True, text=True,
        )
        self.assertEqual(by_hand.returncode, 0, by_hand.stderr)
        # Each key signs for itself, and not for the other.
        wrong = subprocess.run(
            ["sh", str(sign), str(archive), str(self.checkout / "assets/release-signing-key.pub")],
            env=dict(os.environ, ATOMICS_RELEASE_SIGNING_KEY=decrypted), capture_output=True, text=True,
        )
        self.assertNotEqual(wrong.returncode, 0)

    def test_it_will_not_run_without_a_reviewer_or_an_environment_and_keeps_nothing(self):
        self.gh("printf 'wait_timer\\n'", "cat > '%s'" % self.stored)
        result = self.run_it()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no required reviewer", result.stderr)
        self.nothing_kept()
        self.gh("exit 1", "cat > '%s'" % self.stored)
        result = self.run_it()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not there yet", result.stderr)
        self.nothing_kept()

    def test_a_failure_to_store_the_ci_key_keeps_nothing(self):
        self.gh("printf 'required_reviewers\\n'", "exit 1")
        result = self.run_it()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nothing was kept", result.stderr)
        self.assertEqual(list((self.checkout / "assets").iterdir()), [])
        self.assertEqual(list(self.outside.iterdir()), [])

    def test_it_refuses_a_place_or_a_passphrase_that_would_lose_something(self):
        inside = self.checkout / "offline.pem"
        result = self.run_it(offline=inside)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("out of this checkout", result.stderr)
        self.assertFalse(inside.exists())

        self.offline.write_text("an existing key")
        result = self.run_it()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("will not replace a key", result.stderr)
        self.assertEqual(self.offline.read_text(), "an existing key")
        self.offline.unlink()

        self.assertIn("differ", self.run_it(answers="one\nother\n").stderr)
        self.assertIn("needs a passphrase", self.run_it(answers="\n\n").stderr)
        self.nothing_kept()

        (self.checkout / "assets/release-signing-key.pub").write_text("trusted by installed copies")
        result = self.run_it()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Rotation", result.stderr)
        self.assertEqual((self.checkout / "assets/release-signing-key.pub").read_text(), "trusted by installed copies")


if __name__ == "__main__":
    unittest.main()
