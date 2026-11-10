<img src="assets/icon.png" alt="atomisk.ai icon" width="96">

# ATOMICS

[![CI](https://github.com/xmagit/ATOMICS/actions/workflows/ci.yml/badge.svg)](https://github.com/xmagit/ATOMICS/actions/workflows/ci.yml)

A lightweight, review-first workspace for coding agents, written in Rust. Run Claude Code,
Codex and shells side by side, each in its own git worktree. Click any agent's title to see its worktree's diff
against main, comment on lines, and send the review straight to the agent while it keeps working. Or let an
orchestrating agent plan the work, hand out tasks and review the results while you approve and steer.

An open-source project.

## Install

On Linux (x86_64) or a Mac (Apple Silicon), with no Rust and no checkout:

```sh
curl -fsSL https://raw.githubusercontent.com/xmagit/ATOMICS/main/scripts/install.sh | sh
```

It checks the download's SHA-256 and signature, installs ATOMICS for your user with its menu entry, and asks nothing.
Open it, and its first run opens a [setup screen](docs/how-it-works.md#first-run). A build you downloaded by hand offers
to install itself the same way, on that screen or with `atomics install`. After that, ATOMICS updates itself:
[Updates](docs/how-it-works.md#updates) says what it keeps and how to go back.

On an Apple Silicon Mac, agents can run in a [NixOS VM](docs/nixos-vm.md) while the window stays
native. With Lima 2.2 and Rosetta installed, `atomics vm setup` downloads this version's signed VM
image and configures it for your account; no Docker or Nix build is needed. Run the sign-ins it lists,
then `atomics vm config --apply` to connect the app. Defaults are cores minus four, half the Mac's
memory and a 100 GiB sparse disk; `--cpus`, `--memory-gib` and `--disk-gib` override them.

To build it instead, see [Run](#run). `rust-version` in `Cargo.toml` is the oldest Rust it builds on, and
`rust-toolchain.toml` is the one it is built and tested with; rustup picks that up by itself.

## Where things live

The command is `atomics`. `scripts/macos-app.sh` builds ATOMICS.app for the Dock; `scripts/linux-install.sh` installs
the Linux desktop app from a checkout. State stays in `~/.atomics` on macOS and existing installs; new Linux installs use
`${XDG_STATE_HOME:-~/.local/state}/atomics`. Config lives in `${XDG_CONFIG_HOME:-~/.config}/atomics`.

[How ATOMICS works](docs/how-it-works.md) is a one-page overview of sessions, the chief, plans, isolation,
the credential relay and cleanup. [Fresh start on Linux](docs/fresh-start-linux.md) takes a clean Ubuntu
desktop to a working chief with sandboxed agents, and [Moving to a new machine](docs/moving-machines.md)
says what to take along and what to set up again.

## What it does

- **Agents in real terminals.** Every agent runs its own TUI in an embedded terminal, so nothing is lost compared to
  running it yourself.
- **The diff against main, always.** The diff panel follows the active agent. It covers committed, staged, unstaged and
  untracked changes against the merge-base with `base_ref`, so main moving on never shows up as the agent's work. It
  updates live, with syntax highlighting and word-level emphasis.
- **Side by side when there is room.** A diff area at least 1100 points wide shows the old version left and the new one
  right, each edited line beside what it replaced. `diff_split` under `[layout]` picks `auto`, `always` or `never`; the
  **Side by side** toggle in the diff header overrides it until ATOMICS quits. Comments and double-click-to-edit work the same in
  both views.
- **Ungrouped sessions for everything else.** Research and small tasks need no project or board. **+ New** (`cmd+t`)
  starts a session in a scratch folder of its own under the state directory (`<state>/scratch/<id>`), in the
  **Ungrouped** tab, which holds every session outside a plan. It takes nothing from the focused tile: no folder, no
  title. Its first prompt names it (typed in the dialog, sent from the steer bar, or typed into its terminal); rename
  it from its tile title (double-click) or its sidebar menu. It follows the isolation settings like any agent, and
  the dialog can make one session bare or sandboxed. Each scratch folder is a git repository without commits, so the
  diff panel shows everything written there and review comments reach the agent. Closing the session deletes the
  folder and its sandbox; quitting ATOMICS ends scratch sessions for good, while `atomics restart` resumes them. Pick
  **Existing folder** or **New worktree** in the dialog to work in a project instead.
- **Projects and workspaces.** `atomics <folder>` opens the repository it is in, always through its main checkout, so a
  folder inside any worktree opens the same project. The explorer shows the main checkout's tracked files, with the
  changes of the worktree under review laid over them, plus each project's worktrees. **Project › Open project…**
  adds more repositories; save the set as a workspace and reopen it with `atomics --workspace <name>`.
- **Changed files beside the diff.** A "changed" column next to the diff/editor lists what the worktree changes. In
  diff mode a click jumps to the hunks; in editor mode it opens the file at its first change. Drag the divider
  between agents and the panel to resize it (double-click resets).
- **Editor mode.** `cmd+e` (or double-clicking a diff line) swaps the diff for a real editor, Helix by default, at
  that line. Inlay hints, code actions and go-to-definition come from your language servers; the **inlay hints**
  button toggles them. One editor runs per worktree and keeps its state while you switch back to the diff. Neovim
  works too; see `[editor]` in the config.
  When the configured program is `hx` and only `helix` is on PATH, editor launch and doctor use `helix`.
  Helix's gutter compares tracked files with HEAD; ATOMICS reviews against the merge-base. Committed branch
  changes and untracked files can therefore have no Helix marker. See [Helix diff gutters](docs/helix-diff-gutter.md).
- **Worktree clean-up.** **Clean up…** in the explorer checks every worktree for uncommitted work, merged branches
  and deleted remote branches (completed PRs). **select safe** picks the ones that can go. Removal never forces, so
  worktrees with uncommitted work are kept, and branches are deleted only when git considers them merged.
- **Files.** `cmd+p` searches every file of the project. A changed file jumps to its hunks; any other file opens
  read-only, and its lines take comments too.
- **Review is steering.** Click a diff line to comment. **Send review** delivers all comments to the agent as one message,
  each with its file:line and the quoted code.
- **Steer one, some or all.** The bar at the bottom types into the active agent, every agent, or a selection.
- **Status without asking.** Claude and Codex 0.165+ sessions report through hooks. Older Codex versions use their
  screen (`busy_text`). What needs you is counted in the top bar; `cmd+j` jumps to it.
- **Responsive.** On a wide screen, every agent gets a tile beside the diff (10 agents fit on a 5120 px ultrawide). As the window narrows, tiles collapse into
  tabs. Below that the diff stacks above the active agent, and at the smallest size the diff takes the whole area.
- **MCP connections that just work.** Settings → Connections imports the MCP servers Claude Code and Codex
  use (definitions only) and connects one with a click: sbx registers it and its sign-in opens in your
  browser. Every sandboxed session gets every connected server, and a tile whose agent needs a missing one
  offers **Connect <name>**. See [MCP connections](docs/isolation.md#mcp-connections).
- **GitHub without leaving.** A git icon in the top bar counts the open pull requests of the loaded repositories
  (yours by default). `Command+G` on macOS, `Ctrl+G` on Linux, opens the controls. They list each PR with
  its checks, review state and mergeability. For the selected PR you can open it in the browser, review its diff
  read-only in the diff viewer, merge it, or admin merge it when GitHub lets you bypass the rules. Workflows you
  configure run from there too. **Connect GitHub** reuses the `gh` CLI's account after one confirmation, else signs
  in through the browser. The token stays in the OS keychain and never reaches an agent. Azure DevOps repositories
  are listed in the same controls, grouped into those assigned to you and your own, and signed in with the Azure CLI.
  See [docs/github.md](docs/github.md).
- **Remote control and MCP.** Other agents can start, read, review and steer agents in ATOMICS (see below).

## Run

The first start of ATOMICS opens a setup screen: which agents it found, where they run, a first project and
connections, every one of which can wait. See [First run](docs/how-it-works.md#first-run). Agents run on this machine
under your own login until you choose sandboxes. **Settings → Agents** sets those up with clicks, and says the exact
command for what takes sudo. `atomics setup` does the same from a terminal, for what your configuration uses, and
resumes completed steps on rerun. See [fresh-machine setup](docs/isolation.md#fresh-machine-clicks-or-one-guided-command).

```sh
cargo install --path crates/atomics     # or: cargo run --release -p atomics
atomics                                 # opens the repository you are in (or your last projects)
atomics ~/repos/app                     # opens that project
atomics --workspace daily               # opens a saved workspace
atomics --open claude:~/repos/app --open codex:~/repos/app-feature
```

On macOS, `scripts/macos-app.sh` builds `~/Applications/ATOMICS.app` (release binary, the atomisk.ai icon, microphone
permission text), so ATOMICS runs from the Dock or Spotlight and keeps running when terminals close:

```sh
scripts/macos-app.sh
open -a ATOMICS --args --workspace backend
```

The Dock, Spotlight and a Linux desktop launcher start ATOMICS with a bare `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`).
ATOMICS asks your login shell (`$SHELL -l -i -c`) for its `PATH`, once per start and for at most five seconds, and
adds the folders it lacks. Only absolute folders count: an entry such as `.` or `node_modules/.bin` would find
programs in whatever folder a command runs in, so ATOMICS leaves it out. Everything ATOMICS runs itself then finds
what a terminal would: git with its LFS filters and hooks, the Codex version check, MCP and ticket servers and queued
commands. A `PATH` that already has those folders, as one from a terminal does, is left alone; a shell that gives
none in time leaves the `PATH` ATOMICS was started with. `atomics doctor` says which happened.

The window never waits for the shell, which can take seconds. Before it starts anything, it adds the folders the
shell gave at an earlier start (`login-path.json` in the state directory, written whenever ATOMICS reads the shell,
`atomics setup` included), then reads the shell's current `PATH` in the background: agents, MCP and ticket servers
and the Codex version check wait for that one, and it is recorded for the next start. With nothing recorded yet, git
and the rest of what the window runs itself keep the `PATH` it was started with until then. `atomics host` and the
other commands wait for the shell.

On Linux, install the build dependencies first (Debian/Ubuntu example), then install or update from this checkout.
For a new desktop, [Fresh start on Linux](docs/fresh-start-linux.md) lists every prerequisite and check:

```sh
sudo apt-get install build-essential pkg-config libasound2-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
scripts/linux-install.sh
atomics --workspace daily
scripts/linux-install.sh --uninstall  # removes the binary, desktop entry and icon; keeps config/state
```

The installer builds a release binary for the native Linux target and atomically replaces `~/.local/bin/atomics`,
including while ATOMICS is running. It asks nothing: open ATOMICS from your applications menu, and its first run opens a
setup screen. It prints the line to add `~/.local/bin` to PATH for terminal use if that is missing. The desktop entry and icon go under
`${XDG_DATA_HOME:-~/.local/share}`; rerunning updates the same files. Both Wayland and X11 are enabled. eframe supplies
embedded fallback fonts, so the UI and terminal do not require macOS fonts; system fonts can also be used. A working
Vulkan driver (for example Mesa's `mesa-vulkan-drivers`) supports the default wgpu renderer. File dialogs use the desktop
portal and external files open through `xdg-open` (`xdg-utils`).

### NixOS

The flake provides `packages.aarch64-linux.atomics` and `packages.x86_64-linux.atomics` from the workspace
`Cargo.lock`. Install it and set the ATOMICS config for the machine with the NixOS module:

```nix
{
  inputs.atomics.url = "github:xmagit/ATOMICS";
  outputs = { nixpkgs, atomics, ... }: {
    nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      system = "aarch64-linux";
      modules = [
        atomics.nixosModules.atomics
        {
          users.users.alice.isNormalUser = true;
          programs.atomics.enable = true;
          programs.atomics.user = "alice";
          programs.atomics.settings = {
            base_ref = "main";
            layout.diff_split = "auto";
          };
        }
      ];
    };
  };
}
```

`programs.atomics.package` overrides the installed package. When `programs.atomics.settings` is non-empty,
`programs.atomics.user` must name an existing NixOS user. The settings seed that user's
`~/.config/atomics/config.toml` only if it is missing; subsequent edits and `config.d/<host>.toml` stay in the
writable config folder. With empty settings, the module does not create or replace a config file.
The package puts the `atomics` CLI on PATH for
hooks and MCP, and puts its Wayland, X11, Vulkan, Mesa, font and audio libraries in the binary's RUNPATH (not
`LD_LIBRARY_PATH`, which agent terminals would inherit). A working
GPU Vulkan driver and desktop session are still required for the accelerated UI. Run `nix flake check` to
evaluate the sample NixOS configuration and build the package on supported Linux builders.

For the headless Apple Silicon VZ guest with Rosetta, Docker, btrfs and the development toolchains,
see [the NixOS VM setup](docs/nixos-vm.md). Its Lima profile allocates 4 vCPU, 8 GiB RAM and 100 GiB disk by
default; `atomics.vm.cpus`, `memoryGiB` and `diskGiB` change that, and `atomics.vm.user` (default `atomics`) names
the VM's user. Stacks beyond the base, such as the example .NET and Azure DevOps module, are opt-in modules.
ATOMICS stays native on the Mac (Metal) and starts its agents and chief in the guest over ssh; the
flake's `atomics-vm-mac` bundle holds the Mac's wrappers, socket forward and config snippet.

The config is created on first start at `${XDG_CONFIG_HOME:-~/.config}/atomics/config.toml`. It is commented and reloads while ATOMICS runs: agents
and their commands, `base_ref`, layout breakpoints, fonts, colors, terminal scrollback, shortcuts, and the texts used for
steering. `[github]` lists extra repositories and GitHub Enterprise hosts, picks what the counter counts
(`filter = "mine"`, `"review"` or `"all"`) and defines `[[github.actions]]` workflows with `{repo}`, `{pr}`,
`{head_sha}`, `{head}` and `{base}` placeholders (see [docs/github.md](docs/github.md)). Themes (built-in `claude`, `midnight`, `atomisk` and the light `paper`, or your own theme files) are described
in [docs/themes.md](docs/themes.md).

On Linux, a private, user-owned `XDG_RUNTIME_DIR` holds each new install's socket and the compiler jobserver FIFO,
in an `atomics/<instance hash>` subdirectory, with the FIFO isolated inside `jobserver/`. Without it, the socket stays beside the state and the FIFO uses the user's
temporary directory. Existing `~/.atomics` installs retain their state and socket paths; the FIFO can use XDG runtime storage.
`ATOMICS_HOME` always overrides the state path and keeps the socket beside that state; `ATOMICS_SOCKET` overrides just the
socket (except the one an agent's terminal gets from its host, which yields to an explicit `ATOMICS_HOME`), and
`ATOMICS_CONFIG` overrides the config file. Runtime FIFO paths are distinct for each state home. Empty or relative
XDG paths are ignored. The earlier `~/.cox` and config rename migration still runs without overwriting existing state.

### Upgrading from IMACS

ATOMICS was called IMACS before v0.9. The first command you run (the app, setup or doctor) moves `~/.imacs`,
`~/.local/state/imacs` and `~/.config/imacs` to their ATOMICS places without overwriting anything. Until you move
them yourself, ATOMICS honors `IMACS_HOME`, `IMACS_CONFIG` and `IMACS_SOCKET`, keeps reading the old keychain item
until it saves the new one, and reads a repository's `IMACS.md` and `.imacs/` while it has no `ATOMICS.md` or
`.atomics/`.

### Headless host

`atomics host` runs the same runtime with no window, egui, display or GPU, for a VM or a server. Agents run in their
terminals, and hooks, `atomics ctl` and `atomics mcp` reach the host through its local socket with no client attached.
With `[remote] enabled = true` it also serves the `atomics.v1` protocol ([docs/daemon.md](docs/daemon.md#headless-host-atomics-host)),
and `cargo run -p atomics --example loopback_client` prints its board. With remote off there is no protocol thread or port.

```sh
atomics host                        # the projects of the last session, or the launch folder
atomics host -w daily ~/repos/app   # a workspace and a project, as for the desktop
atomics ctl '{"cmd":"start_chief"}' # start the configured chief without opening a window
atomics ctl '{"cmd":"send","session":"chief","text":"Plan the next task"}' # starts it if needed
```

`host` is a subcommand, so `atomics host` no longer opens a folder named `./host` in the desktop; `atomics -- host`
(or `atomics ./host`) still does.

It stays in the foreground and logs to stderr: startup, the protocol address, the notices the desktop would show, and
shutdown. The protocol token is only in `protocol.json` (mode 0600), never in the log. SIGTERM, SIGINT or SIGHUP stops
it cleanly: the socket goes first, agents stop as they do when the desktop quits, `protocol.json` is removed and it
exits 0. A second signal ends it at once. It exits 1 instead of running half-reachable: when the local socket cannot be
served, or when `[remote]` is enabled and the protocol server cannot start (a taken `port`, say). The log then says why.

A host owns its `ATOMICS_HOME`. It holds `instance.lock` there exclusively, and every desktop holds it shared: a host
does not start while a desktop uses that home, a desktop does not start while a host does, and desktops still start
side by side as before. Requests only a local window can serve (`screenshot`, `resize`, `show_settings`) return an error
even with a client connected; these commands are not forwarded, so use the client's UI. Restart the host through
its service manager. The desktop cannot attach to a running host yet; that is phase 3 of
[docs/daemon.md](docs/daemon.md). A systemd user unit (use the installed binary's absolute path):

```ini
[Unit]
Description=ATOMICS headless host

[Service]
ExecStart=%h/.local/bin/atomics host
# SIGTERM to the host alone; it stops its agents itself, and anything left gets SIGKILL after the timeout.
KillMode=mixed
TimeoutStopSec=60
Restart=on-failure

[Install]
WantedBy=default.target
```

### Restart onto a new build

Rebuild and install into the same location first (`scripts/macos-app.sh` on macOS, `scripts/linux-install.sh` on Linux). The built-in chief can then call
`atomics_restart` with an optional `reason`. It is available only in the chief's `atomics mcp --chief` server; ordinary
orchestrators and task agents do not receive it. The equivalent local control request is:

```sh
atomics ctl '{"cmd":"restart","reason":"installed the new build"}'
```

Restart refuses and lists blockers while another agent is working or starting, an approval or question is pending,
messages await delivery, a queue slot is held or waiting, a worktree/cleanup or integration job is running, or a plan
brief is still focused for editing. Finish the edit and wait for agents and queued work. Idle and accepted agents
are fine. The chief that calls `atomics_restart` hands off its own turn; from anywhere else a working chief waits like
any agent. An update's restart waits for more, since nobody chose its moment: see
[Updates](docs/how-it-works.md#updates). `force: true` bypasses those busy checks and may interrupt work, but never
bypasses saving state or validating that the installed executable exists and successfully runs `--version`.

ATOMICS saves every plan, the session store (including Claude resume IDs and the chief's role), and the layout, then
quits gracefully. A relauncher in its own session ignores SIGHUP, waits for the old PID to exit, and starts the same
installation: `open -n -a` on the exact `.app` bundle on macOS, or the same binary on Linux, with the original arguments.
The macOS path requires a `.app` bundle; a development binary outside one refuses to restart. Instance home, socket
and config overrides are preserved, and helper output goes to `restart.log` under `ATOMICS_HOME`.

Live sessions with resume IDs reopen automatically. The chief resumes as chief, with its dock and width preserved,
and starts a turn asking it to call `atomics_wait`. The first wait receives a `restarted` event, even with an old cursor
or a status filter, containing `restarted: <old build> -> <new build>, reason …`. Build identities come from
the flake's commit revision in a small wrapper package for Nix builds, read at startup without recompiling Cargo
when only documentation changes; other builds use `git describe --always --tags --dirty` at compile time,
falling back to the crate version. `atomics --version` prints the crate version and then that build, as
`atomics 0.1.7 (v0.1.7-3-gabc1234)`. An update reads the version from this line.

For Nix builds, `nix eval --raw .#atomics.unwrapped.drvPath` identifies the source-filtered Cargo derivation.
Documentation-only commits reuse that derivation while the small package wrapper updates its revision metadata.

## Using ATOMICS on several machines

Share `~/.config/atomics/config.toml` between your machines and put machine overrides beside it in
`config.d/<hostname>.toml`. `ATOMICS_HOST` overrides the machine name (for example, `ATOMICS_HOST=work-mac atomics`).
On macOS the default is the stable LocalHostName from System Configuration (`scutil --get LocalHostName`), with
the kernel hostname as fallback. Linux uses the kernel hostname. A trailing `.local` is stripped.
Names are case-sensitive and allow letters, numbers, dots, underscores and hyphens.
Empty or invalid overrides fall back to the machine name; if that is unavailable, the name is `localhost`.
The same name selects both the override file and the machine's workspaces.

Configuration loads in this order: built-in defaults, `config.toml`, then the selected host file. Tables merge
recursively by key: overriding `[resources.dotnet].processor_count` keeps other resource settings. Scalars and
arrays replace the base value, so a host's `repos` or `agents` list replaces the whole list; only `[chief] prompt`
adds to the main file's instead (see [docs/configuration.md](docs/configuration.md)). The host file is optional;
an unreadable or invalid existing file is an error. Both files reload while ATOMICS runs, including when you create
or remove the host file. `atomics doctor` prints the selected host and every loaded config file in order.

For example, the shared `config.toml` can contain:

```toml
repos = ["~/repos/atomics"]
worktree_path = "~/repos/{repo}-{slug}"

[font]
size = 13.0
diff_size = 12.5
```

And `config.d/work-mac.toml` can contain:

```toml
repos = ["~/repos/backend"]

[font]
size = 15.0 # diff_size stays 12.5
```

Use `~` or `~/…` for repository and workspace project paths and worktree templates. They expand against the
current machine's home directory when used; the shared config keeps its original portable strings. The default
worktree template, `{repo_parent}/{repo}-{slug}`, uses the current repository's parent and also needs no machine path.
Keep machine-specific absolute paths out of shared config. `ATOMICS_CONFIG` selects a different base file; its sibling
`config.d/` and `workspaces/` directories are used too. Otherwise `XDG_CONFIG_HOME` is honored.

Saved workspaces are per host: `~/.config/atomics/workspaces/<host>/<name>.toml`. Home and work can both have a `daily`
workspace with different projects. Loading checks the host directory first, then falls back to flat
`workspaces/<name>.toml` files saved by older ATOMICS versions. The workspace list combines both locations, with the
host file winning on a name clash. Saving always writes to the host directory and leaves flat files untouched,
so existing launch commands keep working and a shared config directory stays usable by other machines.
Saving projects under your home writes `~/…` paths. For example, `workspaces/home-linux/daily.toml` can contain:

```toml
projects = ["~/repos/atomisk"]
```

A workspace file can also hold its own `[tickets]` section, so work can follow Linear and home Relay. It replaces
the main config's `[tickets]` as a whole, so copy a complete profile: the Relay or Linear block in the default
config. See [docs/tickets.md](docs/tickets.md). Loading and saving a workspace keep the section as written.
Keep runtime state (`~/.atomics`, including sessions, sockets and plans) local to each machine.
[Moving to a new machine](docs/moving-machines.md) covers leaving a machine behind, including plans still in progress.

## Testing on Linux

Docker Desktop (or Docker Engine on Linux) can build, test and capture ATOMICS without installing a Linux toolchain
on the host. Start Docker first; `scripts/linux-qa.sh` checks `docker info` and reports if it cannot reach the daemon.
Run from the checkout you want to test:

```sh
atomics queue --class test -- scripts/linux-qa.sh test
atomics queue --class qa -- scripts/linux-qa.sh screenshot /tmp/atomics-linux.png
atomics queue --class qa -- scripts/linux-qa.sh screenshot /tmp/atomics-linux-wide.png --size 1920x1080
atomics queue --class qa -- scripts/linux-qa.sh screenshot /tmp/atomics-scratch.png \
    --ctl '{"cmd":"start","agent":"shell","isolation":"bare","prompt":"echo hi"}' --ctl '{"cmd":"activate","session":"echo hi"}'
scripts/linux-qa.sh shell
```

`test` runs the full locked workspace suite, including doc tests, and keeps running other test targets if one fails.
`screenshot` builds ATOMICS, starts a disposable project with a shell under Xvfb, waits for its isolated control socket,
captures the window through `atomics ctl`, verifies the PNG size and exits. Each `--ctl` request is sent to that
instance first, in order, to set the scene; one that fails stops the run. The default size is 1600x1000; supported
sizes range from 480x320 to 8192x8192. Its instance names its config, so the setup screen stays away;
`ATOMICS_QA_FIRST_RUN=1` shows a person's first run instead. The image uses Debian, its own Rust 1.98 (whatever `rust-toolchain.toml` says), Xvfb and Mesa's lavapipe Vulkan driver
(software rendering for wgpu). No display server, GPU, agent credentials or ATOMICS state from the host are shared.

The repository and linked-worktree git metadata are mounted read-only. Each checkout gets a separate Docker target
volume labelled with its checkout path; each run removes labelled target volumes whose checkout is gone and which
no running or stopped container uses. Unlabelled volumes and the shared Cargo registry and git volumes are kept.
Cargo registry and git downloads are cached across checkouts. The image is cached by its Dockerfile checksum,
and the builder receives only `Dockerfile.linux-qa`, never the source tree. To refresh the same image's OS packages,
remove its `atomics-linux-qa:<checksum>` tag and rerun. Images are kept until removed manually.

Containers drop all Linux capabilities, so permission checks behave normally even with the image's root user.
They default to two CPUs, two Cargo jobs, two test threads and 8 GiB of memory. Override CPU, job or memory limits
with `ATOMICS_LINUX_QA_CPUS`, `ATOMICS_LINUX_QA_JOBS` and `ATOMICS_LINUX_QA_MEMORY`. The host's jobserver FIFO cannot cross
Docker Desktop's VM boundary, so these limits apply in addition to the host queue slot. Queue commands may wait for
a slot and a cold build can take several minutes; agents should use a long timeout or background execution and request
access outside their sandbox if the ATOMICS or Docker socket is blocked.

For incremental compilation, use `scripts/linux-qa.sh shell` and run `cargo check --workspace --all-targets --locked`.
Fixture readiness and cleanup waits share `tests/support.rs::scaled_timeout`: `ATOMICS_TEST_TIMEOUT_SCALE`
is a positive multiplier (default 1), with a further ×4 when the one-minute load average exceeds the
available core count. Deliberate timeout behavior and latency assertions retain their strict budgets.
During development run only tests you change; reserve the queued full suite for the end. If using the shell for a heavy
test run or a GUI instance, queue the shell itself with the matching class. Shell changes outside `/repo` are disposable;
the target and Cargo caches persist.

Real Docker Sandboxes need KVM and cannot run inside this container. On the Ubuntu 24.04+ desktop,
finish `atomics setup`, then run:

```sh
atomics queue --class qa -- scripts/linux-sandbox-smoke.sh
# Rehearse the lifecycle without KVM (inside scripts/linux-qa.sh shell, under a QA slot):
xvfb-run -a dbus-run-session -- scripts/linux-sandbox-smoke.sh --fake-sbx --atomics /target/debug/atomics
```

The smoke script runs doctor, then `atomics sandbox relay-check`: one throwaway Claude sandbox that must answer on
the Claude subscription through ATOMICS's credential relay, with no token in the VM (store the token with
`atomics setup` first, or pass `--skip-relay`). It starts an instance with its own `ATOMICS_HOME`, `ATOMICS_SOCKET`,
`ATOMICS_CONFIG`, runtime and temporary directories, and creates two shell agents in disposable worktrees. It verifies terminal
output and workspace file synchronization, measures 20 control round trips (maximum 1000 ms by default),
then removes its app/helpers, sandboxes, MCP registrations, worktrees and temporary directories and checks
for leftovers. It prints a pass/fail summary and exits nonzero on failure, including cleanup failures.
Your normal HOME, desktop display (Wayland or X11), sbx sign-in and global policy are reused; a missing policy must be initialized with setup first.
It never removes other instances' sandboxes or MCP servers. Use `--atomics /path/to/atomics`, `--timeout 900`
and `--latency-ms 1000` to select an already built binary or adjust the startup/latency limits. `--fake-sbx`
uses the repository fixture and reports explicitly that VM isolation and KVM were not exercised.

## Troubleshooting

Run `atomics doctor` to check git, Claude Code, Codex, the configured editor, the config, the running ATOMICS socket and global Claude hooks.
It also checks Codex hook support (0.165+) and whether Codex trusts the hooks ATOMICS passes its bare Codex agents, reads plan files,
warns about cargo entries in `[approvals].allow`, lists merged worktrees safe to review for cleanup, and lists each
repository's stash, which all its worktrees share, entry by entry with its message. It changes nothing.
Each failed check includes a hint. Missing git or an unreadable or invalid config returns exit code 1; missing
agents, Helix, a running ATOMICS or hooks do not affect the exit code. Tools are checked on your login-shell PATH; the
first line says where `PATH` came from: the login shell's folders added, already there, or (a warning) not given in time.

For Codex hook trust, doctor computes the hook definitions ATOMICS passes (its executable and socket) the way Codex
does, and reads `[hooks.state]` in the Codex home those agents get (`<ATOMICS_HOME>/codex` for an explicit
`ATOMICS_HOME`, else `$CODEX_HOME`, else `~/.codex`). The executable is the one the desktop or `atomics host` that
serves this ATOMICS home runs from (for a Nix package, the profile link that leads to it, such as
`/run/current-system/sw/bin/atomics`, which the hooks name so that an update keeps Codex's trust), which it records
in `executable.json` there at every start, so a terminal's
doctor from another install (a cargo-installed CLI beside the app) checks the hooks the app passes; with none
recorded yet, it checks its own, and its line names the binary either way. It names each event as trusted,
untrusted, changed since trusted (a new install path, socket or definition) or disabled, and the fix: trust them in
Codex's review prompt or `/hooks`, or set the exact `trusted_hash` entries it prints. Codex keeps trust per event, not
per install, so it trusts one install's hooks at a time: doctor lists each other install it finds (itself, the app in
`~/Applications` or `/Applications`, `~/.local/bin` and `~/.cargo/bin`) and whether Codex trusts its hooks instead.
While every Codex agent runs sandboxed, ATOMICS passes no hooks and doctor says trust is not needed.

Cleanup failures the startup sweep finds are replaced at every start, so a failure that is fixed, such as a deleted
project folder or the record of a sandbox that is gone, drops out of doctor after the next start. Failures that no
sweep looks at again, such as a ref guard's alert, stay until you review them.

## Usage

The sidebar's **USAGE** panel shows account-wide session and weekly limits remaining, reset times and report age.
Claude reports through its statusLine; Codex is polled in the background every 60 seconds from the newest rollout
under `$CODEX_HOME/sessions` (default `~/.codex/sessions`), reading only its last 16 KiB for a `token_count` rate-limit event.
**no data yet** means no report has arrived or a reported window has expired. A window omitted by a received report
shows **not reported by this account**. Limits are shared across sessions; weekly reset days are UTC.

Claude sessions started by ATOMICS use `atomics statusline --socket <path>` automatically. It reads Claude's JSON from stdin,
forwards usage to ATOMICS, then chains to your own `statusLine.command` with the same input and output. User, project and
project-local settings keep their usual precedence. Without a custom command it prints a short usage line.

## Claude sessions anywhere

Claude agents that ATOMICS starts get its hooks through `--settings`. Codex 0.165+ gets inline
`-c hooks.<Event>=...` settings; older versions keep `busy_text` status tracking.
Version detection runs in the background with a 30-second limit. Successful results are shared;
failed or timed-out probes are retried on the next start. Until one answers, hook support is unknown: Codex starts
without hook flags (an older Codex refuses them), and its tile warns that its approvals stay in its terminal.

On the first Codex launch from an ATOMICS install, review and trust the ATOMICS hooks in Codex's
startup prompt (or `/hooks`). Their definitions stay identical across panes, so this is a
one-time step until the install path, socket or hook definitions change. Repository hooks keep
their own trust review; ATOMICS never bypasses hook trust. Each Codex session gets its pane in
`ATOMICS_HOOK_PANE`, which its hooks inherit, so it is found even through a wrapper such as ssh into
a VM; without it, a hook's process ancestry identifies the terminal (Codex runs with `--no-daemon`).
`SessionStart` maps the Codex session ID to that pane, including when several panes share a worktree.
`atomics doctor` says which events Codex trusts and how to trust the rest. To make Claude sessions in any other terminal show up too
(status, diff, and messages delivered at their next tool call or turn end), add the hooks to your user settings:

```sh
atomics install-hooks      # merges into ~/.claude/settings.json, keeps a backup
```

## Permission approvals

Codex permission hooks show **needs you**, the command, working directory and reason on the tile.
`atomics_wait` wakes with that request in the event's `approval` field, plus `plan` and `task` ids when linked to a plan.
`atomics_list` and `atomics_result` also include the oldest pending request. Each carries an `id`. The chief can answer
with `atomics_approve {session, request, decision: "allow" | "allow_always" | "deny", message?}`, where `request` is
the `id` of the request it decided about. Only the session's current request is answered, and only when `request`
names it: a decision without an id, or with the id of a request no longer pending (answered, withdrawn, or followed
by a newer one), answers nothing, and its error shows the current request and its id. A late decision can therefore
never answer a request its maker has not seen.
`allow_always` remembers the exact command, tool and directory for this session only; it does not
write a global rule. Approval decisions and held input target the specified session, even while another plan
is selected. Input held by one plan’s session is released only when that session’s permission request finishes.
`message` is passed to Codex with the hook decision.

Automatic approval is configured in `config.toml`:

```toml
[approvals]
allow = ["ls", "cat", "rg", "git status", "git diff", "git log"]
env = ["RUSTC_WRAPPER="] # optional; defaults to []
```

The built-in allow list is read-only. Adding a cargo entry permits agent-written code, including
build scripts and tests, to run outside Codex's sandbox. Add such entries only if you intend that access.

Command prefixes match parsed words, with the working directory and file arguments inside the
session worktree. Plain `a && b` and `a; b` chains pass only when every command passes independently.
Quoted `&&` and `;` are literal arguments, not chain separators.

`env` lists exact `NAME=value` assignments allowed before a command, either directly or through
`env` without options. `RUSTC_WRAPPER=` permits only an empty value; it does not permit a wrapper
program. Unlisted assignments, including `PATH`, `LD_*`, `DYLD_*` and arbitrary `RUSTC` values,
wait for the chief. Assignments alone do not authorize any program.

An explicit `cargo test` entry also covers `cargo +stable test`, `rustup run stable cargo test`
and absolute paths named `cargo` whose real path matches a configured absolute cargo or the first
executable cargo on PATH. A different binary named cargo does not inherit that permission.
`atomics queue` is judged by the command it executes, including these env and toolchain forms.

Pipes, redirections, substitutions, globs, newlines, comments, unsupported shell syntax, paths
escaping through symlinks and read-tool options that execute programs or write files still wait
for approval. Expansion syntax remains refused even inside quotes. Cargo's `--manifest-path`,
`--config`, `--target-dir`, `-Z` and `-C` options also wait, including joined forms, even with an
explicit cargo entry. Unknown commands always wait for the chief.
Set `allow = []` to make every permission request wait. This policy handles requests Codex raises;
it does not add approval prompts to commands Codex already permits.

Pending permissions appear under the agent's header with the command, cwd, reason and **Allow**, **Allow always**
and **Deny** buttons. They use the same handler as `atomics_approve` and remain available while the chief reviews.
With a running chief, ATOMICS waits up to nine minutes for a hook decision, below Codex's ten-minute permission-hook
timeout. Without a running chief it waits ten seconds, then leaves Codex's native permission menu
available so you can answer in the terminal. After that, `atomics_approve` selects the matching
visible menu choice; it returns an error if no matching menu is visible yet. For `allow_always`,
ATOMICS remembers the command and selects the one-time allow choice rather than creating a broader
Codex prefix rule. A denial message can only be supplied while the hook is waiting.
Truncated commands match their visible prefix. If a long reason hides the command, `atomics_approve`
expands it with Ctrl+A and asks for a retry so the command is checked before selecting a choice.

An agent started without hooks still asks in its own menu. When a Codex command menu ("Would you like
to run the following command?") or a Claude one ("Bash command … Do you want to proceed?") is on screen
and no hook request exists, that menu becomes the session's pending permission request: its header,
the board, `atomics_list` and remote clients show it, and `atomics_approve` answers it with that agent's
keys (Codex: the numbered Yes, or Esc to deny; Claude: Enter on the focused Yes, or Esc). It presses them only
while the screen still shows the very command that was reviewed, and its result echoes the command
(`command`, `reason`, `menu`). `allow_always` answers such a menu once; only hook requests are remembered.
Other menus, such as Codex's edit and terminal-input prompts or Claude's file edits, are not command
approvals: answer them with raw keys.

While a permission request is pending, `atomics_send` holds plain messages and returns `held: true`.
It releases these messages after a lifecycle event confirms the request has finished, or after the
menu has been absent for one second. This also covers requests answered directly in the terminal.
Raw input is rejected with an error pointing to `atomics_approve` while `atomics_approve` can answer the
request; while any other menu waits, raw keys go through, so there is always one way to answer.

Codex sends wait for the complete message to appear in the transcript or visible queue, or for its
prompt hook. ATOMICS retries unconfirmed input and returns an error if delivery cannot be confirmed;
another message cannot replace a send still awaiting confirmation.

Agent terminals inherit no `ATOMICS_PANE`, `ATOMICS_HOME` or `ATOMICS_CONFIG`, and whatever `ATOMICS_SOCKET` the
parent had is replaced with the socket of the ATOMICS that runs the terminal (`ATOMICS_HOST_SOCKET` names the same
path), so `atomics queue` and `atomics ctl` there reach that ATOMICS, whatever a login profile sets. Codex hooks
receive the socket as an argument and their pane in `ATOMICS_HOOK_PANE`, which ATOMICS
sets for that session only (a terminal never inherits another's), then use the mapped Codex session
ID; Claude hooks receive socket and pane arguments. MCP servers receive
their own routing environment. An ATOMICS launched from an agent with an explicit `ATOMICS_HOME` keeps its socket
beside that state rather than its host's; explicitly set `ATOMICS_HOME`, `ATOMICS_SOCKET` and `ATOMICS_CONFIG` when
launching a separate test instance. `atomics queue` says on stderr when it runs a command without a slot, and why.
An explicit `ATOMICS_HOME` also gives bare agents their own `CODEX_HOME` (`<home>/codex`) and
`CLAUDE_CONFIG_DIR` (`<home>/claude`). Codex's per-event hook trust slots and Claude's folder trust
then belong to that instance. Sign in separately in those provider homes when needed.

### The shared stash

All worktrees of a repository share one git stash, so `stash@{1}` names whichever entry is second when git runs,
possibly another agent's. A Codex request to run `git stash drop`, `pop`, `branch` or `clear` always waits for the
chief or you, whatever `allow` says, including inside chains, pipes, substitutions and `sh -c`. It carries a `warning` that lists
the entries the command would drop as git lists them now, in the repository its `cd`s, `git -C`, `--git-dir` or
`GIT_DIR` lead to, each with whose it is: this agent's own, another session's
or task's (by the branch it was stashed on, or the task id it was stashed with), or one ATOMICS cannot tell. The
banner shows the warning above the command and offers no **Allow always**; `allow_always` from the chief answers once
and is not remembered, because the same index names another entry once anyone stashes. For the same reason an
answer that lets the command run lists the stash again first: when a drop would now take other entries than its
warning named (someone stashed or dropped while it waited), nothing is answered. The request waits on under a new
ID with the warning as it reads now, and the decision is refused (`stale_revision`) saying whose entries the
command would drop now. A denial needs no second look. Every brief tells agents
to stash with `git stash push -m <task id>`, to find their entry by that message and restore it with
`git stash apply <commit>`, and to drop, pop or clear nothing without approval. Claude agents ATOMICS starts on this
machine ask in their own terminal before these commands (`permissions.ask` in their session settings), also with
git options before `stash` such as `git -C ../app stash drop`, even where their settings allow git; sandboxed agents run unattended and have only the brief. `atomics doctor` lists every
repository's stash.

## The chief agent

Click the **CHIEF** pill at the top (or `cmd+o`) to open the chief: Claude in a large window in the middle of the
screen, started in the project's main checkout with every ATOMICS tool and a built-in way of working (plan, wait for your
approval, dispatch, verify, end with a fresh reviewer, never push without asking). Close the window to minimize it to
the pill. The **Dock** / **Float** button in its header switches between the floating window and a full-height column
beside the agents; the pill's context menu offers both modes too. Drag the dock's right edge to resize it, or double-click
to reset it. Mode and width are remembered; `[chief] dock = "floating"` (or `"docked"`) sets the initial mode.
`cmd+o` shows or hides either mode. On narrow windows the chief floats instead, then docks again when the window widens.
`cmd+shift+space` dictates to it.

When the chief works on tickets, a **trace lane** under its terminal shows each call as a card instead of JSON: searches,
reads, children, related tickets and the plan it builds, each with a spinner, then a check or a cross. Click a ticket in it
to read it in the Tickets panel. **Plan with chief** (a button in the Tickets panel's Read view, `cmd+enter`) creates an
empty plan with the ticket's link and sends the chief a message with the ticket's key, title, URL and plan id (never its
text), and the plan's name says `planning…` until the chief's `atomics_plan_set` fills it. See
[Tickets](docs/tickets.md#plan-with-chief).

Every `[chief] heartbeat_secs` (default 300 seconds), ATOMICS checks every open plan for missing or exited
agents, running tasks whose agent has been idle or turn-done for over ten minutes without reporting since that
status began, pending approvals or Codex menus, messages held for over a minute, and working agents without an event for fifteen minutes. Include
`heartbeat` in `atomics_wait` statuses to receive one fleet event with a `findings` list; an empty list means all good.
The chief pill shows a compact age and a dot when that check found something; hover for the full local check time.

Every `[chief] wake_secs` (default 180 seconds; `0` disables it, otherwise at least 30), ATOMICS can also wake
an idle or turn-done chief while an open plan has a running or review task, or another agent session exists.
It sends one line: `ATOMICS wake: <n> new events, <m> heartbeat findings (<short kinds>). Call atomics_wait and act; if nothing changed, post one short line.`
The counts summarize events since the previous wake and the latest heartbeat findings. A wake waits for a full
interval after the chief's turn ends or the previous wake. Working turns reset the timer. Pending permissions,
typing in the chief terminal within the last 60 seconds, or an unfinished prompt suppress the wake.
Pending input is tracked from native keystrokes and pastes since the last turn ended or submit (Enter), so
Claude's suggested prompts do not suppress wakes. Without input history, the screen fallback ignores styled suggestions.

The chief may read, plan, review and run the plan you approved without asking. It can also answer other
agents' permission prompts through the pre-approved `atomics_approve`, including `allow_always` for the rest
of a session. Starting agents, typing into terminals and closing sessions still ask for permission.
It verifies a sandboxed task inside its sandbox, with the pre-approved `atomics_exec`, which reaches no more
than that task's agent does: agent code never runs on your machine for a check. `atomics_exec` waits for the
same test and QA slots as `atomics queue`, and is refused for bare tasks, whose checks run on this machine; see
[Checking sandboxed work](docs/isolation.md#checking-sandboxed-work).

The chief can help configure ATOMICS too. `atomics_config_get` shows every effective setting and the file it comes
from, with secrets masked; `atomics_doctor` returns the doctor's checks; `atomics_config_propose` proposes a change to
`config.toml`, this machine's `config.d/<host>.toml` or the workspace file. Each proposal waits in the ATOMICS window
as a diff, with settings that run commands labeled, until you apply or reject it. Applying backs the file up
beside itself, keeps its comments and reloads. Your own instructions for the chief go in `[chief] prompt`; each of
those files adds its own. See [docs/configuration.md](docs/configuration.md) for scopes, merge order and the trust model.
Commit `ATOMICS.md` and declarative `.atomics/config.toml` to share project instructions, branch templates and lifecycle
settings with a team; see [Repository workflows](docs/repo-workflow.md).

## Orchestration: several plans you approve, agents that report back

An orchestrator (a Claude session with the ATOMICS MCP server) turns a goal into a plan with `atomics_plan_set`: tasks with a
brief, acceptance checks, dependencies, and which agent and model should do them. Without `plan`, this creates a new
plan and returns its unique id; pass `plan: "<id>"` to update that plan and to get, update, dispatch or integrate its
tasks. Rewording an existing plan’s goal preserves its task progress and integration branch.
With several open plans, update/dispatch/integrate require the id and errors list the open plans.
`atomics_plan_get` without `plan` lists open plans with their goal, done/total, approval and waiting tasks. With `plan` it
gives a compact view that fits the MCP client's limit for one tool answer: per task its status, dependencies, session,
branch, and its latest note and report summary cut short. Add `task` to read that task in full: brief, acceptance,
report and its newest notes. Pick a
plan in the top bar's **Boards** dropdown, then open its **plan board** with `cmd+b` or **Board** in the view switcher. It has columns for planned, running,
blocked, review and done. Edit briefs,
then **Approve plan**: nothing starts before that.
You can also approve a reviewed plan with `atomics ctl '{"cmd":"plan_approve","plan":"<id>"}'`.
Changing only branch names keeps approval; changing the goal or task requirements requires review again.

Task sessions show `task id · task title`; their branch is available on hover and as a separate field in
`atomics_list`. Generated task branches use `atomics/<short plan slug>/<task id>-<short title>`, with plan and
title slugs capped at 24 characters. With the default `worktree_path`, their folders are
`<repo>-<short plan slug>-<task id>`. Custom path templates still apply, and existing branches and saved
worktree locations stay in use. Collisions add a numeric suffix.

`atomics_plan_set` refuses a plan whose dependencies form a cycle and names it (every plan's final review waits for
all its other tasks, so no task can depend on the review). It also refuses a branch a planned task pins when another
task of the plan or of an open plan has it, and suggests a free name such as `<branch>-2`. A pinned branch that already
exists outside any open plan is accepted with a warning: a task may name it to bring that work into the plan, but
dispatching an agent on it would fail. Dropping a dependency that closed a cycle in an approved plan keeps the approval.

Agents can offer a local web server with `atomics_preview {port: 5173, path: "/", label: "Web"}`;
`atomics_preview {port: 5173, remove: true}` removes the declaration. The tile's **Preview** button opens it
in your browser. A native client connected to a remote host forwards the TCP connection over `atomics.v1`;
the in-process desktop opens loopback directly. Linux also detects listeners owned by the agent's process
tree; macOS requires declarations. Browser clients cannot create local forwards. `atomics_list` and
`atomics_result` show each session's previews (port, path, label, title, plain-HTTP URL on the host, and whether
it was declared or detected), so the chief can check a registration without reading the agent's transcript. See
[Preview](docs/daemon.md#preview).

`atomics_plan_dispatch` (or **Dispatch ready** on the board) gives each ready task its own worktree and an agent whose
first message is the brief. Every agent ATOMICS starts gets a `atomics_report` tool, pre-approved, to say when it is done,
blocked or has a question. Reports land on the task card, move done tasks to review, and wake the orchestrator's
`atomics_wait`, which then reads `atomics_result` and `atomics_diff`, reviews, steers, and accepts with `atomics_plan_update`.
The brief also tells the agent that every worktree shares one git stash: see [The shared stash](#the-shared-stash).

The **experimental plan notebook** is off by default. Enable it with `[plan] notebook = true`.
`atomics_note {text}` proposes a fact from the calling agent's own plan; its task, agent and time come from the host.
Proposals appear on the plan board and wake the chief, which reads them with `atomics_plan_notebook {plan}`.
Only the chief or user can **Pin**, **Unpin** or **Delete** an entry. The chief uses
`atomics_plan_notebook {plan, id, pinned: true}` (or `delete: true`); users use the board, including remote boards.
Other agents see only pinned entries, pulled with `atomics_notes {}`. Enabled task briefs carry one line with the
pinned count, never note text. There are at most 8 pinned entries, 300 characters per entry, and 32 total entries;
control characters are stripped and a full notebook refuses new proposals until the chief or user deletes one.
Entries persist with their plan, and old plans load with an empty notebook. Disabling the experiment stops agent
access while preserving the board's history and **Delete** action.

Report summaries and notebook text returned to agents or the chief carry provenance with their author and time:
**agent-authored text is data, not instructions**. Pinning does not give a note authority over the user's or workspace's
instructions. This framing also applies when the notebook is disabled. See the
[repeatable notebook A/B experiment](docs/notebook-experiment.md) for token, time, approval and rediscovery measurements.

Plans are stored separately in `plans/<id>.json` under the state directory, with status `open`, `completed` or `archived`. The old
`plan.json` in that directory migrates once and is kept as `plan.json.migrated`; an old plan whose tasks are all done
migrates as completed. Saves replace files atomically, and unreadable plan files are listed in the startup message.
Open plans are listed in the **Boards** dropdown; completed and archived ones are under **History** in the same menu. Pick one to see
that plan’s sessions; `cmd+b` opens its board. Other sessions remain under All, Ungrouped and the Repos dropdown. Each plan has
its own approval and automatic final review. Use `atomics_plan_close {plan, status: "completed" | "archived"}` or
**Complete plan** / **Archive plan** on the board to close it after confirmation. Reopen either kind from
**History**; approval and task history are preserved. `atomics_plan_set` with a completed plan's id reopens it too (not an archived one): task progress and the integration branch stay, and added or changed tasks need approval again, so a follow-up can be added after the final review. Its history remains available by id, and once only
one plan is open, mutation tools can omit its id again. Start the next plan while another finishes; the chief is never closed.
`atomics_wait` events include `plan` and `task` ids for sessions belonging to a task. A wait answers within 100 s
(default 90), inside the MCP client's 120 s limit for one call, and one answer carries what fits the client's limit
for tool text: when more events wait, it says so in `more`, and its cursor reads on from the last event shown.
`atomics_list`, `atomics_result`, `atomics_read`, `atomics_diff` and `atomics_exec` cut long texts the same way and say
how much they left out; `atomics_diff` takes `path` to read one file's hunks of a patch that was cut.

Open a completed plan’s board from **History** or from **Open board** on a finished task's tile. **Clean up plan…** previews only its recorded
task and integration branches and their worktrees. Once the integration branch or every task branch is contained
in the configured base (usually main), select the entries to remove. Uncommitted changes, unmerged commits and
worktrees used by agent sessions are kept with a reason. Removal rechecks the preview and never forces Git;
branches without worktrees can be removed too. The saved plan history stays available.

Tasks come together on the plan's **integration branch** (`atomics/plan-<plan-id>` unless the plan names one; the board
shows it). `atomics_plan_integrate` (or **Integrate** on a review or done card) commits whatever the task left uncommitted
on its branch (as `<title> (<id>)`, with the repository's own git identity; ignored files stay out) and merges the
branch into the integration branch. The merge happens in a worktree of its own next to the repository, created on
first use like any other worktree, so your main checkout is never touched. On a conflict the merge is aborted and the
task stays in review with a note listing the conflicted files. Tasks without dependencies branch from `base_ref`;
tasks with dependencies branch from the integration branch, so integrate accepted tasks before dispatching the tasks
that build on them. Every new worktree, a task's or one `atomics_start` makes, fetches the repository's remote first
and starts from the remote as it is now: from `base_ref` when it names a remote branch (`origin/main` by default), and
from the remote's default branch (`origin/HEAD`, else `origin/main`, else `origin/master`) when `base_ref` is a local
`main` or `master`, which is never moved. Another local branch, a tag or a commit is used as it is. A fetch that fails
(offline, missing credentials) does not stop the agent: it starts from what this machine last fetched, and the task's
notes, a toast and `atomics_start`'s `warning` say so. A new integration branch starts from the same fetched base. Returning a task to running clears its integration marker. Background checks verify that each marked
task's branch tip is in the integration branch with nothing left uncommitted, again whenever a ref changes, so new commits
must be integrated too. The board and dispatch use the same verified readiness. A check that cannot tell, or does not
finish within 150 s, says why on the task card and in `atomics_plan_get`, and runs again within 30 s. Dispatching checks
such dependencies afresh and answers once their checks have, or after 100 s, inside the MCP client's limit for one call.
A done task whose branch is still at the commit ATOMICS created it at, with nothing uncommitted, has nothing to
integrate: it counts as integrated wherever the integration branch and `base_ref` have moved since. When every task
finished that way, the final review has nothing to look at: it is marked done with a note saying so instead of being
started, and the plan can complete.

The bar starts with All, Ungrouped and one **Repos** dropdown listing every open project with its session count
and total changed lines (additions plus deletions) across those sessions. Its button shows the selected project's name
in a project view, otherwise **Repos · total changes**. Then comes one **Boards** dropdown. Its button names the board on
screen, at most 360 points wide (a longer goal ends in an ellipsis; hover for all of it), and a warning marker counts the boards that need you. Its menu lists those first, under **Needs you**: plans
waiting for approval, and plans with a blocked task, a question or an agent's permission request. Then **Active** lists
the other open boards, each with done/total and its state, then **History**. A row shows its board, and the board's
**Approve plan** approves it. Each open board's row has a close button that asks first. Tabs that do not fit fold into this menu too, the selected one last.
Dispatching the selected plan returns from its board to its fleet and minimizes a floating chief; a docked chief
stays beside the fleet. Updating or dispatching another plan puts a dot on the Boards button and on the plan's row, and
preserves the selected view and chief. Creating a new plan selects it. Each fleet shows only sessions linked to that
plan’s tasks. Pick the plan again later to return to that overview, or choose All or a project from Repos to see other
sessions.

Accepting an integrated task closes its agent; tasks without a branch, including the final review, close on acceptance.
Its completed tile fills an otherwise empty grid cell, showing the duration, last report, diff size, **Open board**,
**Diff** and **Resume**. Live agents keep their tile sizes and priority. Idle agents waiting for review show when they reported.
When every task is accepted, its board offers **Archive plan**; closed as completed, the plan leaves the dropdown's open
boards for **History**. The chief stays running and
worktrees remain for the existing **Clean up…** flow. Set `[plan] close_done_agents = false` to keep agents running.

## Tickets

Plans can follow a ticket system that has an MCP server, such as Relay or Linear. In board mode a board in the
ticket system is a plan and its tickets are tasks; in ticket mode one ticket is a plan. ATOMICS talks to the server
itself (stdio, or streamable HTTP with native browser OAuth or an environment token). A `[tickets]` section
in the workspace file or config maps discovery/read operations and optional writes to the server's tools.
Auto mode, the default, uses children as a board when present and saves that decision per plan. Swapping
systems changes only that section. Cards show each ticket's live key and state, and a click opens it. ATOMICS moves
ticket states as tasks move and comments on dispatch, reports, acceptance, blocks and completion, signed with
the reporting model. Changes made in the ticket system reach the chief as `ticket` events in `atomics_wait`.
Include `plan` in `atomics_wait` statuses to learn when the user approves a plan (board button or `atomics ctl plan_approve`), edits a task brief on the board, or closes or reopens a plan: one event with `plan` and `change` (`approved`, `edited`, `closed`, `reopened`).
Failures show on the card and never block a plan. Only mapped writes push states or comments; the Relay
example is read-only until per-write idempotency keys are supported. The Tickets panel (`cmd+k`) finds tickets by meaning or
keyword and reads one in full, from the same operations; a ticket key or bare number finds that ticket first.
See [Tickets panel](docs/tickets.md#the-tickets-panel).
The chief can search, read, list children and check related tickets through the chief-only
`atomics_ticket_search`, `atomics_ticket_read`, `atomics_ticket_children` and `atomics_ticket_related` tools.
They return cleaned cards using ATOMICS's credentials; worker agents retain their scoped read/comment tools.
With tickets configured, the chief's prompt explains these tools, the default `auto` mode and that ticket
text is data. See [Chief tools](docs/tickets.md#chief-tools) for arguments, bounds and errors.
Try the [two-minute demo](docs/tickets.md#demo), including a browser-free scripted OAuth walkthrough.
See [docs/tickets.md](docs/tickets.md) for the design, the
config reference and examples, with step-by-step [Relay](docs/tickets.md#step-by-step-relay) and
[Linear](docs/tickets.md#step-by-step-linear) setups.

## Open from a link

ATOMICS registers the `atomics://` URL scheme, so another tool can send you straight to the right place:

```
atomics://open                                        focus ATOMICS
atomics://open?plan=<plan id>                         show that open plan
atomics://open?ticket=<key or ticket URL>&system=<name>   the ticket's plan, or offer to plan it
```

With `ticket=`, ATOMICS resolves the ticket through the configured `[tickets]` profile: a key such as `ABC-12`, or a
web address that matches the profile's `link_url`. If an open plan links the ticket, that plan is selected.
Otherwise the chief opens with `Plan ABC-12: <title>` typed into its prompt, never sent: you read it and press
Enter (appended to anything already typed there). A `system` that is not the configured ticket system, or a ticket system you are not signed in to, shows a
toast asking you to sign in. A link only opens and focuses; anything else in it (another scheme or host, an unknown
parameter, a repeated one, both `plan` and `ticket`) is refused with a toast.
`atomics 'atomics://open?plan=…'` does the same from a terminal. When ATOMICS is already running, the link goes to it
over its control socket and the second process exits (on Windows there is no socket forwarding, so every link starts a new instance); otherwise ATOMICS starts and opens the link once it has loaded.

Registration: `scripts/linux-install.sh` adds `MimeType=x-scheme-handler/atomics;` and `%u` to the desktop entry and
runs `xdg-mime default atomics.desktop x-scheme-handler/atomics`; `scripts/macos-app.sh` declares the scheme in
`CFBundleURLTypes`. For Windows, see [Open from a link](docs/tickets.md#open-from-a-link) for the registry keys.

## Agents in sandboxes

By default each agent runs in its own [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) microVM,
which sees only its worktree and git's write folders. Sandboxed Claude uses your Claude subscription through
ATOMICS's credential relay on this machine, so the token never enters the VM. ATOMICS creates the sandbox when the agent starts,
attaches the tile to it with `sbx run`, reattaches after a restart and removes it with the worktree. It
needs `sbx` v0.50.1 or newer, signed in. Without it, sandboxed agents are refused, never started bare.
Set `[isolation] mode = "bare"`, or `isolation = "bare"` on one agent, to run agents directly on your
machine. A repository can require sandboxes in `.atomics/config.toml`, but never turn them off. See
[docs/isolation.md](docs/isolation.md) for the settings and the trust model.

## Running many agents

Every agent works in its own worktree with its own `target/`, so its builds never wait on another's.

New agent worktrees start with a warm build cache when the main checkout has one. Before launching the agent,
ATOMICS clones the folders in `[worktrees] warm` (default `["target"]`) from the repository's **main checkout**, even
when you start from another linked worktree. The clone runs on the background worktree job. Only copy-on-write
clones are used: unsupported filesystems, missing folders and clone errors are skipped without retrying as a full
copy or blocking launch on a fallback. macOS uses the native clone-only `clonefile` API
(`cp -c` can silently fall back to full copying); Linux uses `cp -a --reflink=always`.
Both preserve file modification times so warm build caches remain reusable.
On btrfs and reflink-enabled XFS, caches on the same filesystem can be cloned. Filesystems without reflinks (including
ordinary ext4), cross-filesystem clones and unsupported files skip warming and launch cold. A failed clone leaves no
partial cache, and ATOMICS never retries it as a full copy. Use `atomics doctor` to check the actual filesystem pair.
Each worktree then owns its cache independently; builds cannot change the main checkout's cache. A successful
clone shows **warm cache from main** in a toast. Existing worktrees are left alone when retrying a launch.
When creating a worktree fails (a filter or hook git runs is missing, say), ATOMICS removes the worktree git began
and deletes the branch it made while that still points at the base, so a retry starts over. A retry uses a branch
that already exists while it is still at the base, and refuses one that has moved.
`atomics doctor` probes CoW cloning between the repository and configured worktree folders, using temporary files
that it removes afterwards.

```toml
[worktrees]
warm = ["target"]          # existing folders relative to the repository root
# warm = ["src/**/bin", "src/**/obj"] # .NET caches at any depth
# warm = ["target", "build/cache"]
# warm = []               # turn cache warming off
```

Entries may use glob patterns: `*` matches within one path component and `**` matches directory levels,
including zero levels. Patterns expand against existing directories in the main checkout. Invalid patterns,
absolute paths, parent traversal, `.git` directories and symlinked directories are skipped. On Linux, the
expanded directories are also mounted read-write in the agent sandbox. For .NET repositories with many
project folders, `src/**/bin` and `src/**/obj` warm each project's build outputs independently.

Measured once per configuration on this Mac (APFS, 2026-10-29), with sccache disabled. ATOMICS itself created and
launched both agent worktrees at revision `63dcfd1`, using separate isolated instances with their own
`ATOMICS_HOME`, `ATOMICS_SOCKET` and `ATOMICS_CONFIG`. Each agent ran its first
`atomics queue --class test -- cargo build -p atomics`:

| Cache warming | Worktree creation → launch | Crates compiled | First queued build | Preparation + build |
| --- | ---: | ---: | ---: | ---: |
| Off (`warm = []`) | 0.18 s | 276 | 56.30 s | 56.48 s |
| On (`warm = ["target"]`) | 6.83 s | 13 | 22.24 s | 29.07 s |

Preparation includes the cache clone; build times are wall clock and include queue overhead. Warming cut
preparation plus first-build time by about 49% in this run. The cache was present before the warm agent's first
command, and absent before the cold agent's first command.

Agents share the machine, and ATOMICS keeps that in check for every agent it starts (`[resources]` in the config):

- **One compiler job pool.** ATOMICS owns a GNU make jobserver (a named pipe in a private folder under Linux's
  `XDG_RUNTIME_DIR`, or the user's temporary directory otherwise) with `jobs` tokens, cores minus 2 by default, and hands it to every agent terminal as
  `CARGO_MAKEFLAGS`. cargo takes a token per job, so ten agents building at once share those jobs instead of each
  using every core. On Linux, Codex also gets this instance's private XDG runtime `jobserver/` subdirectory as a
  workspace-write writable root (`-c sandbox_workspace_write.writable_roots=[…]`), so its sandboxed compilers can open
  the FIFO; unlike `--add-dir`, that never stops Codex in a folder it has not trusted yet. The setting replaces
  `writable_roots` from your own Codex config for ATOMICS's sessions. `MAKEFLAGS` is left alone, since `make` before GNU make 4.4 misreads a pipe jobserver. A job
  killed while holding a token never returns it, so whenever no compiler or build script runs, ATOMICS resets the
  pool to full. cargo passes the pool on to build scripts, and a build script that runs Apple's `make` 3.81 (some
  `-sys` crates, vendored OpenSSL) stops with "invalid --jobserver-fds"; set `jobs = 0` to turn the pool off, or put
  GNU make 4.4 first on PATH.
- **A queue for heavy work.** `atomics queue --class test -- cargo test` waits for a free test slot (one per 6 cores
  by default), runs the command and passes its exit code through. `--class qa` is for app instances started for
  screenshots. The slot is released when the command ends or its process dies. Tiles show `queued for a test slot
  (2 in line)`, and the approval policy judges a queued command by the command it runs (only a bare `atomics`
  counts). Without a running ATOMICS the command runs right away; while ATOMICS restarts, waiting commands reconnect
  and queue again instead of all running at once. A sandbox that blocks the ATOMICS socket (Codex's does) gets a
  clear error and exit code 75: run the command outside the sandbox. Queued commands may wait, so give them a long
  timeout.
- **Lower priority.** Agent terminals start at niceness `agent_nice` (10), so ATOMICS and your desktop stay responsive.
- **A shared compiler cache.** When sccache is installed and you have no `rustc-wrapper` of your own, agents get
  `RUSTC_WRAPPER=sccache`, so dependencies compile once for all worktrees (with
  `SCCACHE_IGNORE_SERVER_IO_ERROR=1`, a sandboxed build that cannot reach the cache compiles locally instead).
- **Automatic .NET budgets.** A worktree containing a `*.sln`, `*.slnx`, `*.csproj` or `global.json`, at its root or
  below, gets a .NET launch environment; other worktrees get no .NET settings. Detection includes untracked files,
  skips Git metadata and directory symlinks, and works when the agent starts in a worktree subdirectory. No project
  files or SDK configuration are changed. `DOTNET_PROCESSOR_COUNT` limits the CPU count seen by MSBuild, Roslyn
  and the test host; `MSBUILDDISABLENODEREUSE=1`, `DOTNET_CLI_USE_MSBUILD_SERVER=0` and
  `UseSharedCompilation=false` prevent new persistent MSBuild and Roslyn servers. Existing servers belonging to
  other sessions are left running. Unlike cargo's shared pool, this is a per-process budget.
- **Tests at the end.** Every brief asks agents to use `cargo check` and the tests they touch while working, and to run
  the full suite once, at the end, through `atomics queue`. For .NET suites use
  `atomics queue --class test -- dotnet test`.

The .NET defaults need no config changes. Override them under `[resources.dotnet]`:

```toml
[resources.dotnet]
enabled = true
# processor_count = 2  # default: resources.jobs / resources.test_slots / 2, rounded down, at least 1
```

With 18 cores, the default 16 jobs and 3 test slots give each .NET process 2 CPUs: each runnable project can have
both an MSBuild node and a compiler process. A fixed `processor_count` takes precedence; zero is clamped to one.
`enabled = false` leaves the inherited environment alone, and changes apply to
new terminals. `resources.jobs = 0` disables the Rust pool and gives .NET a one-CPU default.

These are cooperative limits, not an OS CPU quota: explicit `-m:N` can override MSBuild's processor-based default,
and project properties can override properties imported from the environment. NUnit/xUnit's default workers
follow the reported CPU count (NUnit has a minimum of two); assembly attributes, runner configuration and custom
test code can choose their own parallelism. Keep those settings within the budget and queue whole test suites.
ATOMICS does not inject VSTest-specific arguments into every project, since Microsoft.Testing.Platform interprets
them differently. The [SDK 9 measurements and reproduction scripts](docs/dotnet-resource-budget.md) show the
three-build peak falling from 25 .NET processes to 12 (5.18 s to 6.49 s on the shared 18-core machine).

The `load` gauge in the top bar shows the 1-minute load per core (grey, then yellow from 0.7 and red from 1.0); hover
for the pool size and the slots in use. If ATOMICS ever ends without a clean quit, the next start says so and
`crash.log` in the state directory (`~/.atomics` on macOS/existing installs) has the panic and its backtrace when there was one.

## Voice

Press `cmd+shift+space` and speak; press it again to send. The recording from the default microphone is transcribed
and sent to the chief agent, which is started and opened if it isn't running. `Esc` cancels. The chief pill shows
`● REC m:ss` while you speak and a spinner while the transcript is on its way. Recordings stop on their own after 5
minutes. `target = "active"` under `[voice]` sends to the active agent instead.

Pick a provider under `[voice]` in the config:

- `provider = "openai"` (default): any OpenAI-compatible `/audio/transcriptions` endpoint, so OpenAI
  (`gpt-4o-mini-transcribe`, key in `OPENAI_API_KEY`), Groq or a local Whisper server.
- `provider = "openrouter"`: an audio-capable chat model on OpenRouter (`google/gemini-2.5-flash`, key in
  `OPENROUTER_API_KEY`).

```toml
[voice]
provider = "openai"
base_url = "https://api.groq.com/openai/v1"
model = "whisper-large-v3-turbo"
api_key_env = "GROQ_API_KEY"
```

The key comes from the environment variable named by `api_key_env`. If ATOMICS doesn't have that variable (apps started
from the Dock get a bare environment), it reads the variable from your login shell. `api_key` in the config also works
but is discouraged. ATOMICS never logs or shows the key. macOS asks for microphone access the first time you record, for
the terminal you started ATOMICS from or for ATOMICS itself when it runs as an app.

## Let an agent drive ATOMICS (MCP)

```sh
claude mcp add ATOMICS -s user -- atomics mcp
codex mcp add ATOMICS -- atomics mcp
```

Tools: `atomics_wait` (block until an agent finishes a turn or needs input; returns a cursor for the next call),
`atomics_approve` (allow, allow_always or deny a pending permission request, named by its id),
`atomics_result` (an agent's last prompt, final message and screen), `atomics_list`, `atomics_start` (in a folder, a new worktree from `repo` + `branch`, or with neither in a scratch folder; `isolation` overrides the settings for that session), `atomics_send` (a message, or raw
keys for menus), `atomics_read` (screen text), `atomics_preview` (offer or remove a session's loopback web server), `atomics_diff`, `atomics_comment` + `atomics_send_review`, `atomics_open` (show a file to the user), `atomics_focus`, `atomics_close`.
Comments an agent leaves show inline in the diff panel before they are sent, so you can see what it asks for.

`atomics ctl '<json>'` sends the same requests from a shell, e.g. `atomics ctl '{"cmd":"list"}'`. A few extras exist for
scripting and testing: `show_diff`, `show_chief`, `show_settings` (the Settings window), `resize`,
`screenshot` and `remote` (the protocol server, the rendezvous: registered, online, channels, and the paired devices). For example,
`atomics ctl '{"cmd":"show_chief","open":true,"dock":"docked","width":720}'` shows the dock and remembers its width.

## Shortcuts

The top bar's **view switcher** picks what the panel beside the agents shows: **Board** (the selected plan's board,
with its progress and a count of the tasks in review or blocked), **Diff** (the active agent's changes, with the
number of changed files; the editor counts as the diff) and **Tickets** (when `[tickets]` is set; a dot while the ticket
system wants a sign-in, else how many tickets on the plan's board no task follows). The view on screen is filled;
picking it again hides the panel. Each window remembers its choice: the desktop in its state folder, a browser in
its `localStorage`; a row of the Boards dropdown shows that board. The CHIEF pill stays in the middle of the bar. The location
line over the agents says where the window is, plan › task › session ([Navigation](docs/navigation.md)).
Click the load gauge or **Machine** in Actions to see the host's CPU, memory, disk space, test/QA queues,
containers, listening ports and agent previews. The shared panel works in the desktop and connected clients;
remote access requires the host-management grant. Native controllers can Preview listed loopback services,
including Docker published ports; browser clients show the data with Preview disabled. Collection runs only
while a Machine panel is open. See [Machine dashboard](docs/daemon.md#machine-dashboard).


`cmd` is Command on macOS and Ctrl+Shift on Linux, so it never collides with the terminal's plain Ctrl keys.
For example, `cmd+p` is Ctrl+Shift+P on Linux. `cmd+shift+space` becomes Ctrl+Shift+Space (Shift is applied once).
`primary` is Command on macOS and plain Ctrl elsewhere: `primary+g` is Command+G or Ctrl+G. Like Ctrl+1…9, ATOMICS takes it
before a focused terminal or editor. Its letter follows the keyboard layout. On Linux that takes Claude Code's own Ctrl+G
(edit the prompt in an editor), so bind `github` elsewhere, for example `cmd+g`, to keep it. A `github` binding that
collides with another one, or with Ctrl/Command+1…9, is reported in the top bar and falls back to `primary+g`.
Switching agents uses **Ctrl+1…9 on Linux** and **Command+1…9 on macOS**, including while a terminal has focus,
on the desktop, in a browser and on a `--connect` client alike; each agent's row in the sidebar shows its number
([docs/navigation.md](docs/navigation.md)). The digits follow the physical keys, leaving Shift+digits available for
symbols such as `!`.
Tooltips and the settings menu show the actual platform chord, including custom bindings.

| Key | |
|---|---|
| `cmd+p` | go to file |
| `cmd+e` | diff ⇄ editor |
| `cmd+b` | the plan board: shows it beside the agents, or hides the panel while it shows (the view switcher's **Board**) |
| `shift+↓` / `shift+↑` | next / previous changed file, scrolling to its header and focusing the diff (except in a text field or the editor) |
| `PageDown` / `PageUp` | next / previous hunk in the focused diff, continuing across changed files |
| `↓` / `↑`, `Home` / `End` | scroll a line, or go to the top / bottom of the focused diff; Esc, typing, or clicking the terminal returns focus to it |
| `cmd+d` | the diff: shows it, or hides the panel while it shows (the view switcher's **Diff**) |
| `cmd+j` | next agent that needs you |
| `cmd+alt+left` / `cmd+alt+right` | back / forward along the places the window showed (tab, session, view), as the ‹ › at the start of the location line; Escape goes back too while nothing has the keyboard ([Navigation](docs/navigation.md)) |
| `cmd+k` | the Tickets panel (when `[tickets]` is set; the view switcher's **Tickets**), or hides the panel while it shows: Find, then Read; the same keys as the diff — `shift+↓/↑` next / previous result (in Read: next / previous ticket; not from a text field or the editor), `PageUp/PageDown` a page in Find and the next / previous section in Read, ↑/↓ and Home/End, `Enter` read or open, `Alt+←` back, `Tab` in the box meaning ⇄ keyword, `Esc` leaves the box, then the panel; the chief's trace lane takes `shift+↓/↑` and the page keys when you click it |
| `cmd+enter` | in a ticket the Tickets panel is reading: Plan with chief |
| `cmd+l` | steer bar |
| `cmd+shift+space` | voice: record, press again to send to the chief, `Esc` cancels |
| `cmd+t` | new session (scratch folder, existing folder or new worktree) |
| `cmd+r` | refresh diff |
| `cmd+w` | close session |
| `primary+g` (`Command+G` / `Ctrl+G`) | GitHub controls, which hold the keyboard while open: `shift+↓`/`shift+↑` select the next/previous PR, PageDown/PageUp jump to the next/previous repository, Enter opens the PR, `D` loads its diff, `M` merges (after confirming; Enter confirms, Esc cancels), Esc closes |
| `Command+1…9` (macOS) / `Ctrl+1…9` (Linux) | select agent 1–9 on screen and focus its diff, or its editor in editor mode; in the Tickets panel, Find stays in Find and Read shows that agent's task ticket (the chief's and untasked agents show the plan's) |

## Design

Two crates. `atomics-core` holds everything testable without a window: config, the git diff engine and parser, the
responsive layout, the hook and control protocols, and review message composition. `atomics` is the app.

| Concern | Choice | Why |
|---|---|---|
| UI | egui/eframe (wgpu) | Pure Rust, immediate mode: layout is plain code each frame, so responsive tiling is simple; idle when nothing changes |
| Terminal | alacritty_terminal | Mature VT emulation and PTY event loop; Claude's and Codex's TUIs render correctly |
| Git | `git` CLI | Exact porcelain semantics, and picks up `core.fsmonitor`/`untrackedCache` on large repositories |
| Highlighting | syntect + bat's grammars (two-face), Oniguruma | Works line by line, which suits hunks that start mid-file; Oniguruma is ~3× faster and ~4× smaller than fancy-regex |
| Word diff | similar | Code-aware tokens, so `compute(1)` → `compute(2)` marks only the number |
| Watching | notify (FSEvents/inotify), changes only | Live diff for the worktree on screen; diffs of agents off screen follow their file changes within seconds, no polling. With inotify, build, dependency and git folders (`target/`, `node_modules/`, `bin/` except Rust's `src/bin/`, `obj/`, `dist/`, `.git` beyond its refs) are never watched |
| IPC | Unix socket, JSON lines | One socket for hooks, `atomics ctl` and `atomics mcp`; no async runtime anywhere |

Agents run as children of the ATOMICS process, so closing ATOMICS stops them. Claude sessions in a project can be resumed from the sidebar,
and Codex with `codex resume --last`.
