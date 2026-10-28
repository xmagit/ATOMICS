<img src="assets/icon.png" alt="atomisk.ai icon" width="96">

# IMACS

Emacs, but for agents: a lightweight, review-first workspace for coding agents, written in Rust. Run Claude Code,
Codex and shells side by side, each in its own git worktree. Click any agent's title to see its worktree's diff
against main, comment on lines, and send the review straight to the agent while it keeps working. Or let an
orchestrating agent plan the work, hand out tasks and review the results while you approve and steer.

An atomisk.ai project.

The command is `cox`, after the coxswain: the one in the boat who doesn't row, but steers and calls the stroke.

## What it does

- **Agents in real terminals.** Every agent runs its own TUI in an embedded terminal, so nothing is lost compared to
  running it yourself.
- **The diff against main, always.** The diff panel follows the active agent. It covers committed, staged, unstaged and
  untracked changes against the merge-base with `base_ref`, so main moving on never shows up as the agent's work. It
  updates live, with syntax highlighting and word-level emphasis.
- **Projects and workspaces.** `cox <folder>` opens the repository it is in, always through its main checkout, so a
  folder inside any worktree opens the same project. The explorer shows the main checkout's tracked files, with the
  changes of the worktree under review laid over them, plus each project's worktrees. **Project › Open project…**
  adds more repositories; save the set as a workspace and reopen it with `cox --workspace <name>`.
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
- **Status without asking.** Claude sessions report through hooks. Agents without hooks, like Codex, are read from their
  screen (`busy_text`). What needs you is counted in the top bar; `cmd+j` jumps to it.
- **Responsive.** On a wide screen, every agent gets a tile beside the diff (10 agents fit on a 5120 px ultrawide). As the window narrows, tiles collapse into
  tabs. Below that the diff stacks above the active agent, and at the smallest size the diff takes the whole area.
- **Remote control and MCP.** Other agents can start, read, review and steer agents in cox (see below).

## Run

```sh
cargo install --path crates/cox     # or: cargo run --release -p cox
cox                                 # opens the repository you are in (or your last projects)
cox ~/repos/app                     # opens that project
cox --workspace daily               # opens a saved workspace
cox --open claude:~/repos/app --open codex:~/repos/app-feature
```

The config is created on first start at `~/.config/cox/config.toml`. It is commented and reloads while cox runs: agents
and their commands, `base_ref`, layout breakpoints, fonts, colors, shortcuts, and the texts used for steering.

## Claude sessions anywhere

Agents that cox starts get its hooks through `--settings`. To make Claude sessions in any other terminal show up too
(status, diff, and messages delivered at their next tool call or turn end), add the hooks to your user settings:

```sh
cox install-hooks      # merges into ~/.claude/settings.json, keeps a backup
```

## Orchestration: a plan you approve, agents that report back

An orchestrator (a Claude session with the cox MCP server) turns a goal into a plan with `cox_plan_set`: tasks with a
brief, acceptance checks, dependencies, and which agent and model should do them. The plan appears on the **plan
board** (`cmd+b`, or **Plan** in the top bar) in columns for planned, running, blocked, review and done. Edit briefs,
then **Approve plan**: nothing starts before that.

`cox_plan_dispatch` (or **Dispatch ready** on the board) gives each ready task its own worktree and an agent whose
first message is the brief. Every agent cox starts gets a `cox_report` tool, pre-approved, to say when it is done,
blocked or has a question. Reports land on the task card, move done tasks to review, and wake the orchestrator's
`cox_wait`, which then reads `cox_result` and `cox_diff`, reviews, steers, and accepts with `cox_plan_update`.

## Let an agent drive cox (MCP)

```sh
claude mcp add cox -s user -- cox mcp
codex mcp add cox -- cox mcp
```

Tools: `cox_wait` (block until an agent finishes a turn or needs input; returns a cursor for the next call),
`cox_result` (an agent's last prompt, final message and screen), `cox_list`, `cox_start` (in a folder, or a new worktree from `repo` + `branch`), `cox_send` (a message, or raw
keys for menus), `cox_read` (screen text), `cox_diff`, `cox_comment` + `cox_send_review`, `cox_open` (show a file to the user), `cox_focus`, `cox_close`.
Comments an agent leaves show inline in the diff panel before they are sent, so you can see what it asks for.

`cox ctl '<json>'` sends the same requests from a shell, e.g. `cox ctl '{"cmd":"list"}'`. A few extras exist for
scripting and testing: `show_diff`, `resize` and `screenshot`.

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
| `cmd+t` | new session (existing folder or new worktree) |
| `cmd+r` | refresh diff |
| `cmd+w` | close session |
| `cmd+1…9` | switch session |

## Design

Two crates. `cox-core` holds everything testable without a window: config, the git diff engine and parser, the
responsive layout, the hook and control protocols, and review message composition. `cox` is the app.

| Concern | Choice | Why |
|---|---|---|
| UI | egui/eframe (wgpu) | Pure Rust, immediate mode: layout is plain code each frame, so responsive tiling is simple; idle when nothing changes |
| Terminal | alacritty_terminal | Mature VT emulation and PTY event loop; Claude's and Codex's TUIs render correctly |
| Git | `git` CLI | Exact porcelain semantics, and picks up `core.fsmonitor`/`untrackedCache` on large repositories |
| Highlighting | syntect + bat's grammars (two-face) | Works line by line, which suits hunks that start mid-file |
| Word diff | similar | Code-aware tokens, so `compute(1)` → `compute(2)` marks only the number |
| Watching | notify (FSEvents/inotify) | Live diff for the worktree on screen; agents off screen refresh while they work |
| IPC | Unix socket, JSON lines | One socket for hooks, `cox ctl` and `cox mcp`; no async runtime anywhere |

Agents run as children of the cox process, so closing cox stops them. Claude sessions can be resumed from the sidebar,
and Codex with `codex resume --last`.
