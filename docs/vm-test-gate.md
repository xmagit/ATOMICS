# Workspace gate on the NixOS guest

Run `scripts/check.sh` from the checkout. It acquires an ATOMICS test slot when the CLI is available,
enters the default Nix development shell, and runs formatting, Clippy with warnings denied, and
the locked workspace test suite with `--no-fail-fast`.
The CLI is used whenever `atomics` is on PATH; without a reachable host, it runs the command without
a slot. This native workspace gate does not include CI's browser checks and Clippy for
`wasm32-unknown-unknown`, or `python3 tests/test_release_signing.py`.

Real-binary fixtures may set `ATOMICS_CREDENTIALS_DIR` on macOS only in debug builds; macOS release builds ignore it and always use the OS keychain. Linux's explicit directory behavior is unchanged.

## Historical baseline

These are the historical results supplied for this audit, from separate runs. They are not a
new baseline run of this merged workspace, and do not establish a total workspace pass count.

| Scope | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| `atomics` binary tests | 1183 | 35 | 3 |
| `headless_host` integration target | 17 | 1 | 0 |
| Installer tests on this guest | not recorded | 8 | not recorded |

The old development shell also selected a system Clippy driver built with LLVM 23 alongside its
Nix rustc built with LLVM 21. Both reported Rust 1.99.0, but their crate metadata was incompatible
(`E0514`). The shell now supplies rustc, Cargo, Clippy and rustfmt from the pinned nixpkgs toolchain.

## Machine dependencies removed

- Executed `cat`, `sleep` and `true` helpers resolve on PATH. Fixture helper paths include the Nix
  profile and store paths instead of assuming `/usr/bin` and `/bin` contain the tools.
- Setup, doctor and Azure discovery fixtures expose only their selected programs. Installed
  agents and editors cannot satisfy an intended missing-program assertion.
- App and headless fixtures use a shell that preserves their PATH and reads no startup files.
  The fake sandbox executes its command without login mode: even `--noprofile` alone permits a
  noninteractive login shell's explicit `exit` to run logout hooks and append title escapes.
  Production still uses `bash -lc` inside the sandbox image; the fixture excludes the host's
  startup and logout files from the image it models.
- The macOS placement test explicitly exercises both system-bundle states, independently of
  whether `/Applications/ATOMICS.app` exists on the guest.
- Installer fixtures name the supported x86_64 Linux release target and stub `uname`. The real
  host binary still runs, and updater fixtures still select its compiled target. Signature,
  checksum, version, update and rollback assertions remain in place. Release support is unchanged.

The desktop-typing PTY fixture used an absent `/bin/cat`. The selection fixture also used an
absent `/bin/sleep` to keep its PTY alive after producing the scroll output. Those helpers now
resolve on PATH. Terminal shutdown had a separate runtime bug: its fixed `/bin/ps` lookup failed
on NixOS, preventing discovery of workers in separate job-control groups. It now tries `/bin/ps`,
`/usr/bin/ps` and `/run/current-system/sw/bin/ps` before absolute PATH directories, ignoring relative
and empty entries. Its regression test closes and replaces terminals with a PATH lacking `ps` and
a fake `ps` in the working directory, with and without job control.
The focused terminal run passed all 42 checks, including typing, selection after scrolling,
and closing/replacing terminals with and without job control; its existing benchmark stayed ignored.

The preview integration check waits for a stopped listener to release its socket, including a
worker blocked in `poll`, and still requires that it stop accepting connections within its deadline.

## Initial audit validation on the guest

The authorized final rerun of `scripts/check.sh` on the aarch64 NixOS guest on 2026-11-10
exited successfully: **2175 passed, 0 failed, 42 ignored**, across all 21 target summaries,
including doc tests. Formatting and `cargo clippy --workspace --all-targets --locked -- -D warnings`
also passed inside the default Nix development shell.

The first queued full gate passed formatting and Clippy, then completed every workspace test target:
2174 passed, 1 failed, 42 ignored. The only failure was another fixed-directory helper search in
the `linux_install` target's no-Rust fixture. It now links only `sh`, `uname` and `dirname` resolved
from PATH, still excluding Rust and still checking the installer's missing-toolchain guidance.
After that correction, all 13 `linux_install` tests passed, and formatting and workspace Clippy
were clean again. The 42 ignored tests are the existing benchmarks and browser checks.

The final full run's previously failing scopes were green:

| Scope | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| `atomics` binary tests | 1232 | 0 | 3 |
| `headless_host` integration target | 18 | 0 | 0 |
| `install_and_update` integration target | 12 | 0 | 0 |
| `linux_install` integration target | 13 | 0 | 0 |
| Entire workspace | 2175 | 0 | 42 |

The separate `vm_update` integration target is macOS-only and runs no tests on this Linux guest.

## Round-4 review follow-up

After merging the reviewed integration tip `73277b6`, the Azure sandbox fixture persists its
isolation settings before starting sessions. Each enabled launch forces a real config-file reload,
so the test exercises that reload without relying on the one-second polling schedule. Missing
relay grants and Azure placeholders now identify the agent and expectation in their failure messages.

Persistence alone still reproduced the failure after five passes: the fake `sbx run` exits
immediately by default, and runtime exit handling can remove its grant before the test inspects it.
The fixture now keeps its agents attached until `close`, preserving the disabled-Azure checks,
the environment checks for both providers, and revocation on close. The corrected test passed
30 consecutive runs, including 60 forced reloads across the enabled launches.

The Azure approval, bare Claude relay, shared subscription and sandbox/MCP connection fixtures also
persist their settings before asynchronous work. Nine related targeted checks passed, including
terminal cleanup with no `ps` on PATH and a fake `./ps` that must remain unexecuted.

The single queued `scripts/check.sh` run for this follow-up passed formatting and workspace Clippy.
The full suite finished with **2239 passed, 1 failed, 42 ignored** (exit 101). The Azure regression
passed. The only failure was
`relay::tests::audit_p7_idle_and_trickling_clients_have_separate_short_auth_slots`: its temporary
directory cleanup found a recreated `relay.log` (`Directory not empty`). That p7 teardown failure
was reported separately; this follow-up does not claim a green full workspace run.
