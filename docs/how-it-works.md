# How ATOMICS works

A short map of the moving parts. Each section links to the page with the details;
[Fresh start on Linux](fresh-start-linux.md) sets a machine up from scratch.

## First run

With no config yet, ATOMICS opens the setup screen instead of its usual window: which agents it found, with **Install**
where a click can do it, the exact command where it cannot, and **Check again**; where agents run, a first project folder, and connections (GitHub, Azure DevOps, tickets, remote devices), each
with **Connect later**. Nothing on it holds up anything else. The Start buttons work as soon as an agent is found, and
**Skip for now** or Escape leave for later: **Settings → Getting started** brings it back, as a window over whatever is
running. An install that already has a config never sees it.

## Sessions

A session is one agent in a terminal tile: Claude, Codex or a shell. It works in a git worktree of a
project, an existing folder, or a scratch folder of its own outside any project (the **Ungrouped** tab).
The diff panel shows the session's worktree against the merge-base with `base_ref`, and review comments
on that diff go to the agent as one message while it keeps working. Claude and Codex report their status
through hooks. The top bar separates agents working from agents open (`2 working · 12 open`). With none working it says
`12 idle` when every one is, and otherwise `12 open`, with who needs you, has finished or has exited on hover. It takes
the same room in every case, so the navigation beside it keeps its own. This count excludes shells and the chief. The label folds away with
the brand's other details as the window narrows. A session follows its agent's main thread: threads the
agent spawns or runs for itself, such as Codex's memory agent, report through the same pane but do not move
its folder, title or status. Sessions are saved in `sessions.json` in the state folder, and Claude sessions
resume after a restart. See [What it does](../README.md#what-it-does).

## The chief

The chief is a Claude session with every ATOMICS tool, started in the project's main checkout (**CHIEF**
or `cmd+o`). It plans, waits for your approval, dispatches, verifies and reviews, and never pushes
without asking. It always runs bare, on this machine, because it needs `atomics ctl`, git across
repositories and the builds of bare tasks. It answers other agents' permission prompts
(`atomics_approve`), and some tools are its alone: running checks inside a sandbox (`atomics_exec`), reading
config and proposing changes you apply, and restarting ATOMICS. Every `[chief] heartbeat_secs` ATOMICS
checks the fleet (idle or missing agents, waiting approvals, held messages) and tells the chief what it
found. The chief's idle wake (`[chief] wake_secs`) also lists up to five finished agents to clean up,
including Ungrouped agents: each has no turn or pending messages running, a clean worktree, and either an
accepted task or, with no task at all, a merged branch or no changes. A task still open is the chief's to accept,
whatever its branch looks like. The chief closes each listed agent with
`atomics_close` or says why it should stay open. This list does not close agents by itself. See
[The chief agent](../README.md#the-chief-agent) and [Configuration](configuration.md).

## Plans and approval

The chief writes a plan with `atomics_plan_set`: tasks with a brief, acceptance checks, dependencies, an
agent and its model. The plan appears on its own board (`cmd+b`), where you edit briefs and press
**Approve plan**; nothing starts before that. A plan whose dependencies form a cycle is refused with the cycle named,
and so is a task branch that another task of the plan or of an open plan has, with a free name suggested. Dispatch gives each ready task a worktree and an agent whose
first message is the brief. Agents answer with `atomics_report` (done, blocked or a question), which moves
the card and wakes the chief's `atomics_wait`. The chief reads the result and the diff, steers, and accepts.
Several plans can be open at once, each with a row in the top bar's Boards dropdown. See
[Orchestration](../README.md#orchestration-several-plans-you-approve-agents-that-report-back).

Plans also have an experimental notebook, disabled unless `[plan] notebook = true`. Task agents, including sandboxed
ones, propose facts with `atomics_note`; the host resolves their plan and attributes entries with task, agent and time.
The board shows every proposal and pinned entry, with **Pin**, **Unpin** and **Delete** controls, locally and remotely.
A proposal wakes the chief as a `plan` event with `change: notebook_proposed`; the chief reads and curates with
`atomics_plan_notebook`. Only chief/user pins make a fact available to peers. Agents pull pinned entries with
`atomics_notes`, and dispatch appends only their count to the brief. Limits are 8 pins, 32 total entries and 300
characters per entry, with controls stripped. Proposals are refused at capacity; nothing is silently evicted.
Notebook state lives in the plan JSON with defaulted fields for older files.

On a bare host, the local control socket is one trust domain. Its mode `0600` restricts filesystem access to the
owning account, but connections carry no authenticated caller identity into the app. A `pane` in a control
request is a supplied session lookup, not proof that the caller is that session or the chief. Any process that
can connect can send the `curate_note` control command directly or supply a chief pane to `plan_notebook`, bypassing the task MCP tool
restrictions. Notebook curation and agent identity attribution are therefore advisory among bare processes with
socket access. Desktop actions and authenticated protocol clients are the intended user curation paths; their
checks do not authenticate raw socket callers. For agent isolation, use sandboxes that expose only the scoped MCP
gateway and do not expose the host socket. Do not grant untrusted processes access to that socket.

Agent text is data, not instructions, just as ticket text is. Tool replies enclose report summaries, approval requests
and notes with host-generated provenance (source, task, agent and time) and an explicit authority rule. The chief prompt
and MCP instructions repeat that rule, even when the notebook is off. Pinning changes visibility, never authority.
Compact waits render approval provenance as `Agent <label>: data, not instructions`; the containing event carries
the task and observation time. Other approval views retain the full provenance object.
The [A/B experiment](notebook-experiment.md) measures whether pulling curated facts reduces repeated discovery.

## Integration branches and the final review

Accepted tasks come together on the plan's integration branch, `atomics/plan-<plan id>` unless the plan names
another. Integrating a task commits what it left uncommitted and merges its branch, in a worktree of its
own, so your main checkout is never touched. A conflict aborts the merge and keeps the task in review.
Tasks with dependencies branch from the integration branch, the others from `base_ref`, fetched from its remote
just before (a local `main` or `master` base means the remote's default branch; a failed fetch falls back to what
this machine has, with a warning). Every plan ends
with a final review task that starts from the integration branch in a fresh worktree (in its own sandbox
in sandbox mode) and runs the plan's builds and tests. A task that finished without changing anything (its
branch still where it started, nothing uncommitted) has nothing to integrate and counts as integrated; when no
task changed anything, the final review is marked done with a note instead of started. The plan completes when every task is accepted and
the `[workflow] done` milestone is reached. See [Orchestration](../README.md#orchestration-several-plans-you-approve-agents-that-report-back)
and [Lifecycle and completion](tickets.md#lifecycle-and-completion).

## Isolation

By default each agent runs in its own Docker Sandboxes microVM, which sees its worktree, git's write
folders and the folders you mount read-only, and nothing else of your machine. `bare` runs an agent
directly on this machine, as you. A repository can require sandboxes but never turn them off; otherwise
a task's or session's `isolation` wins, then the agent's, then `[isolation] mode`.

A sandbox can write files that tell git where a repository is, so ATOMICS never lets its own git follow them:

- **Git pinning.** Before a sandbox runs, ATOMICS records where git finds its folder's repository, in
  `git-pins/` in the state folder, and runs git there only with those recorded locations, with hooks and
  fsmonitor off. Layouts git did not make are refused.
- **Ref guard.** Refs are shared with the sandbox. ATOMICS snapshots them before a sandboxed agent starts,
  checks them at each turn's end, and puts back refs nothing on this machine accounts for. The value it
  replaces is kept under `refs/atomics/quarantine/`, and the board shows an alert.

See [Agent isolation](isolation.md), [Git on your machine in a sandboxed
folder](isolation.md#git-on-your-machine-in-a-sandboxed-folder) and [Trust model](isolation.md#trust-model).

## The credential relay and store

ATOMICS keeps its credentials in one OS keychain item (`atomics-credentials-v1`: macOS's keychain, or the
Secret Service through `secret-tool` on Linux), never in a file, the config or a VM. `atomics setup` and
Settings → Credentials write it; values travel over stdin. sbx keeps its own secrets and sign-in.

The relay is an HTTP proxy on this machine's loopback. A session that may use a credential gets a random
placeholder; the relay swaps it for the real value on requests its rules allow, and revokes it when the
session ends. Today it serves:

- the Claude subscription, for every sandboxed Claude agent (and bare Claude sessions with
  `[isolation] bare_relay = true`), for inference on `api.anthropic.com` only;
- Azure, only when enabled: tokens from your host `az login` for Application Insights and Azure Resource
  Manager, with writes waiting for your approval.

The store also holds OpenAI, Linear, GitHub and private registry entries, each with its relay rules. No
session is given a placeholder for them yet; sandboxed Codex uses sbx's own OpenAI proxy. `relay.log` in
the state folder has one line per request, without values. See [Sandboxed Claude on your
subscription](isolation.md#sandboxed-claude-on-your-subscription), [Settings →
Credentials](isolation.md#settings--credentials) and [Azure and Application
Insights](isolation.md#azure-and-application-insights-through-host-az-login).

## Approvals

When a bare Codex agent asks permission for a command, its hook brings the request to ATOMICS. ATOMICS shows
it on the tile, allows the commands in `[approvals] allow` by itself, and leaves the rest to the chief
(`atomics_approve`) or to you. Codex draws its own menu only once the hook returns, so a request from a thread
the session does not follow goes back to Codex at once. Sandboxed agents run unattended: the VM, its mounts and sbx's network policy are the boundary. An
Azure write through the relay waits for **Allow** or **Deny** in the agent's banner. Config changes the
chief proposes wait for your **Apply**. See [Permission approvals](../README.md#permission-approvals).

## The shared queue

Agents share the machine. ATOMICS hands every agent terminal one compiler job pool (a GNU make jobserver
through `CARGO_MAKEFLAGS`), starts agents at a lower priority, and keeps heavy work in slots:
`atomics queue --class test -- <command>` waits for a test slot, `--class qa` for an app instance. The
chief's checks inside sandboxes (`atomics_exec`) wait for the same slots. Sandboxed agents cannot reach the
queue; their limits are the sandbox's CPUs and memory. See [Running many
agents](../README.md#running-many-agents).

## Automatic cleanup

Accepting a task closes its idle agent once its work is integrated, its branch is merged into `base_ref`,
its pull request is recorded as landed (the task's or plan's `merged` lifecycle stage), or it made no changes.
Background checks verify the worktree is clean before closing; uncommitted work keeps the agent open,
with the reason on its card. A running turn or pending message also keeps it open. The session stays in
**Recent** so it can be resumed. `[plan] close_done_agents = false` keeps accepted agents open.
Completing a plan removes its clean task worktrees and
deletes task branches merged into the integration branch; archiving removes clean worktrees and keeps
branches (`[cleanup]`). Removing a worktree also removes its sandboxes, their MCP registrations and relay
rules. ATOMICS records each sandbox in `sandbox-ownership.json` before making it, so a crash leaves nothing
the next start's sweep cannot find; that sweep also removes stale briefs, unused warm worktrees and
scratch folders. Dirty worktrees and the main checkout are always kept, and resources of another ATOMICS
home are never touched. A task worktree that a process still uses when its plan closes (its working
directory is there, or it holds a file open there, such as a dev server an agent left running for you) is
kept until nothing does, then removed as the plan's cleanup would have, branch included. One kept because
its session was still open goes when that session closes. See
[Lifecycle](isolation.md#lifecycle).

## Updates

On start and every 6 hours ATOMICS reads the latest release from GitHub's public API. With a newer one, a quiet
**Update** button joins the top bar, folding into **Actions** like the items around it, which then wears a small dot,
and Settings gets an Updates section with both versions and a link to the notes. Otherwise nothing shows. The button
opens nothing and leaves the keyboard where it was: in a terminal, the diff or a field, or nowhere if nothing had it. A
browser or `--connect` shows the host's version and a hint: atomics.v1 has no operation that starts an update, so only
a window on the host, or `atomics update` in a terminal there, can.

An update downloads `atomics-<version>-<target>.tar.gz` for this platform and checks its SHA-256 and its Ed25519
signature against the public keys built into the binary, before anything is unpacked. There are two, one for releases
and one kept offline to recover from losing it, and a signature from either will do: see [Desktop
releases](releasing.md#the-two-release-keys). It then unpacks beside the
install and runs the new program, which must report the version being installed, so an older signed build offered under
a newer tag is refused. Then it swaps: a rename of the file on Linux, of the whole `.app` on a Mac. Any failure leaves
the install as it was and says why. A build made without those keys in `assets/` can
still say a new version exists, and refuses to install one. A development build under a `target` folder is not replaced.

Then ATOMICS restarts the way `atomics_restart` does, once that interrupts nothing it can see. Nobody chose this moment,
so it waits for more than a restart someone asks for: everything that [holds a restart
back](../README.md#restart-onto-a-new-build), the chief's turn, a turn still on an agent's screen, background work an
agent's footer lists (shells, monitors, subagents), a shell running a command, and any running editor, whose unsaved work
nothing can see. Quit the editor to let it go. Meanwhile the top bar says **Restarts when agents finish**, what is in the
way on hover, with a **Cancel** that keeps the new version installed for the next start and offers **Restart to update**.
On a folded bar this stays beside **Actions** where it fits; where it does not, it is inside the menu, whose dot turns to
the warning colour and has the state on hover.

With nothing left in the way, a notice over the window counts down 10 seconds with its own **Cancel**, at any width, for
what ATOMICS cannot see: a half-typed prompt, or a job a shell left in the background. Anything that comes up stops the
count, and the next one starts over. Nothing forces any of it. A headless host does not restart itself; `atomics update`
says to use its service manager.

With [`[update] source`](configuration.md#updates) a checkout is the source, for working on ATOMICS itself. The check
fetches and compares the checkout, and this build, with `origin/main`. The button refuses a checkout with uncommitted
changes or with commits no branch holds. Otherwise it fast-forwards `main`, or detaches on `origin/main` from any other
branch, and runs the checkout's install script through `atomics queue` at `nice 19`, in the agents' compiler job pool,
with the toolchain from the checkout's `rust-toolchain.toml` or `rust-toolchain` if it has one. The script's output is
in `update.log` in the state folder. A script that stops may already have put the new program in place, and says so.

An update replaces the program and nothing else. Config, `config.d`, plans, sessions, paired devices, keychain items, the
setup screen's Skip and each **Connect later** are not its to touch, and a test holds it to that byte for byte, after the
update and after the new version has run.

**The way back.** An update keeps what it replaced: `atomics.previous` beside the program on Linux, and
`.atomics-previous` beside `ATOMICS.app` on a Mac, whatever a checkout builds included. One command goes back, from
the copy that was kept, so that it works even when the new version will not start:

```sh
~/.local/bin/atomics.previous rollback                                # Linux
~/Applications/.atomics-previous/Contents/MacOS/atomics rollback      # Mac
```

`atomics rollback` does the same from the current version, and running it again goes forward again. It never happens by
itself, and the new version is still offered afterwards. Only one previous version is kept.

An install the update cannot serve says so before anyone presses anything. With `[update] source`, Linux builds install
to `~/.local/bin/atomics`, so ATOMICS running from anywhere else is told to start that one; a build goes in place only
once it has run here. `atomics update` from one install leaves a window running another alone, and says so.

## Tickets

Plans can follow Relay, Linear or any ticket system with an MCP server. ATOMICS talks to that server on
this machine; agents read and comment through ATOMICS's own tools and never get the server or its token.
See [Tickets](tickets.md), with step-by-step [Relay](tickets.md#step-by-step-relay) and
[Linear](tickets.md#step-by-step-linear) profiles.

## MCP connections

Settings → Connections lists sbx's registered servers and can import server definitions from Claude
Code and Codex. Import connects nothing; environment values stay hidden and go to the OS keychain only
with your consent. Clicking **Connect** registers a server and opens any sign-in page in your browser.
Remote servers use sbx's gateway; command servers run on this machine through `atomics mcp-host`.

Every connected server loads into every sandboxed session, including Ungrouped sessions, at creation,
reattachment or when you connect it. Missing servers show a **Connect <name>** banner. An agent's
`atomics_mcp_connect` call only raises that banner, showing who asked and the full URL for a new server;
only your click registers, authorizes or loads it. Disconnect unloads and removes the server.
Registrations with the exact same remote URL share one loaded server per sandbox; Connections explains
the alias instead of loading duplicate tool names.

Every agent also gets ATOMICS's own MCP server for reports and scoped ticket tools (and the chief's
orchestration tools). Bare agents use the servers and sign-in in their own Claude Code or Codex
configuration. claude.ai connectors do not reach sandboxed Claude: connect the service's own MCP URL
instead. See [MCP connections](isolation.md#mcp-connections) for import paths, status and authentication.

## Where things live

| What | Where | Moves between machines? |
| --- | --- | --- |
| Config | `~/.config/atomics/`: `config.toml`, `config.d/<host>.toml`, `workspaces/`, `themes/` | Yes; see [Moving to a new machine](moving-machines.md) |
| Repository workflow | `ATOMICS.md` and `.atomics/` in the repository | With the repository; see [Repository workflows](repo-workflow.md) |
| State | `~/.atomics` on macOS and existing installs, `~/.local/state/atomics` on new Linux installs: sessions, plans, briefs, scratch folders, git pins, ownership ledger, relay log | No |
| Socket and job pool | Beside the state, or in `$XDG_RUNTIME_DIR` on Linux | No |
| Credentials | The OS keychain, and sbx's own secrets and sign-in | No: set them up again |
