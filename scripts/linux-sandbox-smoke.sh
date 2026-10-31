#!/bin/sh
# Run through `imacs queue --class qa -- scripts/linux-sandbox-smoke.sh`.
# Python owns timeouts, JSON, process groups and cleanup; no credentials are copied.
set -eu
exec python3 - "$0" "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser(description="Isolated Linux Docker Sandboxes smoke test")
parser.add_argument("--imacs", default="imacs", help="installed or already built executable")
parser.add_argument("--fake-sbx", action="store_true", help="exercise lifecycle with the test fixture, without KVM")
parser.add_argument("--timeout", type=float, default=900, help="sandbox startup deadline in seconds")
parser.add_argument("--latency-ms", type=float, default=1000, help="maximum ctl round trip in milliseconds")
parser.add_argument("--skip-relay", action="store_true",
                    help="skip sandboxed Claude through the credential relay (needs `imacs setup`'s Claude token)")
args = parser.parse_args(sys.argv[2:])
if sys.platform != "linux":
    parser.error("run this script on the Linux desktop (or in the Linux QA container)")
if args.timeout <= 0 or args.latency_ms <= 0:
    parser.error("timeouts and latency limits must be positive")
imacs = shutil.which(args.imacs)
if not imacs:
    parser.error("IMACS executable not found; install IMACS or pass --imacs /path/to/imacs")
imacs = str(Path(imacs).resolve())
source = Path(sys.argv[1]).resolve().parent.parent
# A short path keeps Unix sockets below sockaddr_un's length limit.
root = Path(tempfile.mkdtemp(prefix="imacs-smoke-", dir="/tmp"))
repo = root / "repo"
env = os.environ.copy()
env.pop("IMACS_PANE", None)
# Do not borrow signing, hooks, worktree redirection or identity from the host.
for key in list(env):
    if key.startswith("GIT_"):
        env.pop(key)
# Wayland finds a relative display in XDG_RUNTIME_DIR, which the instance gets its own copy of.
display = env.get("WAYLAND_DISPLAY", "")
if display and not os.path.isabs(display) and os.path.isabs(env.get("XDG_RUNTIME_DIR", "")):
    env["WAYLAND_DISPLAY"] = os.path.join(env["XDG_RUNTIME_DIR"], display)
env.update(IMACS_HOME=str(root / "state"), IMACS_CONFIG=str(root / "config.toml"),
           IMACS_SOCKET=str(root / "ctl.sock"), XDG_RUNTIME_DIR=str(root / "runtime"),
           TMPDIR=str(root / "tmp"), GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
           GIT_TERMINAL_PROMPT="0")
app = None
sessions = []
names = set()
failures = []
sbx = None
baseline = None


class Interrupted(Exception):
    pass


def run(command, timeout=60, check=True):
    # Files avoid waiting forever on stdout held open by a daemon's descendants.
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        child = subprocess.Popen(command, env=env, cwd=root, stdin=subprocess.DEVNULL,
                                 stdout=out, stderr=err, start_new_session=True)
        try:
            status = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
            raise RuntimeError(f"{Path(command[0]).name} {command[1:2]} timed out") from None
        except BaseException:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            raise
        out.seek(0)
        output = out.read().decode(errors="replace")
        if check and status:
            # sbx error output can include secrets. Never print it.
            raise RuntimeError(f"{Path(command[0]).name} {command[1:2]} failed ({status})")
        return output


def ctl(cmd, **fields):
    response = json.loads(run([imacs, "ctl", json.dumps(dict(cmd=cmd, **fields))], timeout=10))
    if not response.get("ok"):
        raise RuntimeError(f"ctl {cmd} failed: {response.get('error', 'unknown error')}")
    return response["result"]


def listed(*command):
    result = json.loads(run([sbx, *command, "--json"]))
    if not isinstance(result, list):
        raise RuntimeError(f"sbx {command} returned an unreadable list")
    return result


def own_sandboxes():
    found = []
    for item in listed("ls"):
        if any(Path(p.removesuffix(":ro")).is_relative_to(root) for p in item.get("workspaces", [])):
            names.add(item["name"])
            found.append(item)
    return found


def until(check, what):
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        if app.poll() is not None:
            raise RuntimeError("isolated IMACS exited before " + what)
        try:
            result = check()
            if result:
                return result
        except Interrupted:
            raise
        except (RuntimeError, OSError, ValueError):
            pass
        time.sleep(0.2)
    raise RuntimeError("timed out waiting for " + what)


def interrupted(signum, frame):
    raise Interrupted(f"interrupted by signal {signum}")


