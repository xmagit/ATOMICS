# Workspace gate on the NixOS guest

Run `scripts/check.sh` from the checkout. It acquires an ATOMICS test slot when the CLI is available,
enters the default Nix development shell, and runs formatting, Clippy with warnings denied, and
the locked workspace test suite with `--no-fail-fast`.
The CLI is used whenever `atomics` is on PATH; without a reachable host, it runs the command without
a slot. This native workspace gate does not include CI's browser checks and Clippy for
`wasm32-unknown-unknown`, or `python3 tests/test_release_signing.py`.

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

## Tests on a loaded machine

Wait for a fixture to report readiness with a channel, socket or file before exercising its
behavior. A sleep does not establish readiness. Keep a deliberately hung fake blocked until
it is released or killed; a finite sleep can finish while the test is descheduled. Inject time
when checking a deadline, cache lifetime or backoff, and advance it explicitly across the boundary.
Keep generous, load-scaled hang guards for fixture startup and teardown separate from those checks.

Write executable fixtures with `test_support::write_program` or `copy_program`. An unrelated
parallel fork can inherit a writable descriptor and make execution fail with `ETXTBSY` until exec.
It can also inherit a held `flock`, keeping the lock alive after its test drops it; run assertions
about releasing and immediately reacquiring a lock in an isolated test child, and finish any
intentional contender before dropping the owner. Persist configuration before asynchronous work
that may reload it, and stop or join fixture workers before removing their files.

Repeated full runs measure intermittent failures. Record every failed run and fix the fixture's
ordering or clock; retrying a failed test does not make the gate green.

Cloud and agent programs require explicit fixtures. Shared child support supplies a PATH of
ordinary helper links and keeps HOME, AZURE_CONFIG_DIR, GH_CONFIG_DIR, LIMA_HOME and agent state
inside the fixture. It preserves fixture-only and intentionally empty PATH overrides. Unit-test
lookup and launch guards reject `az`, `gh`, `sbx`, `claude`, `codex` and `limactl` outside a registered
fixture, including a symlink pointing outside one. Login-shell probes may read only fixture scripts.
Production binaries launched by integration tests receive the same isolated environment from
the child support code, since those binaries do not contain the unit-test guards.

This closes the inherited PATH in every shared app fixture (including
`app::key_tests::escape_closes_settings_and_the_project_picker_and_the_terminal_sees_neither`)
and in the headless, updater, installer, statusline and doctor child fixtures. The worktree launch
test now uses fixture directories for its original and login PATHs while still checking that both
original directories survive the merge. Azure, GitHub token, sandbox, agent terminal and Lima
launches also check explicitly supplied program paths. No cloud or agent CLI is a permitted helper.

The isolation audit required explicit changes to these fixtures:

- `sandbox::checks::tests::mcp_checks_registration_and_authorization_without_starting_oauth`,
  `sandbox::checks::tests::ssh_warns_for_atomics_forwarding_global_forwarding_or_a_fixed_socket`,
  and `sandbox::tests::preflight_refuses_a_missing_old_or_signed_out_sbx_with_what_to_do`
  consulted the host PATH. They now run in an isolated child.
- `app::approvals::tests::hidden_permission_commands_expand_before_approval_and_running_clears_needs_input`,
  `app::host_tests::held_terminal_input_gets_a_receipt_without_frames`,
  `app::sessions::tests::busy_codex_sends_wait_for_receipts_retry_and_report_failure`,
  `app::sessions::tests::codex_clipped_and_repeated_receipts_never_repaste`, and
  `app::sessions::tests::steering_during_a_pending_codex_send_is_held_and_released`
  now copy the repository's fake Codex into their own directory before executing it.
- `app::setup_tests::settings_runs_every_sandbox_step_with_clicks_and_reads_back_what_is_left`
  now supplies fake sbx, agents and keychain discovery before the app starts.
  The window login-PATH and worktree dispatch tests use fixture paths throughout.
  The sandbox crash child also receives its fixture HOME.
- Direct Azure, GitHub token and sandbox CLI fixtures receive isolated state directories.
  The Azure no-token-on-disk check now scans descendant files too, without following helper
  symlinks into the host's tools.

