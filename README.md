<img src="assets/icon.png" alt="atomisk.ai icon" width="96">

# IMACS

[![CI](https://github.com/xmagit/IMACS/actions/workflows/ci.yml/badge.svg)](https://github.com/xmagit/IMACS/actions/workflows/ci.yml)

Emacs, but for agents: a lightweight, review-first workspace for coding agents, written in Rust. Run Claude Code,
Codex and shells side by side, each in its own git worktree. Click any agent's title to see its worktree's diff
against main, comment on lines, and send the review straight to the agent while it keeps working. Or let an
orchestrating agent plan the work, hand out tasks and review the results while you approve and steer.

An atomisk.ai project.

The command is `imacs`. `scripts/macos-app.sh` builds IMACS.app for the Dock; state lives in `~/.imacs` and the config
in `~/.config/imacs` (moved once from `~/.cox` and `~/.config/cox` by earlier versions).

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

The config is created on first start at `~/.config/imacs/config.toml`. It is commented and reloads while IMACS runs: agents
and their commands, `base_ref`, layout breakpoints, fonts, colors, terminal scrollback, shortcuts, and the texts used for
steering.

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

`imacs_plan_dispatch` (or **Dispatch ready** on the board) gives each ready task its own worktree and an agent whose
first message is the brief. Every agent IMACS starts gets a `imacs_report` tool, pre-approved, to say when it is done,
blocked or has a question. Reports land on the task card, move done tasks to review, and wake the orchestrator's
`imacs_wait`, which then reads `imacs_result` and `imacs_diff`, reviews, steers, and accepts with `imacs_plan_update`.

Plans are stored separately in `~/.imacs/plans/<id>.json`, with status `open`, `completed` or `archived`. The old
`~/.imacs/plan.json` migrates once and is kept as `plan.json.migrated`; an old plan whose tasks are all done
migrates as completed. Saves replace files atomically, and unreadable plan files are listed in the startup message.
Open and completed plans get tabs; archived plans are hidden. Select a tab to see
that plan’s sessions; `cmd+b` opens its board. Other sessions remain under All and their project tabs. Each plan has
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

## Running many agents

Every agent works in its own worktree with its own `target/`, so its builds never wait on another's. What they
share is the machine, and IMACS keeps that in check for every agent it starts (`[resources]` in the config):

New agent worktrees start with a warm build cache when the main checkout has one. Before launching the agent,
IMACS clones the folders in `[worktrees] warm` (default `["target"]`) from the repository's **main checkout**, even
when you start from another linked worktree. The clone runs on the background worktree job. Only copy-on-write
clones are used: unsupported filesystems, missing folders and clone errors are skipped without retrying as a full
copy or blocking launch on a fallback. macOS uses the native clone-only `clonefile` API
(`cp -c` can silently fall back to full copying); Linux uses `cp -R --reflink=always`.
Each worktree then owns its cache independently; builds cannot change the
main checkout's cache. A successful clone shows **warm cache from main** in a toast. Existing worktrees are left
alone when retrying a launch. `imacs doctor` probes CoW cloning between the repository and configured worktree
folders, using temporary files that it removes afterwards.

```toml
[worktrees]
warm = ["target"]          # existing folders relative to the repository root; nested paths also work
# warm = ["target", "build/cache"]
# warm = []               # turn cache warming off
```


- **One compiler job pool.** IMACS owns a GNU make jobserver (a named pipe in a private folder under `/tmp`, which
  Codex's sandbox can open) with `jobs` tokens, cores minus 2 by default, and hands it to every agent terminal as
  `CARGO_MAKEFLAGS`. cargo takes a token per job, so ten agents building at once share those jobs instead of each
  using every core. `MAKEFLAGS` is left alone, since `make` before GNU make 4.4 misreads a pipe jobserver. A job
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
`~/.imacs/crash.log` has the panic and its backtrace when there was one.

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
`imacs_result` (an agent's last prompt, final message and screen), `imacs_list`, `imacs_start` (in a folder, or a new worktree from `repo` + `branch`), `imacs_send` (a message, or raw
keys for menus), `imacs_read` (screen text), `imacs_diff`, `imacs_comment` + `imacs_send_review`, `imacs_open` (show a file to the user), `imacs_focus`, `imacs_close`.
Comments an agent leaves show inline in the diff panel before they are sent, so you can see what it asks for.

`imacs ctl '<json>'` sends the same requests from a shell, e.g. `imacs ctl '{"cmd":"list"}'`. A few extras exist for
scripting and testing: `show_diff`, `show_chief`, `resize` and `screenshot`. For example,
`imacs ctl '{"cmd":"show_chief","open":true,"dock":"docked","width":720}'` shows the dock and remembers its width.

## Shortcuts

`cmd` is Command on macOS and Ctrl+Shift elsewhere, so it never collides with the terminal.

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
| `cmd+t` | new session (existing folder or new worktree) |
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

Agents run as children of the IMACS process, so closing IMACS stops them. Claude sessions can be resumed from the sidebar,
and Codex with `codex resume --last`.
