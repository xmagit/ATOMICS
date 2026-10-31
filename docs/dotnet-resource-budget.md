# .NET resource budget measurements

Measured on 2026-10-28 on this shared 18-core macOS arm64 machine, SDK 9.0.313,
MSBuild 17.14.43. These are single-run observations while other agents were active,
not a controlled throughput benchmark.

## Three concurrent builds

`scripts/dotnet-budget-bench.py` uses `dotnet new sln` and eight `dotnet new classlib`
projects, each with 300 generated classes and 20 methods per class. The projects
are independent so all eight can compile concurrently. Three copies have separate
output directories. Restore finishes before timing; each timed command is
`dotnet build Bench.sln --no-restore -t:Rebuild -v:q`. Wall time ends when all three
commands finish. `ps` samples their descendants every 50 ms; short-lived processes
between samples may be missed. The total includes the three `dotnet build` drivers;
the MSBuild column counts processes running `MSBuild.dll`, including any server.
Column peaks are sampled independently.

| Environment change from baseline | Peak .NET total | Peak MSBuild | Peak VBCSCompiler | Peak csc | Wall seconds | MSBuild / VBCS retained after 1 s |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| None | 25 | 21 | 1 | 0 | 5.18 | 0 / 1 |
| `MSBuildMaxCpuCount=2` | 25 | 21 | 1 | 0 | 4.13 | 0 / 1 |
| `BuildInParallel=false` | 25 | 21 | 1 | 0 | 3.73 | 0 / 1 |
| `DOTNET_PROCESSOR_COUNT=2` | 7 | 3 | 1 | 0 | 8.26 | 0 / 1 |
| `MSBUILDDISABLENODEREUSE=1` | 25 | 21 | 1 | 0 | 4.33 | 0 / 1 |
| `UseSharedCompilation=false` | 48 | 21 | 0 | 24 | 5.22 | 0 / 0 |
| `DOTNET_CLI_USE_MSBUILD_SERVER=0` | 25 | 21 | 1 | 0 | 8.31 | 0 / 1 |
| Opt in: `DOTNET_CLI_USE_MSBUILD_SERVER=1` | 12 | 8 | 1 | 0 | 5.88 | 1 / 1 |
| Opt in plus `MSBUILDDISABLENODEREUSE=1` | 25 | 21 | 1 | 0 | 14.41 | 0 / 1 |
| **Combined budget, 2 CPUs** | **12** | **3** | **0** | **6** | **6.49** | **0 / 0** |
| Combined budget, 5 CPUs | 30 | 12 | 0 | 15 | 4.86 | 0 / 0 |

The combined budget sets `DOTNET_PROCESSOR_COUNT`, `MSBUILDDISABLENODEREUSE=1`,
`DOTNET_CLI_USE_MSBUILD_SERVER=0` and `UseSharedCompilation=false`. Default reported
CPUs are `max(1, jobs / test_slots / 2)`: two on this machine. Accounting for both
MSBuild nodes and separate compiler processes avoids the five-CPU candidate's
larger process peak. This is a cooperative per-process limit; it is not cargo's
shared token pool or an OS CPU quota.

