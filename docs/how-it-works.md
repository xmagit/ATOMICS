# How ATOMICS works

A short map of the moving parts. Each section links to the page with the details.
[Fresh start on Linux](fresh-start-linux.md) sets up a Linux desktop from scratch, and
[the NixOS VM](nixos-vm.md) sets up a Mac.

## The window and the host

ATOMICS has two halves:

- **The host** runs the work: agent terminals, the chief, plans, approvals, worktrees, git, hooks, MCP, the
  control socket (`atomics ctl`, `atomics queue`) and the credential store.
- **The window** draws it: the native desktop app, or the browser client on a paired device.

Plain `atomics` runs both in one process on your machine. `atomics host` runs the host with no window (on a
server or in a VM), and a window connects to it with `[client] connect` or `atomics --connect`.

On a Mac, the recommended setup puts the host in the ATOMICS NixOS VM: ATOMICS.app is only the window
(`[client] connect = "vm"`), and agents, the chief, builds, Docker and every sign-in live in the VM. The VM is
the agents' boundary, so they run bare inside it. Your repositories are shared into the VM at the same paths.

In the rest of this page, **this machine** means the machine that runs the host: your desktop, or the VM.
See [The headless host](daemon.md#headless-host-atomics-host), [The desktop as a client](daemon.md#the-desktop-as-a-client)
and [How the Mac and the VM fit together](nixos-vm.md#how-the-mac-and-the-vm-fit-together).

## First run

With no config yet, ATOMICS opens the setup screen. It lists the agents it found, with **Install** where a click can
install one and the exact command where it cannot. It also asks where agents run, for a first project folder, and for
connections (GitHub, Azure DevOps, tickets, remote devices). Each step has **Connect later**, and **Skip for now** leaves
the screen; **Settings → Getting started** brings it back. An install that already has a config never sees it.

On an Apple Silicon Mac, the first run offers **Simple**, a guided setup of the hardened VM, or **Full**, the setup
above ([Optional Simple setup](nixos-vm.md#optional-simple-setup)). Simple is also a layout: the chief's conversation
first, and the plan's work as a flow ([Simple mode](configuration.md#simple-mode)). Each window picks Simple or Full
for itself.

## Projects

`atomics <folder>` opens the repository the folder is in, always through its main checkout. Given a folder that is not
a repository, or started in one from a terminal, ATOMICS treats it as a working folder: it opens every repository up to
two levels below it and starts the chief there. With none found, it reopens your last session. Starts from the Dock or
an applications menu, a bare `atomics host`, and starts in `/`, your home folder or above it always reopen your last
session or workspace. Save a set of projects as a workspace and
reopen it with `atomics --workspace <name>`. See [What it does](../README.md#what-it-does).

## Sessions

A session is one agent in a terminal tile: Claude Code, Codex, GitHub Copilot CLI, Cursor CLI (`agent`) or a shell.
It works in a git worktree of a project, in an existing folder, or in a scratch folder outside any project (the
**Ungrouped** tab). The four agents report their status through hooks. The top bar counts agents working and open,
and shows who needs you on hover.

The diff panel shows a plan task's worktree against its recorded starting commit, so stacked tasks leave out their
dependencies' changes. Other sessions compare against the merge-base with `base_ref`. Review comments on the diff
go to the agent as one message while it keeps working.

Sessions are saved in `sessions.json` in the state folder, and agents that can resume do so after a restart. The
meters at the foot of the sidebar show what is left of Claude, Codex and any other provider in use. See
[Usage](../README.md#usage).
`sessions.json` and `plans/<id>.json` are written in order on a thread of their own, never while the window waits, and
flushed before quitting, restarting or a cleanup reads them; a crash can lose the last few milliseconds of changes.

## The chief

The chief is an agent with ATOMICS's orchestration tools, started in the project's main checkout (**CHIEF** or
`cmd+o`). It plans, waits for your approval, dispatches, verifies and reviews, and never pushes without asking.
It answers other agents' permission prompts (`atomics_approve`) and has tools of its own: running checks inside a
sandbox (`atomics_exec`), reading config and proposing changes you apply, and restarting ATOMICS.

Several chief sessions can run side by side, each with its own boards: `atomics_wait` tells a chief of its own boards
only, and `atomics_plan_get` marks them `yours`. News about a board whose session does not run, and about agents that
belong to no board, goes to the workspace's own chief. See [Chief sessions](configuration.md#chief-sessions).

The chief is never sandboxed, whatever the isolation settings say. It runs on the machine that runs the host, because
it needs `atomics ctl`, git across repositories, and the builds of bare tasks. On a desktop that is your own account.
With the host in the NixOS VM, the chief runs in the VM, inside the same boundary as the agents.

Every `[chief] heartbeat_secs` the host checks the fleet (idle or missing agents, waiting approvals, held messages)
and tells the chief what it found. The idle wake (`[chief] wake_secs`) also lists up to five finished agents to close.
The chief closes each one with `atomics_close` or says why it should stay open. See
[The chief agent](../README.md#the-chief-agent).

### Choosing a chief

Claude Code is the default chief. **Settings → Chief** switches to Codex, Copilot CLI or Cursor CLI, and lists each
provider's models, sign-in status and remaining quota. Changing models within Claude or Codex resumes the same
conversation. Changing providers starts a new conversation with a bounded handoff of open plans, task states and
recent turns. The current chief stays active until the new one acknowledges it.

| Chief | Instructions and tools | Permissions |
| --- | --- | --- |
| Claude Code | The composed system prompt and `atomics mcp --chief` | The chief tool allowlist and Claude's own prompts |
| Codex | The same prompt as `developer_instructions`, the MCP server through `-c` | A read-only sandbox; shell commands outside it ask for approval |
| Copilot CLI, Cursor CLI | A session-local plugin folder with the MCP server and rules | The CLI's own approval menus, read off its screen |

opencode is recognised but cannot be the chief yet: its permission model does not map onto ATOMICS's approvals.
See [Copilot and Cursor chiefs](configuration.md#copilot-and-cursor-chiefs) and
[Chief models and intervals](configuration.md#chief-models-and-intervals).

## Plans and approval

The chief writes a plan with `atomics_plan_set`: tasks with a brief, acceptance checks, dependencies, an agent and its
model. The plan appears on its own board (`cmd+b`), where you edit briefs and press **Approve plan**. Nothing starts
before that. Dispatch gives each ready task a worktree and an agent whose first message is the brief. Agents answer with
`atomics_report` (done, blocked or a question), which moves the card and wakes the chief. The chief reads the result
and the diff, steers, and accepts. Several plans can be open at once.

Plan agents can leave addressed messages in the plan's notebook. By default each one waits for the chief or you to
approve delivery, and approval never gives the text authority: agent text is data, not instructions, like ticket text.
See [Notebook coordination](configuration.md#notebook-coordination) and
[Orchestration](../README.md#orchestration-several-plans-you-approve-agents-that-report-back).

## Integration branches and the final review

Accepted tasks come together on the plan's integration branch, `atomics/plan-<plan id>` unless the plan names another.
Integrating a task commits what it left uncommitted and merges its branch in a worktree of its own, so your main
checkout is never touched. A conflict aborts the merge and keeps the task in review. Tasks with dependencies branch
from the integration branch; the others branch from `base_ref`, fetched just before.

Every plan ends with a final review task that starts from the integration branch in a fresh worktree and runs the
plan's builds and tests. The plan completes when every task is accepted and the `[workflow] done` milestone is
reached. See [Lifecycle and completion](tickets.md#lifecycle-and-completion).

## Isolation

`[isolation] mode` decides where agents run:

- **`sandbox`** (the default on a desktop): each agent gets its own Docker Sandboxes microVM. It sees its worktree, git's
  write folders and the folders you mount read-only, and nothing else of your machine.
- **`bare`**: the agent runs directly on this machine, as you. The NixOS VM seeds this, because the VM is the boundary.

A repository can require sandboxes but never turn them off. Otherwise a task's or session's `isolation` wins, then the
agent's, then `[isolation] mode`. Because a sandbox can write files that tell git where a repository is, ATOMICS pins
the git locations it trusts before a sandbox runs, and puts back refs a sandboxed agent moved without accounting for
them (kept under `refs/atomics/quarantine/`). See [Agent isolation](isolation.md) and
[Trust model](isolation.md#trust-model).

## Credentials and the relay

ATOMICS keeps its own credentials in the OS keychain (`atomics-credentials-v1`: macOS's keychain, or the Secret
Service on Linux). On a Linux machine without a keyring, such as the NixOS VM, `ATOMICS_CREDENTIALS_DIR` names a
private folder that holds them instead, one file per item, readable by its user only. `atomics setup` and **Settings → Credentials** write them; values
travel over stdin. Agents' own sign-ins (Claude, Codex, gh, az) stay with their CLIs on this machine.

The relay is an HTTP proxy on this machine's loopback. A sandboxed session that may use a credential gets a random
placeholder, which the relay swaps for the real value on the requests its rules allow. Today it serves the Claude
subscription to sandboxed Claude agents (and to bare ones with `[isolation] bare_relay = true`), and, when enabled,
Azure tokens from your `az login`, with writes waiting for your approval. `relay.log` in the state folder has one
line per request, without values. See [Sandboxed Claude on your
subscription](isolation.md#sandboxed-claude-on-your-subscription).

## Approvals

When a bare agent asks permission for a command, its hook brings the request to ATOMICS. ATOMICS shows it on the
tile, allows the commands in `[approvals] allow` by itself, and leaves the rest to the chief (`atomics_approve`) or to
you. Copilot and Cursor ask in menus of their own instead: ATOMICS reads a command or MCP tool menu off the screen into
the same pending approval and answers it with the CLI's own keys, bound to that very menu, and checks that the menu
closed. In Simple mode, **Needs you** offers folder-access decisions for Claude Code and Codex. Copilot and Cursor
folder trust and other setup menus take their answer in the host terminal. Typed messages wait while a menu is open.
Sandboxed agents run unattended under the sandbox and its network policy. Config changes the chief proposes wait for
your **Apply**. See [Folder access](configuration.md#folder-access) and [Permission approvals](../README.md#permission-approvals).

## The shared queue

Agents share this machine. ATOMICS gives every agent terminal one compiler job pool (a GNU make jobserver through
`CARGO_MAKEFLAGS`), starts agents at a lower priority, and keeps heavy work in slots: `atomics queue --class test --
<command>` waits for a test slot, and `--class qa` for an app instance. The chief's `atomics_exec` waits for the same
slots. See [Running many agents](../README.md#running-many-agents).

## Cleanup

Accepting a task closes its idle agent once its work is integrated or merged, or it made no changes. Uncommitted work,
a running turn or a pending message keeps the agent open, with the reason on its card. Completing a plan removes its
clean task worktrees and the task branches merged into the integration branch. Dirty worktrees and the main checkout
are always kept. ATOMICS records each sandbox before making it, so a crash leaves nothing the next start's sweep
cannot find. See [Lifecycle](isolation.md#lifecycle) and `[cleanup]` in [Configuration](configuration.md).

## Updates

ATOMICS checks GitHub for a new release on start and every 6 hours, and shows **Update** in the top bar when there is
one. An update checks the download's SHA-256 and Ed25519 signature before unpacking, replaces only the program, and
restarts once no agent, turn or editor would be interrupted. `atomics rollback` goes back to the version it replaced.
With [`[update] source`](configuration.md#updates), a checkout of ATOMICS is the source instead of releases.

The VM is updated separately: `atomics vm update` rebuilds its system with your `[vm] modules` and keeps its disk and
sign-ins ([Updating the VM in place](nixos-vm.md#updating-the-vm-in-place)). See also
[Desktop releases](releasing.md).

## Tickets

Plans can follow Linear, Relay or any ticket system with an MCP server. ATOMICS talks to that server itself; agents
read and comment through ATOMICS's own tools and never get the server or its token. See [Tickets](tickets.md).

## MCP connections

**Settings → Connections** lists sbx's registered servers and can import server definitions from Claude Code, Codex,
Copilot CLI and Cursor CLI. Importing connects nothing; clicking **Connect** registers a server and opens its sign-in in
your browser. Every connected server loads into every sandboxed session. Bare agents use their own CLI's servers and
sign-ins. Every agent also gets ATOMICS's own MCP server for reports and ticket tools, and the chief gets its
orchestration tools on top. See [MCP connections](isolation.md#mcp-connections).

## Where things live

| What | Where | Moves between machines? |
| --- | --- | --- |
| Config | `~/.config/atomics/`: `config.toml`, `config.d/<host>.toml`, `workspaces/`, `themes/` | Yes; see [Moving to a new machine](moving-machines.md) |
| Repository workflow | `ATOMICS.md` and `.atomics/` in the repository | With the repository; see [Repository workflows](repo-workflow.md) |
| State | `~/.atomics` on macOS and older installs, `~/.local/state/atomics` on new Linux installs: sessions, plans, briefs, scratch folders, git pins, the sandbox ledger, the relay log | No |
| Socket and job pool | Beside the state, or in `$XDG_RUNTIME_DIR` on Linux | No |
| Credentials | The OS keychain, or `ATOMICS_CREDENTIALS_DIR` on Linux without a keyring; sbx keeps its own | No: set them up again |

With the host in the NixOS VM, everything above except the window's own `[client]` setting lives in the VM, under
the VM user's home. The Mac keeps the window's config and the pairing token in its keychain.
