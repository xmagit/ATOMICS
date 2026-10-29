# Linux platform audit

Written by GPT-6 Codex, 2026-10-29.

| Assumption | Linux behavior / compatibility |
| --- | --- |
| `~/Applications`, `.app`, `sips`, `iconutil`, `codesign` | `macos-app.sh` explicitly requires Darwin. `linux-install.sh` builds a native release, atomically installs `~/.local/bin/imacs`, and installs `imacs.desktop`, a scalable icon and a 512px PNG under XDG data storage. Repeated install/uninstall is supported; uninstall keeps state/config. |
| State in `~/.imacs` | Existing directories and symlinks remain authoritative, including offline links. New Linux installs use `$XDG_STATE_HOME/imacs`, defaulting to `~/.local/state/imacs`. macOS keeps its current path. The earlier `.cox` migration remains available. |
| Socket beside state | New Linux installs use a private, owned `$XDG_RUNTIME_DIR/imacs/<home hash>/imacs.sock`; missing, relative, public or unavailable runtime directories fall back to state storage. Existing installs and explicit `IMACS_HOME` retain their socket paths. `IMACS_SOCKET` wins. New socket files have mode 0600. |
| Jobserver under a macOS temporary path | Linux uses the same per-home XDG runtime directory when valid. Otherwise, both platforms retain the existing `std::env::temp_dir()/imacs-<uid>/<home hash>` fallback. No `/private/tmp` dependency. |
| FIFO creation and permissions | `mkfifo(0600)` and read/write opening work on Linux and macOS. Pool directories are created with mode 0700; symlink/foreign-owner checks remain. Existing ancestor permissions are no longer changed. Tests consume tokens and reset leaks, check mode/type and ancestor preservation. Only `CARGO_MAKEFLAGS` carries the FIFO; older GNU make and Apple's make must not inherit it via `MAKEFLAGS`. |
| Sandbox access to `/run/user` | When a FIFO lives in XDG runtime storage, Codex gets `--add-dir` for that instance's private runtime directory so sandboxed compilers can open it. This preserves any user-supplied extra directories. App tests use a private temporary XDG runtime directory to exercise this path on Linux. |
| `ps`, `nice`, load average | `ps -axo pid=,ppid=` (ancestry), `ps -axo args=` (compiler detection), `ps -o nice= -p` (tests), `nice -n` and libc `getloadavg` work on procps Linux and macOS. A failed compiler scan conservatively prevents FIFO refill. Real-parent ancestry and inherited niceness have tests. |
| `/bin/zsh` fallback | `$SHELL` is used when nonempty. Without it, Linux uses `/bin/sh`; macOS keeps `/bin/zsh`. Both login-environment discovery and shell terminals use this choice. |
| Microphone privacy settings | macOS silence errors keep the System Settings microphone hint; Linux points to the default input, mute and desktop audio permissions. Audio uses cpal's platform backend (ALSA on Linux). |
| `open` and bundle restart | Files use `xdg-open` on Linux. Bundle-specific `open -n -a` is only used on macOS; Linux restarts the executable directly. Linux's `/proc/self/exe` deleted-inode suffix after an atomic update is removed when finding the installed replacement, including for hooks and agent launch. A Linux subprocess test replaces its own executable and validates the restart target. |
| App-bundle screenshot executable | `--screenshot` captures through eframe from the same CLI binary on Linux. The Linux Docker harness supplies an isolated HOME/config/state/socket and a virtual display; no app-bundle screenshot path is needed. |
| Window system and fonts | eframe 0.37 enables Wayland, X11, wgpu, system fonts and embedded default fonts. Wayland/X11/default-font features are explicit in the app dependency. No Apple font path is used. Linux desktop identity is `imacs`, matching `imacs.desktop` and its icon. |
| Command shortcuts | The existing parser maps `cmd` to Ctrl+Shift on Linux, applying Shift once. Plain Ctrl stays available to terminals. UI hints now format the same parsed chords as the handler, including configured editor shortcuts. The README gives Linux examples. |
| APFS cache clones | macOS retains native `clonefile`; Linux uses GNU `cp -R --reflink=always`. btrfs and reflink-enabled XFS can clone on the same filesystem; ordinary ext4, disabled reflinks and cross-filesystem copies fail open to a cold worktree. Staging removes partial clones and there is no full-copy fallback. Native Linux tests verify either independent cloned files/symlinks or a clean skip. Set `IMACS_COW_TEST_DIR` to repeat the native test on a mounted filesystem. |
| macOS-only warm-worktree tests | Clone, doctor and launch-order tests now run on Linux too, account for unsupported reflinks and use an uncloneable socket to force failure even when QA runs as root. |
| QA scripts and old paths | Both .NET budget scripts already use `tempfile` and compatible `ps` flags; they now find a system .NET installation when `~/.dotnet/dotnet` is absent (or honor `DOTNET`). They explain a missing SDK. Docs give Linux `ps eww -p <pid>` syntax. Literal macOS paths in captured fixtures, quoting/parser tests and historical QA evidence remain as data; none are opened as live QA paths. Heartbeat's `date` test already selects BSD `-r` versus GNU `-d`. |

For fresh Debian/Ubuntu installations, the README lists build libraries, Mesa runtime support and desktop portal/`xdg-open` integration. CI already runs Linux by default with its native library dependencies; macOS remains an opt-in runner.

## Validation

The focused native checks cover state fallback/migration, clone cleanup/independence, platform shortcuts and FIFO behavior. Linux-specific XDG tests run as subprocesses with temporary homes, independent state/config/runtime overrides, and invalid runtime cases. The executable-update test runs a copied test executable, replaces it atomically and validates the replacement.

Final build, screenshot and installer lifecycle results are recorded below after running the shared Linux QA harness.