`MSBuildMaxCpuCount` is not an environment equivalent of `-m:N`.
`BuildInParallel=false` did not reduce this solution's process peak either. Neither
is exported. MSBuild imports environment variables as project properties, but
switches and project-defined properties have different precedence. The effective
mechanism here is the runtime's reported processor count, which the CLI's bare
`-maxcpucount` uses. Explicit `-m:N` can override it.
See [MSBuild environment properties](https://learn.microsoft.com/en-us/visualstudio/msbuild/how-to-use-environment-variables-in-a-build)
and [parallel MSBuild](https://learn.microsoft.com/en-us/visualstudio/msbuild/building-multiple-projects-in-parallel-with-msbuild).

Disabling shared compilation alone **increases** concurrent processes: it moves
compiler work out of VBCSCompiler into standalone csc processes. The CPU setting
must accompany it. `UseSharedCompilation` and `SharedCompilationId` are passed to
the Csc task in SDK 9's `Roslyn/Microsoft.CSharp.Core.targets`. A project can
explicitly override the imported `UseSharedCompilation` property.

Ordinary SDK 9 builds left no idle MSBuild nodes on this machine. The opt-in server
probe establishes that disabling node reuse also prevents the persistent MSBuild
server, and explicitly disabling the server covers an inherited opt-in. Both are
kept; neither limits active worker count by itself. `dotnet build-server shutdown`
is not a launch policy: it shuts down existing servers rather than preventing the
next build from starting them, and could affect another agent.
See [MSBuild environment controls](https://github.com/dotnet/msbuild/blob/main/documentation/wiki/MSBuild-Environment-Variables.md)
and [MSBuild server](https://learn.microsoft.com/en-us/visualstudio/msbuild/msbuild-server).

The benchmark isolates compiler servers with `SharedCompilationId` and MSBuild
nodes with `MSBUILDNODEHANDSHAKESALT`. It terminates only its own observed server
descendants after measuring retained processes. It never invokes global server
shutdown, edits SDK/NuGet settings, or changes an existing .NET repository.

## Test-run parallelism

`scripts/dotnet-test-budget-probe.py` generated 36 independent sleeping fixtures
per framework, using Microsoft.NET.Test.Sdk 17.11.1, NUnit 4.4.0 / adapter 5.2.0,
and xUnit 2.9.2 / VS adapter 2.8.2. A shared counter inside each test assembly
records peak concurrent test methods. Build/restore are outside these timings.

| Reported CPU setting | NUnit peak workers / seconds | xUnit peak workers / seconds |
| --- | --- | --- |
| Unset (18 CPUs) | 18 / 1.48 | 17 / 1.17 |
| 5 | 5 / 2.65 | 5 / 2.19 |
| **2 (default budget here)** | **2 / 4.60** | **2 / 4.99** |
| 1 | 2 / 4.35 | 1 / 8.89 |

VSTest was separately given eight copies of the NUnit assembly with a filter
matching no tests. This measures discovery/host scheduling without running fixture
code or conflating framework workers with host processes:

| VSTest mode | Reported CPUs | Peak testhost processes | Seconds |
| --- | ---: | ---: | ---: |
| Default | 18 | 1 | 9.47 |
| `/Parallel` | 18 | 8 | 1.49 |
| `/Parallel` | 2 | 2 | 3.01 |

`DOTNET_PROCESSOR_COUNT` affects VSTest's processor-based parallel mode and the
default NUnit/xUnit worker counts. NUnit's minimum is two workers; xUnit's measured
17 is below its 18-worker default ceiling. Explicit runsettings, assembly
attributes, runner arguments, per-project MSBuild settings, and custom test code
can override these defaults. VSTest's default is already one host; a runsettings
`MaxCpuCount` controls host parallelism separately from adapter workers.
See [runtime CPU setting](https://devblogs.microsoft.com/dotnet/announcing-net-6/),
[VSTest parallelism](https://learn.microsoft.com/en-us/dotnet/core/tools/dotnet-vstest),
[NUnit defaults](https://docs.nunit.org/articles/nunit/writing-tests/attributes/levelofparallelism.html)
and [xUnit defaults](https://xunit.net/docs/running-tests-in-parallel).

ATOMICS does not export guessed `MaxCpuCount`, `NumberOfTestWorkers` or
`MaxParallelThreads` environment properties, inject a `.runsettings` file, or
export `VSTestCLIRunSettings`. The latter is also repurposed as application
arguments by Microsoft.Testing.Platform, so injecting VSTest settings would break
some .NET projects. See [VSTest/MTP compatibility](https://learn.microsoft.com/en-us/dotnet/core/testing/unit-testing-with-dotnet-test).
Whole suites still use `atomics queue --class test -- dotnet test`; queue slots limit
simultaneous commands, while the inherited CPU budget limits processor-based
defaults within each command.

## Reproduce

From this worktree, outside a sandbox that blocks `ps` or the ATOMICS socket:

```sh
python3 scripts/dotnet-budget-bench.py
atomics queue --class test -- python3 scripts/dotnet-test-budget-probe.py
```

Both scripts leave generated projects and per-run logs in the temporary directory
they print. The build script also saves JSON results. They pin the SDK version
measured here, and perform ordinary restore/build operations only in those
temporary projects. Initial raw build logs/results were under
`$TMPDIR/atomics-dotnet-budget-5zumaqh1`; test logs were under
`$TMPDIR/atomics-dotnet-tests-yntfd3n4` (temporary evidence, not required to reproduce).
