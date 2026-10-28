#!/usr/bin/env python3
"""Run through `imacs queue --class test -- python3 scripts/dotnet-test-budget-probe.py`.

Measure default NUnit/xUnit workers using sleeping tests in independent fixtures.
Only generated projects in a temporary directory are built or tested.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import time


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--reuse-root", type=Path, help="only probe VSTest using earlier generated projects")
options = parser.parse_args()
root = (options.reuse_root or Path(tempfile.mkdtemp(prefix="imacs-dotnet-tests-"))).resolve()
dotnet = str(Path.home() / ".dotnet/dotnet")
env = {k: v for k, v in os.environ.items() if k != "DOTNET_PROCESSOR_COUNT"}
env.update(MSBUILDDISABLENODEREUSE="1", UseSharedCompilation="false",
           DOTNET_CLI_USE_MSBUILD_SERVER="0", DOTNET_CLI_TELEMETRY_OPTOUT="1")
print(f"Generated projects and logs: {root}", flush=True)


def run(args, cwd, variables, log):
    with log.open("w") as output:
        subprocess.run([dotnet, *args], cwd=cwd, env=variables,
                       stdout=output, stderr=subprocess.STDOUT, check=True)


for framework in ([] if options.reuse_root else ["nunit", "xunit"]):
    project = root / framework
    project.mkdir()
    packages = ('<PackageReference Include="NUnit" Version="4.4.0" />'
                '<PackageReference Include="NUnit3TestAdapter" Version="5.2.0" />'
                if framework == "nunit" else
                '<PackageReference Include="xunit" Version="2.9.2" />'
                '<PackageReference Include="xunit.runner.visualstudio" Version="2.8.2" />')
    (project / "Probe.csproj").write_text(
        '<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup>'
        '<TargetFramework>net9.0</TargetFramework><IsTestProject>true</IsTestProject>'
        '</PropertyGroup><ItemGroup>'
        '<PackageReference Include="Microsoft.NET.Test.Sdk" Version="17.11.1" />'
        + packages + '</ItemGroup></Project>')
    (project / "global.json").write_text('{"sdk":{"version":"9.0.313"}}\n')
    source = '''using System;
using System.IO;
using System.Threading;
public static class Probe {
    static readonly object Gate = new object();
    static int active, peak;
    public static void Work() {
        lock (Gate) { active++; peak = Math.Max(peak, active); }
        Thread.Sleep(200);
        lock (Gate) {
            active--;
            File.WriteAllText(Environment.GetEnvironmentVariable("IMACS_PROBE_OUTPUT"),
                $"{Environment.ProcessorCount},{peak}");
        }
    }
}
'''
    for i in range(36):
        if framework == "nunit":
            source += (f'[NUnit.Framework.TestFixture, NUnit.Framework.Parallelizable(NUnit.Framework.ParallelScope.All)] '
                       f'public class C{i} {{ [NUnit.Framework.Test] public void Test() => Probe.Work(); }}\n')
        else:
            source += f'public class C{i} {{ [Xunit.Fact] public void Test() => Probe.Work(); }}\n'
    (project / "Tests.cs").write_text(source)
    run(["build", "-v:q", "--ignore-failed-sources"], project,
        env | {"DOTNET_PROCESSOR_COUNT": "2"}, project / "build.log")
    for processors in [None, "5", "2", "1"]:
        output = project / f"workers-{processors}.txt"
        variables = env | {"IMACS_PROBE_OUTPUT": str(output)}
        if processors:
            variables["DOTNET_PROCESSOR_COUNT"] = processors
        start = time.monotonic()
        run(["test", "--no-build", "--no-restore", "-v:n"], project,
            variables, project / f"test-{processors}.log")
        cpu, peak = map(int, output.read_text().split(","))
        print(json.dumps(dict(framework=framework, processor_count=processors,
                              reported_cpus=cpu, peak_workers=peak,
                              seconds=round(time.monotonic() - start, 2))), flush=True)

# Independent source paths let VSTest start a host per assembly. Discover without
# executing the sleeping tests: this measures host scheduling, not framework workers.
assemblies = []
for i in range(8):
    destination = root / f"assembly{i}"
    shutil.copytree(root / "nunit/bin/Debug/net9.0", destination, dirs_exist_ok=True)
    assemblies.append(str(destination / "Probe.dll"))
for parallel, processors in [(False, None), (True, None), (True, "2")]:
    variables = env.copy()
    if processors:
        variables["DOTNET_PROCESSOR_COUNT"] = processors
    label = f"vstest-{parallel}-{processors}"
    with (root / f"{label}.log").open("w") as log:
        command = [dotnet, "vstest", *assemblies, "/TestCaseFilter:FullyQualifiedName~NoSuchFixture"]
        if parallel:
            command.append("/Parallel")
        start = time.monotonic()
        child = subprocess.Popen(command, cwd=root / "nunit", env=variables, stdout=log, stderr=subprocess.STDOUT)
        peak = 0
        known = {child.pid}
        while True:
            rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,args="], text=True)
            processes = [line.strip().split(None, 2) for line in rows.splitlines()]
            for _ in range(3):
                known.update(int(pid) for pid, parent, _ in processes if int(parent) in known)
            hosts = sum(int(pid) in known and "testhost.dll" in args for pid, _, args in processes)
            peak = max(peak, hosts)
            if child.poll() is not None:
                break
            time.sleep(0.05)
        if child.returncode:
            raise RuntimeError(f"VSTest failed: see {root}/{label}.log")
        print(json.dumps(dict(framework="vstest", parallel=parallel, processor_count=processors,
                              peak_hosts=peak, seconds=round(time.monotonic() - start, 2))), flush=True)