def app_processes():
    # sbx helpers and PTYs have their own groups; include them before orphaning them.
    rows = [list(map(int, line.split())) for line in run(["ps", "-eo", "pid=,ppid=,pgid="]).splitlines()]
    owned = {app.pid}
    while True:
        children = {pid for pid, parent, group in rows if parent in owned}
        if children <= owned:
            return [(pid, group) for pid, parent, group in rows if pid in owned]
        owned.update(children)


def cleanup():
    # Stop the app before removal so background preparation cannot recreate resources.
    if app is not None:
        processes = []
        try:
            # A start request can time out after creating a session but before returning its id.
            known = {session["id"] for session in sessions}
            for session in ctl("list")["sessions"]:
                if Path(session["folder"]).is_relative_to(root) and session["id"] not in known:
                    sessions.append(session)
                    names.add("imacs-" + session["id"])
        except Exception:
            pass
        for session in sessions:
            try:
                ctl("close", session=session["id"])
            except Exception:
                pass
        try:
            if app.poll() is None:
                os.kill(app.pid, signal.SIGSTOP)
            processes = app_processes()
            for pid, group in reversed(processes):
                try:
                    if pid == group:
                        os.killpg(group, signal.SIGKILL)
                    else:
                        os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            # Even if the leader exited, a launcher/helper may still own the group.
            os.killpg(app.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except Exception as error:
            failures.append(f"process cleanup: {error}")
            app.kill()
        try:
            app.wait(timeout=10)
        except subprocess.TimeoutExpired:
            failures.append("isolated app did not stop")
        # Reparented zombies may wait briefly for init; they cannot keep working.
        deadline = time.monotonic() + 5
        while True:
            live = []
            for pid, group in processes:
                try:
                    status = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
                    if status != "Z":
                        live.append(pid)
                except FileNotFoundError:
                    pass
            if not live or time.monotonic() >= deadline:
                break
            time.sleep(0.05)
        if live:
            failures.append(f"app/helper processes remain: {live}")
        else:
            print("PASS cleanup: isolated app and helper processes stopped")
    if sbx and baseline is not None:
        try:
            own_sandboxes()
            # Include a registration made just before a failed create. With no configured
            # external MCPs, every new imacs registration points at our unique socket.
            for item in listed("mcp", "ls"):
                if str(root / "ctl.sock") in json.dumps(item):
                    names.add(item["name"])
            for name in sorted(names):
                run([sbx, "rm", "--force", name], check=False)
                run([sbx, "mcp", "rm", "--force", name], check=False)
            remaining = own_sandboxes()
            registered = {item["name"] for item in listed("mcp", "ls")}
            if remaining or names & registered:
                raise RuntimeError(f"sandbox or MCP registration remains: {[item['name'] for item in remaining]}, {sorted(names & registered)}")
            if not baseline <= registered:
                raise RuntimeError("a pre-existing MCP registration was lost")
            print("PASS cleanup: no smoke sandboxes or MCP registrations remain")
        except Exception as error:
            failures.append(f"sandbox cleanup: {error}")
    if (repo / ".git").exists():
        try:
            worktrees = run(["git", "-C", str(repo), "worktree", "list", "--porcelain"])
            for line in worktrees.splitlines():
                if line.startswith("worktree "):
                    path = Path(line[9:])
                    if path != repo:
                        if not path.is_relative_to(root):
                            raise RuntimeError("refusing worktree outside smoke directory")
                        run(["git", "-C", str(repo), "worktree", "remove", "--force", str(path)])
            remaining = run(["git", "-C", str(repo), "worktree", "list", "--porcelain"])
            if [line for line in remaining.splitlines() if line.startswith("worktree ")] != [f"worktree {repo}"]:
                raise RuntimeError("linked worktree metadata remains")
            print("PASS cleanup: no linked worktrees remain")
        except Exception as error:
            failures.append(f"worktree cleanup: {error}")
    if failures and (root / "app.log").exists():
        print("Isolated app log (last 20 lines):")
        print("\n".join((root / "app.log").read_text(errors="replace").splitlines()[-20:]))
    try:
        shutil.rmtree(root)
        if root.exists():
            raise RuntimeError("smoke directory remains")
        print("PASS cleanup: state, socket, runtime, temp and repository directories removed")
    except Exception as error:
        failures.append(f"temporary directory cleanup: {error}")


for sig in [signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
    signal.signal(sig, interrupted)

try:
    for directory in ["runtime", "tmp", "state", "repo"]:
        (root / directory).mkdir(mode=0o700)
    (root / "config.toml").write_text('''base_ref = "main"
worktree_path = "{repo_parent}/worktrees/{slug}"
[isolation]
mode = "sandbox"
[resources]
jobs = 2
agent_nice = 0
''')
    if args.fake_sbx:
        print("FAKE SBX: lifecycle rehearsal only; KVM and real VM isolation are not tested")
        fake = root / "fake"
        fake.mkdir()
        shutil.copy2(source / "crates/imacs/tests/fixtures/sbx", fake / "sbx")
        (fake / "sbx").chmod(0o700)
        (fake / "agent").write_text("exec /bin/sh -i\n")
        # Prevent a login profile from replacing the fixture's PATH.
        shell = fake / "shell"
        shell.write_text('#!/bin/sh\n[ "$1" != -l ] || shift\nexec /bin/sh "$@"\n')
        shell.chmod(0o700)
        env.update(PATH=str(fake) + os.pathsep + env["PATH"], SHELL=str(shell))
    sbx = shutil.which("sbx", path=env["PATH"])
    if not sbx:
        raise RuntimeError("sbx missing; run imacs setup first")
    baseline = {item["name"] for item in listed("mcp", "ls")}
    diagnosis = json.loads(run([sbx, "diagnose", "--json"], check=False))
    checks = {item["name"].lower(): item["status"] for item in diagnosis["checks"]}
    if checks.get("global network policy", "pass") != "pass":
        raise RuntimeError("network policy missing; run imacs setup first (smoke never changes global policy)")
    print(run([imacs, "doctor"], timeout=180), end="", flush=True)
    print("PASS doctor completed (review its host/setup warnings above)")
    if args.fake_sbx or args.skip_relay:
        print("SKIP credential relay: " + ("the fake sbx has no VM or Claude" if args.fake_sbx else "--skip-relay"))
    else:
        # One throwaway Claude sandbox on the subscription in the keyring; it removes itself. Its
        # output is PASS/FAIL lines without values, so it is shown even on failure.
        output = run([imacs, "sandbox", "relay-check"], timeout=args.timeout, check=False)
        print(output, end="", flush=True)
        if "PASS relay check" not in output:
            raise RuntimeError("sandboxed Claude did not answer through the credential relay (see above)")
    run(["git", "-C", str(repo), "init", "-q", "-b", "main"])
    run(["git", "-C", str(repo), "-c", "user.name=IMACS smoke", "-c", "user.email=smoke@example.invalid",
         "commit", "--allow-empty", "-qm", "Isolated sandbox smoke repository"])
    with (root / "app.log").open("w") as log:
        app = subprocess.Popen([imacs, str(repo)], env=env, cwd=repo, stdin=subprocess.DEVNULL,
                               stdout=log, stderr=log, start_new_session=True)
    until(lambda: ctl("list"), "control socket")
    for number in [1, 2]:
        session = ctl("start", agent="shell", repo=str(repo), branch=f"smoke-{number}")
        sessions.append(session)
        names.add("imacs-" + session["id"])
    until(lambda: len(own_sandboxes()) == 2 and all(
        item.get("sandbox") and not item["outside_imacs"]
        for item in ctl("list")["sessions"]), "two attached sandbox shells")
    for number, session in enumerate(sessions, 1):
        # Split the marker so terminal input echo cannot satisfy the output check.
        command = f"printf '%s%s\\n' 'IMACS_SMOKE_' '{number}_OK' | tee smoke-output\r"
        until(lambda: ctl("send", session=session["id"], text=command, raw=True), "shell input")
        marker = f"IMACS_SMOKE_{number}_OK"
        path = Path(session["folder"]) / "smoke-output"
        until(lambda: marker in ctl("read", session=session["id"])["screen"]
              and path.exists() and path.read_text().strip() == marker, "shell output and workspace sync")
        print(f"PASS sandbox shell {number}: output visible and worktree file synchronized")
    timings = []
    for _ in range(20):
        start = time.monotonic()
        ctl("list")
        timings.append((time.monotonic() - start) * 1000)
    print(f"ctl latency (20 calls): median={sorted(timings)[10]:.1f}ms max={max(timings):.1f}ms limit={args.latency_ms:g}ms")
    if max(timings) > args.latency_ms:
        raise RuntimeError("ctl latency limit exceeded")
    print("PASS ctl latency with two sandboxed shells")
except Exception as error:
    failures.append(str(error))
finally:
    # A second Ctrl-C must not bypass cleanup.
    for sig in [signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
        signal.signal(sig, signal.SIG_IGN)
    cleanup()
for failure in failures:
    print("FAIL " + failure, file=sys.stderr)
print(f"{'FAIL' if failures else 'PASS'} Linux sandbox smoke ({'fake sbx' if args.fake_sbx else 'real sbx'})", flush=True)
sys.exit(bool(failures))
PY