## Loaded-machine measurements

The ten-run before measurement used frozen revision `2dc9614` and
`cargo test --workspace --no-fail-fast --locked`, through one ATOMICS test queue slot.
Nine runs passed 2247 tests; run 5 passed 2246 and failed the ticket timeout assertion.
Every run had 42 ignored tests. Start-of-run one-minute load averages ranged from 5.04 to 15.85
on 14 available CPUs. The initial ENOSPC build attempt executed no tests and is excluded.
Both measured phases disable Cargo incremental compilation to avoid regrowing its disk cache;
the shared build jobserver and normal parallel test harness remain enabled.

| Test | Failures in 10 before runs | Other agents' reported failures | Failures in final 10 runs |
| --- | ---: | ---: | ---: |
| `machine::docker::tests::missing_failing_and_timed_out_docker_have_distinct_status` | 0 | 1 | 0 |
| `app::tickets::tests::failures_show_on_the_link_and_never_block_the_plan` | 1 | 1 | 0 |
| `update::tests::a_crash_leaves_nothing_the_next_update_keeps_and_only_one_update_runs_at_a_time` | 0 | 1 | 0 |
| `app::key_tests::escape_closes_settings_and_the_project_picker_and_the_terminal_sees_neither` | 0 | 1 | 0 |
| `relay::tests::audit_p7_idle_and_trickling_clients_have_separate_short_auth_slots` | 0 | 1 in the previous gate | 0 |
| `ipc::tests::audit_p7_stale_socket_recovery_has_one_owner_and_cleanup_keeps_a_replacement` | 0 | 1 | 0 |

The external reports have no shared denominator and are not added to the ten-run rates.
The ticket assertion was the only failure observed in those ten runs. The relay test belongs to
p7; its integration changes are merged before the after measurement. Docker, ticket and updater
regressions each passed 30 consecutive targeted runs with the corrected fixtures.

The separate CLI-isolation migration checks exposed fixture assumptions (missing ordinary
helpers, repository fakes outside their owner directory, and host PATH lookups). Their failures
are retained in `/tmp/p3-isolation-binary.log` and `/tmp/p3-isolation-binary-fixed.log` and are not
represented as intermittent failures in the frozen before measurement.

The first merged stress batch at `fca1dcf` completed all ten runs with **2308 passed,
11 failed, 42 ignored** each time. Formatting and Clippy also passed in its final gate run.
All reported tests in the table above passed in all ten runs. The isolation
support exposed the following additional fixture failures, each **10/10**; none failed in the
earlier frozen baseline:

| Target | Test | Failures in first merged batch | Failures in final 10 runs |
| --- | --- | ---: | ---: |
| `doctor` | `setup_from_an_empty_machine_creates_config_and_gives_install_commands_without_secret_arguments` | 10 | 0 |
| `doctor` | `doctor_says_where_path_came_from_and_reads_the_codex_version_with_the_login_shells` | 10 | 0 |
| `linux_install` | `a_build_that_does_not_run_replaces_nothing_and_no_rust_says_what_to_do` | 10 | 0 |
| `linux_install` | `install_and_update_ask_nothing_and_replace_the_program_atomically` | 10 | 0 |
| `linux_install` | `installer_registers_the_atomics_link_scheme` | 10 | 0 |
| `linux_install` | `setup_finds_what_an_installer_left_in_the_users_own_programs_folder` | 10 | 0 |
| `linux_install` | `setup_with_agents_on_this_machine_needs_one_agent_and_nothing_of_sandboxes` | 10 | 0 |
| `linux_install` | `setup_with_sandboxes_in_use_still_checks_what_they_need` | 10 | 0 |
| `linux_install` | `stopping_setup_reads_as_a_stop_and_it_can_still_ask_for_sandboxes` | 10 | 0 |
| `linux_install` | `the_script_and_the_program_write_the_same_menu_entry` | 10 | 0 |
| `atomics-core` lib | `paths::tests::linux_xdg_paths_and_instance_overrides` | 10 | 0 |

