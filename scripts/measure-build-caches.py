#!/usr/bin/env python3
"""Two fresh-worktree build series, through one ATOMICS test slot. No existing cache is removed."""
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


ROOT = Path(__file__).resolve().parent.parent
FLOOR = 40 * 1024**3


def space(path):
    free = shutil.disk_usage(path).free
    if free < FLOOR:
        raise RuntimeError(f"Stopped: {free / 1024**3:.1f} GiB free; at least 40 GiB required")
    return free


def used(path):
    # Allocated blocks, with links counted as links, not their targets.
    return int(subprocess.check_output(["du", "-sk", str(path)], text=True).split()[0]) * 1024


def run_build(command, cwd, env, log):
    start = time.monotonic()
    with log.open("w") as output:
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        try:
            while process.poll() is None:
                space(ROOT)
                time.sleep(1)
            if process.returncode:
                raise RuntimeError(f"Build failed ({process.returncode}); see {log}")
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
    return round(time.monotonic() - start, 2)


def measure(count, package, report):
    initial_free = space(ROOT)
    if not shutil.which("sccache"):
        raise RuntimeError("sccache is required; run in the pinned dev shell")
    result = {"revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "package": package, "worktrees_per_series": count, "initial_free_gib": round(initial_free / 1024**3, 2),
              "toolchain": subprocess.check_output(["rustc", "--version"], text=True).strip(), "series": {}}
    # Keep logs and reports in this worktree. Only temporary worktrees and our isolated sccache are removed.
    report.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="cache-measure-", dir=ROOT / "target") as temp, \
            tempfile.TemporaryDirectory(prefix="atomics-cache-") as socket_temp:
        temp = Path(temp)
        env_base = os.environ.copy()
        for key in ["CARGO_TARGET_DIR", "RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"]:
            env_base.pop(key, None)
        # Separate server endpoint: never stop or reset another agent's sccache server or counters.
        env_base.update(SCCACHE_DIR=str(temp / "sccache"), SCCACHE_CACHE_SIZE="10G",
                        SCCACHE_SERVER_UDS=str(Path(socket_temp) / "sccache.sock"))
        try:
            for mode in ["off", "on"]:
                rows, worktrees = [], []
                try:
                    for index in range(count):
                        space(ROOT)
                        worktree = temp / f"{mode}-{index + 1}"
                        subprocess.run(["git", "worktree", "add", "--detach", str(worktree), "HEAD"], cwd=ROOT,
                                       check=True, stdout=subprocess.DEVNULL)
                        worktrees.append(worktree)
                        env = env_base.copy()
                        env.update(CARGO_INCREMENTAL="1" if mode == "off" else "0",
                                   CARGO_PROFILE_DEV_DEBUG="2" if mode == "off" else "line-tables-only",
                                   CARGO_PROFILE_TEST_DEBUG="2" if mode == "off" else "line-tables-only")
                        if mode == "on":
                            env["RUSTC_WRAPPER"] = shutil.which("sccache")
                            env["SCCACHE_IGNORE_SERVER_IO_ERROR"] = "1"
                        seconds = run_build(["cargo", "build", "--locked", "-p", package], worktree, env,
                                            report.with_name(f"{report.stem}-{mode}-{index + 1}.log"))
                        rows.append({"worktree": index + 1, "seconds": seconds, "output_bytes": used(worktree / "target")})
                        print(f"{mode} worktree {index + 1}: {seconds}s, {rows[-1]['output_bytes'] / 1024**3:.3f} GiB", flush=True)
                    cache_bytes = used(temp / "sccache") if (temp / "sccache").exists() else 0
                    result["series"][mode] = {"builds": rows, "total_seconds": round(sum(r["seconds"] for r in rows), 2),
                                              "output_bytes": sum(r["output_bytes"] for r in rows), "shared_cache_bytes": cache_bytes,
                                              "total_bytes": sum(r["output_bytes"] for r in rows) + cache_bytes}
                    if mode == "on":
                        result["sccache_stats"] = json.loads(subprocess.check_output(["sccache", "--show-stats", "--stats-format", "json"], env=env_base, text=True))
                finally:
                    for worktree in reversed(worktrees):
                        subprocess.run(["git", "worktree", "remove", "--force", str(worktree)], cwd=ROOT, check=True)
        finally:
            subprocess.run(["sccache", "--stop-server"], env=env_base, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    result["final_free_gib"] = round(space(ROOT) / 1024**3, 2)
    report.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Report: {report}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--count", type=int, default=2, choices=range(1, 5))
    parser.add_argument("--package", default="atomics-core")
    parser.add_argument("--report", type=Path, default=ROOT / "target/build-cache-measurement.json")
    parser.add_argument("--run", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    space(ROOT)
    if not args.run:
        # The long-running parent remains attached to the shared queue until both series finish.
        subprocess.run(["atomics", "queue", "--class", "test", "--", "nix", "--extra-experimental-features",
                        "nix-command flakes", "develop", ".#default", "--command", "python3", str(Path(__file__).resolve()),
                        "--run", "--count", str(args.count), "--package", args.package, "--report", str(args.report.resolve())],
                       cwd=ROOT, check=True)
    else:
        (ROOT / "target").mkdir(exist_ok=True)
        measure(args.count, args.package, args.report.resolve())


if __name__ == "__main__":
    main()
