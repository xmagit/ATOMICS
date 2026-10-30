"""Exercise the host runner's volume lifecycle without a Docker daemon."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


LABEL = "com.imacs.linux-qa.checkout"
FAKE_DOCKER = r'''
import json
import os
import sys
from pathlib import Path

path = Path(os.environ["DOCKER_TEST_STATE"])
state = json.loads(path.read_text())
args = sys.argv[1:]
state["calls"].append(args)
status = 0
if args[:2] == ["volume", "ls"]:
    assert args[2:] == ["--filter", "label=com.imacs.linux-qa.checkout", "--format", "{{.Name}}"]
    for name, checkout in state["volumes"].items():
        if checkout is not None:
            print(name)
elif args[:2] == ["volume", "inspect"]:
    name = args[2]
    if name not in state["volumes"] or name in state.get("inspect_failures", []):
        status = 1
    elif "--format" in args:
        assert args[3:] == ["--format", '{{ index .Labels "com.imacs.linux-qa.checkout" }}']
        print(state["volumes"][name] or "<no value>")
elif args[:2] == ["volume", "create"]:
    assert args[2] == "--label"
    key, checkout = args[3].split("=", 1)
    assert key == "com.imacs.linux-qa.checkout"
    state["volumes"].setdefault(args[4], checkout)
elif args[:1] == ["ps"]:
    assert args[1:3] == ["-aq", "--filter"]
    name = args[3].removeprefix("volume=")
    if name in state.get("ps_failures", []):
        status = 1
    elif name in state.get("containers", {}):
        # -a is required to protect stopped containers too.
        print(state["containers"][name])
elif args[:2] == ["volume", "rm"]:
    assert len(args) == 3, "removal must never use --force"
    name = args[2]
    if name in state.get("rm_failures", []):
        status = 1
    else:
        del state["volumes"][name]
elif args[:1] == ["run"]:
    mounts = [args[i + 1] for i, arg in enumerate(args) if arg == "--mount"]
    assert any(f"source={name},target=/target" in mount for name in state["volumes"] for mount in mounts)
elif args != ["info"] and args[:2] != ["image", "inspect"]:
    raise AssertionError(f"unexpected docker invocation: {args}")
path.write_text(json.dumps(state))
sys.exit(status)
'''


class LinuxQaVolumes(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="imacs-linux-qa-test-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.checkout = self.directory / "checkout with spaces"
        (self.checkout / "scripts").mkdir(parents=True)
        shutil.copy2(Path(__file__).resolve().parents[1] / "scripts/linux-qa.sh", self.checkout / "scripts")
        (self.checkout / "Dockerfile.linux-qa").write_text("FROM fixture\n")
        subprocess.run(["git", "init", "-q", str(self.checkout)], check=True)
        tools = self.directory / "tools"
        tools.mkdir()
        docker = tools / "docker"
        docker.write_text(f"#!{sys.executable}\n" + FAKE_DOCKER)
        docker.chmod(0o755)
        self.state = self.directory / "docker.json"
        self.env = dict(os.environ, DOCKER_TEST_STATE=str(self.state),
                        PATH=str(tools) + os.pathsep + os.environ["PATH"])
        key = subprocess.run(["cksum"], input=str(self.checkout), text=True, capture_output=True, check=True).stdout.split()[0]
        self.target = "imacs-linux-qa-target-" + key

    def run_runner(self, state, *args):
        self.state.write_text(json.dumps(state))
        result = subprocess.run(["bash", str(self.checkout / "scripts/linux-qa.sh"), *(args or ["shell"])],
                                env=self.env, stdin=subprocess.DEVNULL, text=True, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(self.state.read_text()), result

    def test_screenshot_passes_control_requests_to_the_container_in_order(self):
        output = self.directory / "shots" / "ungrouped.png"
        requests = ['{"cmd":"start","agent":"shell","isolation":"bare"}', '{"cmd":"activate","session":"a b"}']
        state, _ = self.run_runner(dict(volumes={}, calls=[]), "screenshot", str(output), "--size", "1280x800",
                                   "--ctl", requests[0], "--ctl", requests[1])
        run = next(call for call in state["calls"] if call[:1] == ["run"])
        self.assertEqual(run[-5:], ["/repo/scripts/linux-qa-container.sh", "screenshot", "1280x800", *requests])
        self.assertTrue(output.exists())
        state, _ = self.run_runner(dict(volumes={}, calls=[]), "screenshot", str(output))
        run = next(call for call in state["calls"] if call[:1] == ["run"])
        self.assertEqual(run[-3:], ["/repo/scripts/linux-qa-container.sh", "screenshot", "1600x1000"])

    def test_screenshot_options_need_values(self):
        for args in (["--ctl"], ["--ctl", ""], ["--size"], ["--other", "x"]):
            result = subprocess.run(["bash", str(self.checkout / "scripts/linux-qa.sh"), "screenshot",
                                     str(self.directory / "out.png"), *args],
                                    env=self.env, stdin=subprocess.DEVNULL, text=True, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 2, args)

    def test_new_target_has_checkout_label_and_is_reused(self):
        state, _ = self.run_runner(dict(volumes={}, calls=[]))
        self.assertEqual(state["volumes"], {self.target: str(self.checkout)})
        state, _ = self.run_runner(state)
        self.assertEqual(sum(call[:2] == ["volume", "create"] for call in state["calls"]), 1)

    def test_only_labelled_missing_unused_targets_are_removed(self):
        missing = str(self.directory / "deleted checkout with spaces")
        existing = self.directory / "another checkout"
        existing.mkdir()
        broken = self.directory / "offline symlink"
        broken.symlink_to(self.directory / "offline mount")
        volumes = {
            "imacs-linux-qa-target-orphan": missing,
            "imacs-linux-qa-target-live": str(existing),
            "imacs-linux-qa-target-broken-link": str(broken),
            "imacs-linux-qa-target-running": missing,
            "imacs-linux-qa-target-stopped": missing,
            "imacs-linux-qa-target-unlabelled": None,
            "imacs-linux-qa-target-relative": "relative/checkout",
            "imacs-linux-qa-target-empty": "",
            "imacs-linux-qa-target-inspect-error": missing,
            "imacs-linux-qa-target-ps-error": missing,
            "imacs-linux-qa-target-race": missing,
            "imacs-linux-qa-registry": missing,
            "imacs-linux-qa-git": missing,
            "someone-elses-volume": missing,
        }
        state, result = self.run_runner(dict(
            volumes=volumes, calls=[],
            containers={"imacs-linux-qa-target-running": "running", "imacs-linux-qa-target-stopped": "stopped"},
            inspect_failures=["imacs-linux-qa-target-inspect-error"],
            ps_failures=["imacs-linux-qa-target-ps-error"],
            rm_failures=["imacs-linux-qa-target-race"],
        ))
        expected = dict(volumes)
        del expected["imacs-linux-qa-target-orphan"]
        expected[self.target] = str(self.checkout)
        self.assertEqual(state["volumes"], expected)
        removals = [call[2] for call in state["calls"] if call[:2] == ["volume", "rm"]]
        self.assertEqual(removals, ["imacs-linux-qa-target-orphan", "imacs-linux-qa-target-race"])
        self.assertIn("Removed orphaned Linux QA target cache", result.stderr)
        self.assertEqual(result.stdout, "", "cleanup must not corrupt screenshot output")

    def test_existing_unlabelled_target_is_never_adopted_or_removed(self):
        state, _ = self.run_runner(dict(volumes={self.target: None}, calls=[]))
        self.assertEqual(state["volumes"], {self.target: None})
        self.assertFalse(any(call[:2] in (["volume", "create"], ["volume", "rm"]) for call in state["calls"]))


if __name__ == "__main__":
    unittest.main()
