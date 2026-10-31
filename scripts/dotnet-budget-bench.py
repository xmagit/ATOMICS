#!/usr/bin/env python3
"""Measure three concurrent SDK 9 builds without touching an existing repository.

Run outside a sandbox that blocks ps. Only this script's descendants are counted
and stopped; never shuts down another agent's build servers. Logs remain in the
printed temporary directory.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import uuid


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--reuse-root", type=Path, help="reuse this script's generated, restored worktrees")
parser.add_argument("--only", nargs="+", help="measure only these variant labels")
options = parser.parse_args()
personal_dotnet = Path.home() / ".dotnet/dotnet"
DOTNET = os.environ.get("DOTNET") or (str(personal_dotnet) if personal_dotnet.is_file() else shutil.which("dotnet"))
if not DOTNET:
    raise SystemExit("Install .NET SDK 9 or set DOTNET to its executable before running this benchmark.")
ROOT = options.reuse_root or Path(tempfile.mkdtemp(prefix="atomics-dotnet-budget-"))
KEYS = (
    "DOTNET_PROCESSOR_COUNT", "MSBUILDDISABLENODEREUSE",
    "DOTNET_CLI_USE_MSBUILD_SERVER", "UseSharedCompilation", "BuildInParallel",
    "MSBuildMaxCpuCount", "VSTestCLIRunSettings",
)
BASE = {k: v for k, v in os.environ.items() if k not in KEYS}
BASE.update(DOTNET_CLI_TELEMETRY_OPTOUT="1", DOTNET_NOLOGO="1")
# A private Roslyn pipe prevents reusing another agent's compiler server.
BASE["SharedCompilationId"] = "atomics-budget-" + uuid.uuid4().hex
BASE["MSBUILDNODEHANDSHAKESALT"] = BASE["SharedCompilationId"]


def run(args, cwd, env):
    result = subprocess.run([DOTNET, *args], cwd=cwd, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if result.returncode:
        raise RuntimeError(result.stdout)


def processes():
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid=,args="], text=True)
    rows = {}
    for line in output.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) == 3:
            rows[int(parts[0])] = (int(parts[1]), parts[2])
    return rows


def descendants(rows, known):
    while True:
        more = {pid for pid, (parent, _) in rows.items() if parent in known}
        if more <= known:
            return
        known.update(more)


def measure(label, extra):
    env = BASE | extra
    logs = [(ROOT / f"{label}-{i}.log").open("w") for i in range(3)]
    started = time.monotonic()
    children = [subprocess.Popen([DOTNET, "build", "Bench.sln", "--no-restore",
                                  "-t:Rebuild", "-v:q"], cwd=ROOT / f"work{i}",
                                 env=env, stdout=log, stderr=subprocess.STDOUT)
                for i, log in enumerate(logs)]
    known = {p.pid for p in children}
    peak = dict(total=0, msbuild=0, vbcs=0, csc=0)
    peak_commands = []
    try:
        while True:
            rows = processes()
            descendants(rows, known)
            commands = [args for pid, (_, args) in rows.items()
                        if pid in known and (args.startswith(DOTNET + " ") or "/dotnet " in args)]
            counts = dict(total=len(commands),
                          msbuild=sum("MSBuild.dll" in c for c in commands),
                          vbcs=sum("VBCSCompiler.dll" in c for c in commands),
                          csc=sum("csc.dll" in c for c in commands))
            if counts["total"] > peak["total"]:
                peak_commands = commands
            peak = {k: max(v, counts[k]) for k, v in peak.items()}
            if all(p.poll() is not None for p in children):
                break
            time.sleep(0.05)
        elapsed = round(time.monotonic() - started, 2)
        if any(p.returncode for p in children):
            raise RuntimeError(f"build failed: see {ROOT}/{label}-*.log")
        time.sleep(1)
        rows = processes()
        descendants(rows, known)
        remaining = [args for pid, (_, args) in rows.items()
                     if pid in known and ("MSBuild.dll" in args or "VBCSCompiler.dll" in args)]
        result = dict(label=label, env=extra, seconds=elapsed, peak=peak,
                      retained_msbuild=sum("MSBuild.dll" in c for c in remaining),
                      retained_vbcs=sum("VBCSCompiler.dll" in c for c in remaining))
        (ROOT / f"{label}-processes.log").write_text("\n".join(peak_commands + ["RETAINED", *remaining]) + "\n")
        print(json.dumps(result), flush=True)
        return result
    finally:
        # Only stop descendants observed while their parent was still alive.
        rows = processes()
        descendants(rows, known)
        for pid in known:
            if pid in rows and ("MSBuild.dll" in rows[pid][1] or "VBCSCompiler.dll" in rows[pid][1]):
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
        for p in children:
            if p.poll() is None:
                p.terminate()
            p.wait()
        for log in logs:
            log.close()


print(f"Logs and generated projects: {ROOT}", flush=True)
if not options.reuse_root:
    template = ROOT / "template"
    template.mkdir()
    (template / "global.json").write_text('{"sdk":{"version":"9.0.313"}}\n')
    run(["new", "sln", "-n", "Bench"], template, BASE)
    for i in range(8):
        run(["new", "classlib", "-n", f"P{i}", "--no-restore"], template, BASE)
        run(["sln", "Bench.sln", "add", f"P{i}/P{i}.csproj"], template, BASE)
        # Independent projects keep all eight runnable at once. Substantial compiler
        # work makes short-lived processes observable without synthetic build sleeps.
        source = f"namespace P{i};\n" + "\n".join(
            f"public class C{j} {{ " + " ".join(
                f"public int M{k}(int x) => (x * {k + 1} + {j}) % 997;" for k in range(20)
            ) + " }" for j in range(300)
        )
        (template / f"P{i}/Class1.cs").write_text(source)
    for i in range(3):
        work = ROOT / f"work{i}"
        shutil.copytree(template, work)
        run(["restore", "Bench.sln", "--ignore-failed-sources"], work,
            BASE | {"DOTNET_PROCESSOR_COUNT": "2", "MSBUILDDISABLENODEREUSE": "1"})

budget = dict(DOTNET_PROCESSOR_COUNT="2", MSBUILDDISABLENODEREUSE="1",
              DOTNET_CLI_USE_MSBUILD_SERVER="0", UseSharedCompilation="false")
variants = [
    ("baseline", {}),
    ("maxcpucount_property", {"MSBuildMaxCpuCount": "2"}),
    ("build_in_parallel_property", {"BuildInParallel": "false"}),
    ("processor_count", {"DOTNET_PROCESSOR_COUNT": "2"}),
    ("no_node_reuse", {"MSBUILDDISABLENODEREUSE": "1"}),
    ("no_shared_compiler", {"UseSharedCompilation": "false"}),
    ("no_msbuild_server", {"DOTNET_CLI_USE_MSBUILD_SERVER": "0"}),
    ("msbuild_server_enabled", {"DOTNET_CLI_USE_MSBUILD_SERVER": "1"}),
    ("msbuild_server_without_node_reuse", {"DOTNET_CLI_USE_MSBUILD_SERVER": "1", "MSBUILDDISABLENODEREUSE": "1"}),
    ("budget", budget),
    ("budget_five_cpus", budget | {"DOTNET_PROCESSOR_COUNT": "5"}),
]
results = [measure(label, env) for label, env in variants if not options.only or label in options.only]
(ROOT / ("results-" + "-".join(options.only) + ".json" if options.only else "results.json")).write_text(
    json.dumps(results, indent=2) + "\n")