Installer and doctor fixtures now register their temporary directories with the shared support.
The doctor PATH test uses fixture directories while still requiring exactly one added directory
and a successful Codex version probe. Deliberately relative or empty XDG values remain available
to the path-resolution tests: the XDG rules ignore them in favor of the isolated HOME.
Cloud and agent state directories still require fixture paths.

The next batch, at `1468152`, passed all ten runs with **2319 passed, 0 failed, 42 ignored**
each, including the complete gate in run 10. Review then found a remaining real deadline in
the Docker success fixture and an elapsed-time cache assertion. Commit `086d912` freezes the
success and failure command clocks too; the cache fixture checks reuse at 9.999 seconds and
refresh at exactly 10 seconds. Both Docker regressions passed 30 consecutive targeted runs.
Only separate fixture hang guards use real time.

The following ten runs at `086d912` reached a sampled one-minute load of 25.07 on 14 CPUs.
Eight were green; two each passed 2318 and failed one test, with 42 ignored in every run.
These additional intermittent failures were absent from the frozen before measurement and
the earlier green batch:

| Later observed test | Failures in 10 runs at `086d912` | Failures in final 10 runs |
| --- | ---: | ---: |
| `app::github::tests::github_connect_uses_gh_after_one_confirmation` | 1 | 0 |
| `credentials::tests::credentials_relay_rules_restrict_hosts_and_capabilities_and_track_usage` | 1 | 0 |

The GitHub fixture now waits for the applied disconnect result, including its revoke action.
A poll can clear the viewer after keychain deletion before that result arrives; an absent viewer
alone is not readiness. The same predicate covers the related device-flow reconnect fixture.
The credentials fixture waits for all request logs, then shuts down and joins the service before
inspecting the usage file: logging and usage persistence use different workers. It also verifies
that the persisted timestamp equals the in-memory usage timestamp. Existing capability,
revocation, browser and secret-redaction checks remain in place.

The two GitHub connect fixtures and the credentials usage fixture each passed 30 consecutive
targeted runs after these completion barriers were corrected.

Another reviewer reported the stale-socket recovery test failing once (zero successful binders
instead of one), although it passed throughout this worktree's measured batches. Creating and
closing its initial listener in the parallel harness lets an unrelated fork inherit the descriptor;
until exec, that process can keep the supposed stale socket listening. The fixture now creates
the socket in a child and waits for that child's exit before starting its two simultaneous recovery
attempts. The one-owner check, refusal to steal a live listener, replacement-inode cleanup and
regular-file preservation checks are retained. The ownership assertion also prints both bind results.
The corrected stale-socket test passed 30 consecutive targeted runs.

The final measurement at `33855a6`, after bringing in integration tip `0422626`,
passed **all ten consecutive full workspace runs**: **2319 passed, 0 failed, 42 ignored** in each.
There were no source changes during the batch. The tenth run executed the complete
`scripts/check.sh --run-gate` inside the held ATOMICS test slot and Nix shell; formatting and
workspace Clippy with warnings denied passed too. No failed test was retried within a run.
The shared jobserver and ordinary parallel test harness remained enabled on 14 available CPUs.
Start-of-run one-minute loads ranged from 0.96 to 6.92; once-per-second
samples reached 8.40. All original reported regressions, all eleven fixture failures,
and the later GitHub/usage-writer races above measured **0/10 failures** in the final batch.

The complete run commands, revisions, exit codes, counts, loads and per-test failures are retained in:

- Before: `/tmp/p3-reliability-before-nuvs90t2/summary.json`.
- First merged batch: `/tmp/p3-reliability-after-ys2up5wt/summary.json`.
- Earlier green batch: `/tmp/p3-reliability-final-kn4m490o/summary.json`.
- Further loaded batch: `/tmp/p3-reliability-final_source-klgom9d5/summary.json`.
- Final batch: `/tmp/p3-reliability-verified_source-dmrieyy_/summary.json`.

Final native target counts at the measured revision:

| Scope | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| `atomics` binary tests | 1313 | 0 | 3 |
| `headless_host` integration target | 23 | 0 | 0 |
| `install_and_update` integration target | 13 | 0 | 0 |
| `linux_install` integration target | 14 | 0 | 0 |
| Entire native workspace | 2319 | 0 | 42 |
