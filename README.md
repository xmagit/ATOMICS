<img src="assets/icon.png" alt="atomisk.ai icon" width="96">

# IMACS

[![CI](https://github.com/xmagit/IMACS/actions/workflows/ci.yml/badge.svg)](https://github.com/xmagit/IMACS/actions/workflows/ci.yml)

Emacs, but for agents: a lightweight, review-first workspace for coding agents, written in Rust. Run Claude Code,
Codex and shells side by side, each in its own git worktree. Click any agent's title to see its worktree's diff
against main, comment on lines, and send the review straight to the agent while it keeps working. Or let an
orchestrating agent plan the work, hand out tasks and review the results while you approve and steer.

An atomisk.ai project.

The command is `imacs`. `scripts/macos-app.sh` builds IMACS.app for the Dock; `scripts/linux-install.sh` installs
the Linux desktop app. State stays in `~/.imacs` on macOS and existing installs; new Linux installs use
`${XDG_STATE_HOME:-~/.local/state}/imacs`. Config lives in `${XDG_CONFIG_HOME:-~/.config}/imacs`.

## What it does

- **Agents in real terminals.** Every agent runs its own TUI in an embedded terminal, so nothing is lost compared to
  running it yourself.
- **The diff against main, always.** The diff panel follows the active agent. It covers committed, staged, unstaged and
  untracked changes against the merge-base with `base_ref`, so main moving on never shows up as the agent's work. It
  updates live, with syntax highlighting and word-level emphasis.
- **Side by side when there is room.** A diff area at least 1100 points wide shows the old version left and the new one
  right, each edited line beside what it replaced. `diff_split` under `[layout]` picks `auto`, `always` or `never`; the
  **split** toggle in the diff header overrides it until IMACS quits. Comments and double-click-to-edit work the same in
  both views.
- **Ungrouped sessions for everything else.** Research and small tasks need no project or board. **+ New** (`cmd+t`)
  starts a session in a scratch folder of its own under the state directory (`<state>/scratch/<id>`), in the
  **Ungrouped** tab, which holds every session outside a plan. It takes nothing from the focused tile: no folder, no
  title. Its first prompt names it (typed in the dialog, sent from the steer bar, or typed into its terminal); rename
  it from its tile title (double-click) or its sidebar menu. It follows the isolation settings like any agent, and
  the dialog can make one session bare or sandboxed. Each scratch folder is a git repository without commits, so the
  diff panel shows everything written there and review comments reach the agent. Closing the session deletes the
  folder and its sandbox; quitting IMACS ends scratch sessions for good, while `imacs restart` resumes them. Pick
  **Existing folder** or **New worktree** in the dialog to work in a project instead.
- **Projects and workspaces.** `imacs <folder>` opens the repository it is in, always through its main checkout, so a
  folder inside any worktree opens the same project. The explorer shows the main checkout's tracked files, with the
  changes of the worktree under review laid over them, plus each project's worktrees. **Project › Open project…**
  adds more repositories; save the set as a workspace and reopen it with `imacs --workspace <name>`.
- **Changed files beside the diff.** A "changed" column next to the diff/editor lists what the worktree changes. In
  diff mode a click jumps to the hunks; in editor mode it opens the file at its first change. Drag the divider
  between agents and the panel to resize it (double-click resets).
- **Editor mode.** `cmd+e` (or double-clicking a diff line) swaps the diff for a real editor, Helix by default, at
  that line. Inlay hints, code actions and go-to-definition come from your language servers; the **inlay hints**
  button toggles them. One editor runs per worktree and keeps its state while you switch back to the diff. Neovim
  works too; see `[editor]` in the config.
- **Worktree clean-up.** **clean up…** in the explorer checks every worktree for uncommitted work, merged branches
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
- **Remote control and MCP.** Other agents can start, read, review and steer agents in IMACS (see below).

## Run

On a fresh Mac or Ubuntu machine, run `imacs setup` after installing IMACS. It guides tool
installation, Docker sign-in, network policy, credentials over stdin and Linear authorization,
and resumes completed steps on rerun. See [fresh-machine setup](docs/isolation.md#fresh-machine-one-guided-command).

```sh
cargo install --path crates/imacs     # or: cargo run --release -p imacs
imacs                                 # opens the repository you are in (or your last projects)
imacs ~/repos/app                     # opens that project
imacs --workspace daily               # opens a saved workspace
imacs --open claude:~/repos/app --open codex:~/repos/app-feature
```

On macOS, `scripts/macos-app.sh` builds `~/Applications/IMACS.app` (release binary, the atomisk.ai icon, microphone
permission text), so IMACS runs from the Dock or Spotlight and keeps running when terminals close:

```sh
scripts/macos-app.sh
open -a IMACS --args --workspace cz3backend
```

On Linux, install the build dependencies first (Debian/Ubuntu example), then install or update from this checkout:

```sh
sudo apt-get install build-essential pkg-config libasound2-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
scripts/linux-install.sh
imacs --workspace daily
scripts/linux-install.sh --uninstall  # removes the binary, desktop entry and icon; keeps config/state
```

The installer builds a release binary for the native Linux target, atomically replaces `~/.local/bin/imacs`,
including while IMACS is running, and runs the installed `imacs setup` with your terminal attached.
Setup guides missing tools, KVM access, Docker sign-in and credentials; updates reuse completed steps.
If setup is interrupted, rerun `imacs setup` to resume. Add `~/.local/bin` to PATH for terminal use. The desktop entry and icon go under
`${XDG_DATA_HOME:-~/.local/share}`; rerunning updates the same files. Both Wayland and X11 are enabled. eframe supplies
embedded fallback fonts, so the UI and terminal do not require macOS fonts; system fonts can also be used. A working
Vulkan driver (for example Mesa's `mesa-vulkan-drivers`) supports the default wgpu renderer. File dialogs use the desktop
portal and external files open through `xdg-open` (`xdg-utils`).

The config is created on first start at `${XDG_CONFIG_HOME:-~/.config}/imacs/config.toml`. It is commented and reloads while IMACS runs: agents
and their commands, `base_ref`, layout breakpoints, fonts, colors, terminal scrollback, shortcuts, and the texts used for
steering. Themes (built-in `claude`, `midnight`, `atomisk` and the light `paper`, or your own theme files) are described
in [docs/themes.md](docs/themes.md).

On Linux, a private, user-owned `XDG_RUNTIME_DIR` holds each new install's socket and the compiler jobserver FIFO,
in an `imacs/<instance hash>` subdirectory, with the FIFO isolated inside `jobserver/`. Without it, the socket stays beside the state and the FIFO uses the user's
temporary directory. Existing `~/.imacs` installs retain their state and socket paths; the FIFO can use XDG runtime storage.
`IMACS_HOME` always overrides the state path and keeps the socket beside that state; `IMACS_SOCKET` overrides just the
socket, and `IMACS_CONFIG` overrides the config file. Runtime FIFO paths are distinct for each state home. Empty or relative
XDG paths are ignored. The earlier `~/.cox` and config rename migration still runs without overwriting existing state.

### Restart onto a new build

Rebuild and install into the same location first (`scripts/macos-app.sh` on macOS, `scripts/linux-install.sh` on Linux). The built-in chief can then call
`imacs_restart` with an optional `reason`. It is available only in the chief's `imacs mcp --chief` server; ordinary
orchestrators and task agents do not receive it. The equivalent local control request is:

```sh
imacs ctl '{"cmd":"restart","reason":"installed the new build"}'
```

Restart refuses and lists blockers while another agent is working or starting, an approval or question is pending,
messages await delivery, a queue slot is held or waiting, a worktree/cleanup or integration job is running, or a plan
brief is still focused for editing. Finish the edit and wait for agents and queued work. Idle and accepted agents
are fine; the chief's own turn can hand off. `force: true` bypasses those busy checks and may interrupt work, but never
bypasses saving state or validating that the installed executable exists and successfully runs `--version`.

IMACS saves every plan, the session store (including Claude resume IDs and the chief's role), and the layout, then
quits gracefully. A relauncher in its own session ignores SIGHUP, waits for the old PID to exit, and starts the same
installation: `open -n -a` on the exact `.app` bundle on macOS, or the same binary on Linux, with the original arguments.
The macOS path requires a `.app` bundle; a development binary outside one refuses to restart. Instance home, socket
and config overrides are preserved, and helper output goes to `restart.log` under `IMACS_HOME`.

Live sessions with resume IDs reopen automatically. The chief resumes as chief, with its dock and width preserved,
and starts a turn asking it to call `imacs_wait`. The first wait receives a `restarted` event, even with an old cursor
or a status filter, containing `restarted: <old build> -> <new build>, reason …`. Build identities come from
`git describe --always --tags --dirty` at compile time, falling back to the crate version. `imacs --version` prints it.

## Using IMACS on several machines

Share `~/.config/imacs/config.toml` between your machines and put machine overrides beside it in
`config.d/<hostname>.toml`. `IMACS_HOST` overrides the machine name (for example, `IMACS_HOST=work-mac imacs`).
On macOS the default is the stable LocalHostName from System Configuration (`scutil --get LocalHostName`), with
the kernel hostname as fallback. Linux uses the kernel hostname. A trailing `.local` is stripped.
Names are case-sensitive and allow letters, numbers, dots, underscores and hyphens.
Empty or invalid overrides fall back to the machine name; if that is unavailable, the name is `localhost`.
The same name selects both the override file and the machine's workspaces.

Configuration loads in this order: built-in defaults, `config.toml`, then the selected host file. Tables merge
recursively by key: overriding `[resources.dotnet].processor_count` keeps other resource settings. Scalars and
arrays replace the base value, so a host's `repos` or `agents` list replaces the whole list; only `[chief] prompt`
adds to the main file's instead (see [docs/configuration.md](docs/configuration.md)). The host file is optional;
an unreadable or invalid existing file is an error. Both files reload while IMACS runs, including when you create
or remove the host file. `imacs doctor` prints the selected host and every loaded config file in order.

For example, the shared `config.toml` can contain:

```toml
repos = ["~/repos/imacs"]
worktree_path = "~/repos/{repo}-{slug}"

[font]
size = 13.0
diff_size = 12.5
```

And `config.d/work-mac.toml` can contain:

```toml
repos = ["~/repos/cz3backend"]

[font]
size = 15.0 # diff_size stays 12.5
```

Use `~` or `~/…` for repository and workspace project paths and worktree templates. They expand against the
current machine's home directory when used; the shared config keeps its original portable strings. The default
worktree template, `{repo_parent}/{repo}-{slug}`, uses the current repository's parent and also needs no machine path.
Keep machine-specific absolute paths out of shared config. `IMACS_CONFIG` selects a different base file; its sibling
`config.d/` and `workspaces/` directories are used too. Otherwise `XDG_CONFIG_HOME` is honored.

Saved workspaces are per host: `~/.config/imacs/workspaces/<host>/<name>.toml`. Home and work can both have a `daily`
workspace with different projects. Loading checks the host directory first, then falls back to flat
`workspaces/<name>.toml` files saved by older IMACS versions. The workspace list combines both locations, with the
host file winning on a name clash. Saving always writes to the host directory and leaves flat files untouched,
so existing launch commands keep working and a shared config directory stays usable by other machines.
Saving projects under your home writes `~/…` paths. For example, `workspaces/home-linux/daily.toml` can contain:

```toml
projects = ["~/repos/atomisk"]
```

A workspace file can also hold its own `[tickets]` section, so work can follow Linear and home Relay. It replaces
the main config's `[tickets]` as a whole, so copy a complete profile: the Relay or Linear block in the default
config. See [docs/tickets.md](docs/tickets.md). Loading and saving a workspace keep the section as written.
Keep runtime state (`~/.imacs`, including sessions, sockets and plans) local to each machine.

## Testing on Linux

Docker Desktop (or Docker Engine on Linux) can build, test and capture IMACS without installing a Linux toolchain
on the host. Start Docker first; `scripts/linux-qa.sh` checks `docker info` and reports if it cannot reach the daemon.
Run from the checkout you want to test:

```sh
imacs queue --class test -- scripts/linux-qa.sh test
imacs queue --class qa -- scripts/linux-qa.sh screenshot /tmp/imacs-linux.png
imacs queue --class qa -- scripts/linux-qa.sh screenshot /tmp/imacs-linux-wide.png --size 1920x1080
imacs queue --class qa -- scripts/linux-qa.sh screenshot /tmp/imacs-scratch.png \
    --ctl '{"cmd":"start","agent":"shell","isolation":"bare","prompt":"echo hi"}' --ctl '{"cmd":"activate","session":"echo hi"}'
scripts/linux-qa.sh shell
```

`test` runs the full locked workspace suite, including doc tests, and keeps running other test targets if one fails.
`screenshot` builds IMACS, starts a disposable project with a shell under Xvfb, waits for its isolated control socket,
captures the window through `imacs ctl`, verifies the PNG size and exits. Each `--ctl` request is sent to that
instance first, in order, to set the scene; one that fails stops the run. The default size is 1600x1000; supported
sizes range from 480x320 to 8192x8192. The image uses Debian, Rust 1.98, Xvfb and Mesa's lavapipe Vulkan driver
(software rendering for wgpu). No display server, GPU, agent credentials or IMACS state from the host are shared.

The repository and linked-worktree git metadata are mounted read-only. Each checkout gets a separate Docker target
volume labelled with its checkout path; each run removes labelled target volumes whose checkout is gone and which
no running or stopped container uses. Unlabelled volumes and the shared Cargo registry and git volumes are kept.
Cargo registry and git downloads are cached across checkouts. The image is cached by its Dockerfile checksum,
and the builder receives only `Dockerfile.linux-qa`, never the source tree. To refresh the same image's OS packages,
remove its `imacs-linux-qa:<checksum>` tag and rerun. Images are kept until removed manually.

Containers drop all Linux capabilities, so permission checks behave normally even with the image's root user.
They default to two CPUs, two Cargo jobs, two test threads and 8 GiB of memory. Override CPU, job or memory limits
with `IMACS_LINUX_QA_CPUS`, `IMACS_LINUX_QA_JOBS` and `IMACS_LINUX_QA_MEMORY`. The host's jobserver FIFO cannot cross
Docker Desktop's VM boundary, so these limits apply in addition to the host queue slot. Queue commands may wait for
a slot and a cold build can take several minutes; agents should use a long timeout or background execution and request
access outside their sandbox if the IMACS or Docker socket is blocked.

For incremental compilation, use `scripts/linux-qa.sh shell` and run `cargo check --workspace --all-targets --locked`.
During development run only tests you change; reserve the queued full suite for the end. If using the shell for a heavy
test run or a GUI instance, queue the shell itself with the matching class. Shell changes outside `/repo` are disposable;
the target and Cargo caches persist.

Real Docker Sandboxes need KVM and cannot run inside this container. On the Ubuntu 24.04+ desktop,
finish `imacs setup`, then run:

```sh
imacs queue --class qa -- scripts/linux-sandbox-smoke.sh
# Rehearse the lifecycle without KVM (inside scripts/linux-qa.sh shell, under a QA slot):
xvfb-run -a dbus-run-session -- scripts/linux-sandbox-smoke.sh --fake-sbx --imacs /target/debug/imacs
```

The smoke script runs doctor, starts an instance with its own `IMACS_HOME`, `IMACS_SOCKET`, `IMACS_CONFIG`,
runtime and temporary directories, and creates two shell agents in disposable worktrees. It verifies terminal
output and workspace file synchronization, measures 20 control round trips (maximum 1000 ms by default),
then removes its app/helpers, sandboxes, MCP registrations, worktrees and temporary directories and checks
for leftovers. It prints a pass/fail summary and exits nonzero on failure, including cleanup failures.
Your normal HOME, sbx sign-in and global policy are reused; a missing policy must be initialized with setup first.
It never removes other instances' sandboxes or MCP servers. Use `--imacs /path/to/imacs`, `--timeout 900`
and `--latency-ms 1000` to select an already built binary or adjust the startup/latency limits. `--fake-sbx`
uses the repository fixture and reports explicitly that VM isolation and KVM were not exercised.

## Troubleshooting

Run `imacs doctor` to check git, Claude Code, Codex, Helix, the config, the running IMACS socket and global Claude hooks.
It also checks Codex hook support (0.165+), reports hook trust as unverified with `/hooks` instructions, reads plan files,
warns about cargo entries in `[approvals].allow`, and lists merged worktrees safe to review for cleanup. It changes nothing.
Each failed check includes a hint. Missing git or an unreadable or invalid config returns exit code 1; missing
agents, Helix, a running IMACS or hooks do not affect the exit code. Tools are checked on your login-shell PATH.

## Usage

The sidebar's **USAGE** panel shows account-wide session and weekly limits remaining, reset times and report age.
Claude reports through its statusLine; Codex is polled in the background every 60 seconds from the newest rollout
under `$CODEX_HOME/sessions` (default `~/.codex/sessions`), reading only its last 16 KiB for a `token_count` rate-limit event.
**no data yet** means no report has arrived or a reported window has expired. A window omitted by a received report
shows **not reported by this account**. Limits are shared across sessions; weekly reset days are UTC.

Claude sessions started by IMACS use `imacs statusline --socket <path>` automatically. It reads Claude's JSON from stdin,
forwards usage to IMACS, then chains to your own `statusLine.command` with the same input and output. User, project and
project-local settings keep their usual precedence. Without a custom command it prints a short usage line.

## Claude sessions anywhere

Claude agents that IMACS starts get its hooks through `--settings`. Codex 0.165+ gets inline
`-c hooks.<Event>=...` settings; older versions keep `busy_text` status tracking.
Version detection runs in the background with a 30-second limit. Successful results are shared;
failed or timed-out probes are retried on the next start.

On the first Codex launch from an IMACS install, review and trust the IMACS hooks in Codex's
startup prompt (or `/hooks`). Their definitions stay identical across panes, so this is a
one-time step until the install path, socket or hook definitions change. Repository hooks keep
their own trust review; IMACS never bypasses hook trust. Codex sessions with hooks run with
`--no-daemon`, so a hook's process ancestry identifies its terminal reliably. `SessionStart`
maps the Codex session ID to that pane, including when several panes share a worktree.
`imacs doctor` prints the trust instructions. To make Claude sessions in any other terminal show up too
(status, diff, and messages delivered at their next tool call or turn end), add the hooks to your user settings:

```sh
imacs install-hooks      # merges into ~/.claude/settings.json, keeps a backup
```

## Permission approvals

Codex permission hooks show **needs you**, the command, working directory and reason on the tile.
`imacs_wait` wakes with that request in the event's `approval` field, plus `plan` and `task` ids when linked to a plan.
`imacs_list` and `imacs_result` also include the oldest pending request. The chief can answer with
`imacs_approve {session, decision: "allow" | "allow_always" | "deny", message?}`.
`allow_always` remembers the exact command, tool and directory for this session only; it does not
write a global rule. Approval decisions and held input target the specified session, even while another plan tab
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
`imacs queue` is judged by the command it executes, including these env and toolchain forms.

Pipes, redirections, substitutions, globs, newlines, comments, unsupported shell syntax, paths
escaping through symlinks and read-tool options that execute programs or write files still wait
for approval. Expansion syntax remains refused even inside quotes. Cargo's `--manifest-path`,
`--config`, `--target-dir`, `-Z` and `-C` options also wait, including joined forms, even with an
explicit cargo entry. Unknown commands always wait for the chief.
Set `allow = []` to make every permission request wait. This policy handles requests Codex raises;
it does not add approval prompts to commands Codex already permits.

Pending permissions appear under the agent's header with the command, cwd, reason and **Allow**, **Allow always**
and **Deny** buttons. They use the same handler as `imacs_approve` and remain available while the chief reviews.
With a running chief, IMACS waits up to nine minutes for a hook decision, below Codex's ten-minute permission-hook
timeout. Without a running chief it waits ten seconds, then leaves Codex's native permission menu
available so you can answer in the terminal. After that, `imacs_approve` selects the matching
visible menu choice; it returns an error if no matching menu is visible yet. For `allow_always`,
IMACS remembers the command and selects the one-time allow choice rather than creating a broader
Codex prefix rule. A denial message can only be supplied while the hook is waiting.
Truncated commands match their visible prefix. If a long reason hides the command, `imacs_approve`
expands it with Ctrl+A and asks for a retry so the command is checked before selecting a choice.

While a permission request is pending, `imacs_send` holds plain messages and returns `held: true`.
It releases these messages after a lifecycle event confirms the request has finished, or after the
menu has been absent for one second. This also covers requests answered directly in the terminal.
Raw input is rejected with an error pointing to `imacs_approve`.

Codex sends wait for the complete message to appear in the transcript or visible queue, or for its
prompt hook. IMACS retries unconfirmed input and returns an error if delivery cannot be confirmed;
another message cannot replace a send still awaiting confirmation.

Agent terminals inherit no `IMACS_SOCKET`, `IMACS_PANE`, `IMACS_HOME` or `IMACS_CONFIG`.
Codex hooks receive the socket as an argument and find their pane from process ancestry or
the mapped Codex session ID; Claude hooks receive socket and pane arguments. MCP servers receive
their own routing environment. An IMACS launched from an agent therefore uses its own defaults; explicitly set
`IMACS_HOME`, `IMACS_SOCKET` and `IMACS_CONFIG` when launching a separate test instance.
An explicit `IMACS_HOME` also gives bare agents their own `CODEX_HOME` (`<home>/codex`) and
`CLAUDE_CONFIG_DIR` (`<home>/claude`). Codex's per-event hook trust slots and Claude's folder trust
then belong to that instance. Sign in separately in those provider homes when needed.

## The chief agent

Click the **CHIEF** pill at the top (or `cmd+o`) to open the chief: Claude in a large window in the middle of the
screen, started in the project's main checkout with every IMACS tool and a built-in way of working (plan, wait for your
approval, dispatch, verify, end with a fresh reviewer, never push without asking). Close the window to minimize it to
the pill. The **dock** / **float** button in its header switches between the floating window and a full-height column
beside the agents; the pill's context menu offers both modes too. Drag the dock's right edge to resize it, or double-click
to reset it. Mode and width are remembered; `[chief] dock = "floating"` (or `"docked"`) sets the initial mode.
`cmd+o` shows or hides either mode. On narrow windows the chief floats instead, then docks again when the window widens.
`cmd+shift+space` dictates to it.

Every `[chief] heartbeat_secs` (default 300 seconds), IMACS checks every open plan for missing or exited
agents, running tasks whose agent has been idle or turn-done for over ten minutes without reporting since that
status began, pending approvals or Codex menus, messages held for over a minute, and working agents without an event for fifteen minutes. Include
`heartbeat` in `imacs_wait` statuses to receive one fleet event with a `findings` list; an empty list means all good.
The chief pill shows a compact age and a dot when that check found something; hover for the full local check time.

The chief may read, plan, review and run the plan you approved without asking. It can also answer other
agents' permission prompts through the pre-approved `imacs_approve`, including `allow_always` for the rest
of a session. Starting agents, typing into terminals and closing sessions still ask for permission.
It verifies a sandboxed task inside its sandbox, with the pre-approved `imacs_exec`, which reaches no more
than that task's agent does: agent code never runs on your machine for a check. `imacs_exec` waits for the
same test and QA slots as `imacs queue`, and is refused for bare tasks, whose checks run on this machine; see
[Checking sandboxed work](docs/isolation.md#checking-sandboxed-work).

The chief can help configure IMACS too. `imacs_config_get` shows every effective setting and the file it comes
from, with secrets masked; `imacs_doctor` returns the doctor's checks; `imacs_config_propose` proposes a change to
`config.toml`, this machine's `config.d/<host>.toml` or the workspace file. Each proposal waits in the IMACS window
as a diff, with settings that run commands labeled, until you apply or reject it. Applying backs the file up
beside itself, keeps its comments and reloads. Your own instructions for the chief go in `[chief] prompt`; each of
those files adds its own. See [docs/configuration.md](docs/configuration.md) for scopes, merge order and the trust model.
Commit `IMACS.md` and declarative `.imacs/config.toml` to share project instructions, branch templates and lifecycle
settings with a team; see [Repository workflows](docs/repo-workflow.md).

## Orchestration: several plans you approve, agents that report back

An orchestrator (a Claude session with the IMACS MCP server) turns a goal into a plan with `imacs_plan_set`: tasks with a
brief, acceptance checks, dependencies, and which agent and model should do them. Without `plan`, this creates a new
plan and returns its unique id; pass `plan: "<id>"` to update that plan and to get, update, dispatch or integrate its
tasks. Rewording an existing plan’s goal preserves its task progress and integration branch.
With several open plans, update/dispatch/integrate require the id and errors list the open plans.
`imacs_plan_get` without `plan` lists open plans with their goal, done/total, approval and waiting tasks. Select a
plan’s tab, then open its **plan board** with `cmd+b` or **Plan** in the top bar. It has columns for planned, running,
blocked, review and done. Edit briefs,
then **Approve plan**: nothing starts before that.
You can also approve a reviewed plan with `imacs ctl '{"cmd":"plan_approve","plan":"<id>"}'`.
Changing only branch names keeps approval; changing the goal or task requirements requires review again.

`imacs_plan_dispatch` (or **Dispatch ready** on the board) gives each ready task its own worktree and an agent whose
first message is the brief. Every agent IMACS starts gets a `imacs_report` tool, pre-approved, to say when it is done,
blocked or has a question. Reports land on the task card, move done tasks to review, and wake the orchestrator's
`imacs_wait`, which then reads `imacs_result` and `imacs_diff`, reviews, steers, and accepts with `imacs_plan_update`.

Plans are stored separately in `plans/<id>.json` under the state directory, with status `open`, `completed` or `archived`. The old
`plan.json` in that directory migrates once and is kept as `plan.json.migrated`; an old plan whose tasks are all done
migrates as completed. Saves replace files atomically, and unreadable plan files are listed in the startup message.
Open and completed plans get tabs; archived plans are hidden. Select a tab to see
that plan’s sessions; `cmd+b` opens its board. Other sessions remain under All, Ungrouped and their project tabs. Each plan has
its own approval and automatic final review. Use `imacs_plan_close {plan, status: "completed" | "archived"}` or
**Complete plan** / **Archive plan** on the board to close it after confirmation. Reopen either kind from
**Closed plans** beside the plan tabs; approval and task history are preserved. Its history remains available by id, and once only
one plan is open, mutation tools can omit its id again. Start the next plan while another finishes; the chief is never closed.
`imacs_wait` events include `plan` and `task` ids for sessions belonging to a task.

Open a completed plan’s ✓ tab and its board. **Clean up plan…** previews only its recorded
task and integration branches and their worktrees. Once the integration branch or every task branch is contained
in the configured base (usually main), select the entries to remove. Uncommitted changes, unmerged commits and
worktrees used by agent sessions are kept with a reason. Removal rechecks the preview and never forces Git;
branches without worktrees can be removed too. The saved plan history stays available.

Tasks come together on the plan's **integration branch** (`imacs/plan-<plan-id>` unless the plan names one; the board
shows it). `imacs_plan_integrate` (or **integrate** on a review or done card) commits whatever the task left uncommitted
on its branch (as `<title> (<id>)`, with the repository's own git identity; ignored files stay out) and merges the
branch into the integration branch. The merge happens in a worktree of its own next to the repository, created on
first use like any other worktree, so your main checkout is never touched. On a conflict the merge is aborted and the
task stays in review with a note listing the conflicted files. Tasks without dependencies branch from `base_ref`;
tasks with dependencies branch from the integration branch, so integrate accepted tasks before dispatching the tasks
that build on them. Returning a task to running clears its integration marker. Background checks verify that each marked task branch tip is an ancestor of the integration branch. The board
and dispatch use the same verified readiness, so new commits must be integrated too.

Each open plan has a **short name · done/total** tab with a status dot (working, needs you, all done).
Dispatching the selected plan returns from its board to its fleet and minimizes a floating chief; a docked chief
stays beside the fleet. Updating or dispatching another plan marks its tab with `*` and preserves the selected
view and chief. Creating a new plan selects its tab. Each fleet shows only sessions linked to that plan’s tasks.
Click the plan tab later to return to that overview, or choose All or a project tab to see other sessions.

Accepting an integrated task closes its agent; tasks without a branch, including the final review, close on acceptance.
Its completed tile fills an otherwise empty grid cell, showing the duration, last report, diff size, **diff** and
**resume**. Live agents keep their tile sizes and priority. Idle agents waiting for review show when they reported.
When every task is accepted, the plan tab shows ✓ and its board offers **Archive plan**. The chief stays running and
worktrees remain for the existing **clean up…** flow. Set `[plan] close_done_agents = false` to keep agents running.

## Tickets

Plans can follow a ticket system that has an MCP server, such as Relay or Linear. In board mode a board in the
ticket system is a plan and its tickets are tasks; in ticket mode one ticket is a plan. IMACS talks to the server
itself (stdio, or streamable HTTP with a token from the environment). A `[tickets]` section in the workspace file
or the config maps each generic operation (get, list, update state, comment, create) to one of its tools. Swapping
systems changes only that section. Cards show each ticket's live key and state, and a click opens it. IMACS moves
ticket states as tasks move and comments on dispatch, reports, acceptance, blocks and completion, signed with
the reporting model. Changes made in the ticket system reach the chief as `ticket` events in `imacs_wait`.
Failures show on the card and never block a plan. See [docs/tickets.md](docs/tickets.md) for the design, the
config reference and examples.

## Agents in sandboxes

By default each agent runs in its own [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) microVM,
which sees only its worktree and git's write folders. Claude agents are the exception and run bare by default:
a Claude subscription cannot authenticate in a local sandbox. IMACS creates the sandbox when the agent starts,
attaches the tile to it with `sbx run`, reattaches after a restart and removes it with the worktree. It
needs `sbx` v0.50.1 or newer, signed in. Without it, sandboxed agents are refused, never started bare.
Set `[isolation] mode = "bare"`, or `isolation = "bare"` on one agent, to run agents directly on your
machine. A repository can require sandboxes in `.imacs/config.toml`, but never turn them off. See
[docs/isolation.md](docs/isolation.md) for the settings and the trust model.

## Running many agents

Every agent works in its own worktree with its own `target/`, so its builds never wait on another's.

New agent worktrees start with a warm build cache when the main checkout has one. Before launching the agent,
IMACS clones the folders in `[worktrees] warm` (default `["target"]`) from the repository's **main checkout**, even
when you start from another linked worktree. The clone runs on the background worktree job. Only copy-on-write
clones are used: unsupported filesystems, missing folders and clone errors are skipped without retrying as a full
copy or blocking launch on a fallback. macOS uses the native clone-only `clonefile` API
(`cp -c` can silently fall back to full copying); Linux uses `cp -R --reflink=always`.
On btrfs and reflink-enabled XFS, caches on the same filesystem can be cloned. Filesystems without reflinks (including
ordinary ext4), cross-filesystem clones and unsupported files skip warming and launch cold. A failed clone leaves no
partial cache, and IMACS never retries it as a full copy. Use `imacs doctor` to check the actual filesystem pair.
Each worktree then owns its cache independently; builds cannot change the main checkout's cache. A successful
clone shows **warm cache from main** in a toast. Existing worktrees are left alone when retrying a launch.
`imacs doctor` probes CoW cloning between the repository and configured worktree folders, using temporary files
that it removes afterwards.

```toml
[worktrees]
warm = ["target"]          # existing folders relative to the repository root; nested paths also work
# warm = ["target", "build/cache"]
# warm = []               # turn cache warming off
```

Measured once per configuration on this Mac (APFS, 2026-10-29), with sccache disabled. IMACS itself created and
launched both agent worktrees at revision `63dcfd1`, using separate isolated instances with their own
`IMACS_HOME`, `IMACS_SOCKET` and `IMACS_CONFIG`. Each agent ran its first
`imacs queue --class test -- cargo build -p imacs`:

| Cache warming | Worktree creation → launch | Crates compiled | First queued build | Preparation + build |
| --- | ---: | ---: | ---: | ---: |
| Off (`warm = []`) | 0.18 s | 276 | 56.30 s | 56.48 s |
| On (`warm = ["target"]`) | 6.83 s | 13 | 22.24 s | 29.07 s |

Preparation includes the cache clone; build times are wall clock and include queue overhead. Warming cut
preparation plus first-build time by about 49% in this run. The cache was present before the warm agent's first
command, and absent before the cold agent's first command.

Agents share the machine, and IMACS keeps that in check for every agent it starts (`[resources]` in the config):

- **One compiler job pool.** IMACS owns a GNU make jobserver (a named pipe in a private folder under Linux's
  `XDG_RUNTIME_DIR`, or the user's temporary directory otherwise) with `jobs` tokens, cores minus 2 by default, and hands it to every agent terminal as
  `CARGO_MAKEFLAGS`. cargo takes a token per job, so ten agents building at once share those jobs instead of each
  using every core. On Linux, Codex also gets `--add-dir` for this instance's private XDG runtime `jobserver/` subdirectory so its
  sandboxed compilers can open the FIFO. `MAKEFLAGS` is left alone, since `make` before GNU make 4.4 misreads a pipe jobserver. A job
  killed while holding a token never returns it, so whenever no compiler or build script runs, IMACS resets the
  pool to full. cargo passes the pool on to build scripts, and a build script that runs Apple's `make` 3.81 (some
  `-sys` crates, vendored OpenSSL) stops with "invalid --jobserver-fds"; set `jobs = 0` to turn the pool off, or put
  GNU make 4.4 first on PATH.
- **A queue for heavy work.** `imacs queue --class test -- cargo test` waits for a free test slot (one per 6 cores
  by default), runs the command and passes its exit code through. `--class qa` is for app instances started for
  screenshots. The slot is released when the command ends or its process dies. Tiles show `queued for a test slot
  (2 in line)`, and the approval policy judges a queued command by the command it runs (only a bare `imacs`
  counts). Without a running IMACS the command runs right away; while IMACS restarts, waiting commands reconnect
  and queue again instead of all running at once. A sandbox that blocks the IMACS socket (Codex's does) gets a
  clear error and exit code 75: run the command outside the sandbox. Queued commands may wait, so give them a long
  timeout.
- **Lower priority.** Agent terminals start at niceness `agent_nice` (10), so IMACS and your desktop stay responsive.
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
  the full suite once, at the end, through `imacs queue`. For .NET suites use
  `imacs queue --class test -- dotnet test`.

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
IMACS does not inject VSTest-specific arguments into every project, since Microsoft.Testing.Platform interprets
them differently. The [SDK 9 measurements and reproduction scripts](docs/dotnet-resource-budget.md) show the
three-build peak falling from 25 .NET processes to 12 (5.18 s to 6.49 s on the shared 18-core machine).

The `load` gauge in the top bar shows the 1-minute load per core (grey, then yellow from 0.7 and red from 1.0); hover
for the pool size and the slots in use. If IMACS ever ends without a clean quit, the next start says so and
`crash.log` in the state directory (`~/.imacs` on macOS/existing installs) has the panic and its backtrace when there was one.

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

The key comes from the environment variable named by `api_key_env`. If IMACS doesn't have that variable (apps started
from the Dock get a bare environment), it reads the variable from your login shell. `api_key` in the config also works
but is discouraged. IMACS never logs or shows the key. macOS asks for microphone access the first time you record, for
the terminal you started IMACS from or for IMACS itself when it runs as an app.

## Let an agent drive IMACS (MCP)

```sh
claude mcp add IMACS -s user -- imacs mcp
codex mcp add IMACS -- imacs mcp
```

Tools: `imacs_wait` (block until an agent finishes a turn or needs input; returns a cursor for the next call),
`imacs_approve` (allow, allow_always or deny a pending permission request),
`imacs_result` (an agent's last prompt, final message and screen), `imacs_list`, `imacs_start` (in a folder, a new worktree from `repo` + `branch`, or with neither in a scratch folder; `isolation` overrides the settings for that session), `imacs_send` (a message, or raw
keys for menus), `imacs_read` (screen text), `imacs_diff`, `imacs_comment` + `imacs_send_review`, `imacs_open` (show a file to the user), `imacs_focus`, `imacs_close`.
Comments an agent leaves show inline in the diff panel before they are sent, so you can see what it asks for.

`imacs ctl '<json>'` sends the same requests from a shell, e.g. `imacs ctl '{"cmd":"list"}'`. A few extras exist for
scripting and testing: `show_diff`, `show_chief`, `show_settings` (the Settings menu and its theme picker), `resize`
and `screenshot`. For example,
`imacs ctl '{"cmd":"show_chief","open":true,"dock":"docked","width":720}'` shows the dock and remembers its width.

## Shortcuts

`cmd` is Command on macOS and Ctrl+Shift on Linux, so it never collides with the terminal's plain Ctrl keys.
For example, `cmd+p` is Ctrl+Shift+P on Linux. `cmd+shift+space` becomes Ctrl+Shift+Space (Shift is applied once).
Tooltips and the settings menu show the actual platform chord, including custom bindings.

| Key | |
|---|---|
| `cmd+p` | go to file |
| `cmd+e` | diff ⇄ editor |
| `cmd+b` | plan board |
| `shift+↓` / `shift+↑` | next / previous changed file (from anywhere but a text field or the editor) |
| `cmd+d` | show/hide the diff |
| `cmd+j` | next agent that needs you |
| `cmd+l` | steer bar |
| `cmd+shift+space` | voice: record, press again to send to the chief, `Esc` cancels |
| `cmd+t` | new session (scratch folder, existing folder or new worktree) |
| `cmd+r` | refresh diff |
| `cmd+w` | close session |
| `cmd+1…9` | switch session |

## Design

Two crates. `imacs-core` holds everything testable without a window: config, the git diff engine and parser, the
responsive layout, the hook and control protocols, and review message composition. `imacs` is the app.

| Concern | Choice | Why |
|---|---|---|
| UI | egui/eframe (wgpu) | Pure Rust, immediate mode: layout is plain code each frame, so responsive tiling is simple; idle when nothing changes |
| Terminal | alacritty_terminal | Mature VT emulation and PTY event loop; Claude's and Codex's TUIs render correctly |
| Git | `git` CLI | Exact porcelain semantics, and picks up `core.fsmonitor`/`untrackedCache` on large repositories |
| Highlighting | syntect + bat's grammars (two-face), Oniguruma | Works line by line, which suits hunks that start mid-file; Oniguruma is ~3× faster and ~4× smaller than fancy-regex |
| Word diff | similar | Code-aware tokens, so `compute(1)` → `compute(2)` marks only the number |
| Watching | notify (FSEvents/inotify) | Live diff for the worktree on screen; diffs of agents off screen follow their file changes within seconds, no polling |
| IPC | Unix socket, JSON lines | One socket for hooks, `imacs ctl` and `imacs mcp`; no async runtime anywhere |

Agents run as children of the IMACS process, so closing IMACS stops them. Claude sessions in a project can be resumed from the sidebar,
and Codex with `codex resume --last`.
