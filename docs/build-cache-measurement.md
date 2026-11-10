# Build cache measurement

Measured on 2026-11-10 with two fresh linked worktrees per series, using the pinned Nix shell,
Rust 1.99.0 and sccache 0.19.0. Each worktree built `cargo build --locked -p atomics-core` once,
through one ATOMICS test queue slot. The source revision was `81618b7` (the required round-4 baseline).
The package scope keeps this experiment small on the shared machine; these are not whole-workspace numbers.

| Setting | First worktree | Second worktree | Combined wall time | Worktree outputs | Shared compiler cache | Total disk |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Off: incremental, full debug, no wrapper | 128.09 s | 116.10 s | 244.19 s | 1.805 GiB | 0 | 1.805 GiB |
| On: no incremental, line tables, sccache | 67.06 s | 24.02 s | 91.08 s | 0.668 GiB | 0.103 GiB | 0.771 GiB |

The enabled series used an initially empty, private sccache store and distinct target directories.
It recorded 79 Rust cache hits plus 24 C/assembler hits, with no cache read/write errors or timeouts.
The second worktree's first build reused dependency compilations without a shared Cargo target lock.
Including the compiler store, total allocated disk fell 57.3%; combined wall time fell 62.7%.
Lean settings account for much of the output reduction; sccache adds reuse across worktrees.
Linked binaries, build scripts and procedural macros can still require work in each checkout.
Shared machine load and already-shared package downloads affect timing.

The [JSON report](build-cache-measurement.json) preserves byte counts and sccache statistics.
The experiment checked 221.38 GiB free before starting and stayed above the 40 GiB floor.
Its four temporary worktrees, private compiler store and private server were removed afterward.
The machine also ran unrelated work, so before/after filesystem free space is not attributed to this feature.

Reproduce from the repository root (Python is in the dev shell):

```sh
nix --extra-experimental-features 'nix-command flakes' develop .#default --command \
  python3 scripts/measure-build-caches.py --count 2
```

The script acquires a test queue slot, builds sequentially, measures allocated bytes with `du`, and
writes logs plus JSON under `target/`. It checks free space before every build and during builds,
stopping if it falls below 40 GiB. `--package` selects another workspace package; `--count` accepts 1–4.
Its isolated sccache server uses a short temporary Unix socket path and never resets another agent's cache.
