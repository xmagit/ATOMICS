# How IMACS works

A short map of the moving parts. Each section links to the page with the details;
[Fresh start on Linux](fresh-start-linux.md) sets a machine up from scratch.

## Sessions

A session is one agent in a terminal tile: Claude, Codex or a shell. It works in a git worktree of a
project, an existing folder, or a scratch folder of its own outside any project (the **Ungrouped** tab).
The diff panel shows the session's worktree against the merge-base with `base_ref`, and review comments
on that diff go to the agent as one message while it keeps working. Claude and Codex report their status
through hooks, so the top bar counts what needs you. Sessions are saved in `sessions.json` in the state
folder, and Claude sessions resume after a restart. See [What it does](../README.md#what-it-does).

## The chief

The chief is a Claude session with every IMACS tool, started in the project's main checkout (**CHIEF**
or `cmd+o`). It plans, waits for your approval, dispatches, verifies and reviews, and never pushes
without asking. It always runs bare, on this machine, because it needs `imacs ctl`, git across
repositories and the builds of bare tasks. It answers other agents' permission prompts
(`imacs_approve`), and some tools are its alone: running checks inside a sandbox (`imacs_exec`), reading
config and proposing changes you apply, and restarting IMACS. Every `[chief] heartbeat_secs` IMACS
checks the fleet (idle or missing agents, waiting approvals, held messages) and tells the chief what it
found. See [The chief agent](../README.md#the-chief-agent) and [Configuration](configuration.md).

## Plans and approval

The chief writes a plan with `imacs_plan_set`: tasks with a brief, acceptance checks, dependencies, an
agent and its model. The plan appears on its own board (`cmd+b`), where you edit briefs and press
**Approve plan**; nothing starts before that. Dispatch gives each ready task a worktree and an agent whose
first message is the brief. Agents answer with `imacs_report` (done, blocked or a question), which moves
the card and wakes the chief's `imacs_wait`. The chief reads the result and the diff, steers, and accepts.
Several plans can be open at once, each with its own tab. See
[Orchestration](../README.md#orchestration-several-plans-you-approve-agents-that-report-back).

## Integration branches and the final review

Accepted tasks come together on the plan's integration branch, `imacs/plan-<plan id>` unless the plan names
another. Integrating a task commits what it left uncommitted and merges its branch, in a worktree of its
own, so your main checkout is never touched. A conflict aborts the merge and keeps the task in review.
Tasks with dependencies branch from the integration branch, the others from `base_ref`. Every plan ends
with a final review task that starts from the integration branch in a fresh worktree (in its own sandbox
in sandbox mode) and runs the plan's builds and tests. The plan completes when every task is accepted and
the `[workflow] done` milestone is reached. See [Orchestration](../README.md#orchestration-several-plans-you-approve-agents-that-report-back)
and [Lifecycle and completion](tickets.md#lifecycle-and-completion).

## Isolation

By default each agent runs in its own Docker Sandboxes microVM, which sees its worktree, git's write
folders and the folders you mount read-only, and nothing else of your machine. `bare` runs an agent
directly on this machine, as you. A repository can require sandboxes but never turn them off; otherwise
a task's or session's `isolation` wins, then the agent's, then `[isolation] mode`.

A sandbox can write files that tell git where a repository is, so IMACS never lets its own git follow them:

- **Git pinning.** Before a sandbox runs, IMACS records where git finds its folder's repository, in
  `git-pins/` in the state folder, and runs git there only with those recorded locations, with hooks and
  fsmonitor off. Layouts git did not make are refused.
- **Ref guard.** Refs are shared with the sandbox. IMACS snapshots them before a sandboxed agent starts,
  checks them at each turn's end, and puts back refs nothing on this machine accounts for. The value it
  replaces is kept under `refs/imacs/quarantine/`, and the board shows an alert.

See [Agent isolation](isolation.md), [Git on your machine in a sandboxed
folder](isolation.md#git-on-your-machine-in-a-sandboxed-folder) and [Trust model](isolation.md#trust-model).

## The credential relay and store

IMACS keeps its credentials in one OS keychain item (`imacs-credentials-v1`: macOS's keychain, or the
Secret Service through `secret-tool` on Linux), never in a file, the config or a VM. `imacs setup` and
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

When a bare Codex agent asks permission for a command, its hook brings the request to IMACS. IMACS shows
it on the tile, allows the commands in `[approvals] allow` by itself, and leaves the rest to the chief
(`imacs_approve`) or to you. Sandboxed agents run unattended: the VM, its mounts and sbx's network policy are the boundary. An
Azure write through the relay waits for **Allow** or **Deny** in the agent's banner. Config changes the
chief proposes wait for your **Apply**. See [Permission approvals](../README.md#permission-approvals).

## The shared queue

Agents share the machine. IMACS hands every agent terminal one compiler job pool (a GNU make jobserver
through `CARGO_MAKEFLAGS`), starts agents at a lower priority, and keeps heavy work in slots:
`imacs queue --class test -- <command>` waits for a test slot, `--class qa` for an app instance. The
chief's checks inside sandboxes (`imacs_exec`) wait for the same slots. Sandboxed agents cannot reach the
queue; their limits are the sandbox's CPUs and memory. See [Running many
agents](../README.md#running-many-agents).

## Automatic cleanup

Accepting an integrated task closes its agent. Completing a plan removes its clean task worktrees and
deletes task branches merged into the integration branch; archiving removes clean worktrees and keeps
branches (`[cleanup]`). Removing a worktree also removes its sandboxes, their MCP registrations and relay
rules. IMACS records each sandbox in `sandbox-ownership.json` before making it, so a crash leaves nothing
the next start's sweep cannot find; that sweep also removes stale briefs, unused warm worktrees and
scratch folders. Dirty worktrees and the main checkout are always kept, and resources of another IMACS
home are never touched. See [Lifecycle](isolation.md#lifecycle).

## Tickets

Plans can follow Relay, Linear or any ticket system with an MCP server. IMACS talks to that server on
this machine; agents read and comment through IMACS's own tools and never get the server or its token.
See [Tickets](tickets.md), with step-by-step [Relay](tickets.md#step-by-step-relay) and
[Linear](tickets.md#step-by-step-linear) profiles.

## MCP connections

Settings → Connections lists sbx's registered servers and can import server definitions from Claude
Code and Codex. Import connects nothing; environment values stay hidden and go to the OS keychain only
with your consent. Clicking **Connect** registers a server and opens any sign-in page in your browser.
Remote servers use sbx's gateway; command servers run on this machine through `imacs mcp-host`.

Every connected server loads into every sandboxed session, including Ungrouped sessions, at creation,
reattachment or when you connect it. Missing servers show a **Connect <name>** banner. An agent's
`imacs_mcp_connect` call only raises that banner, showing who asked and the full URL for a new server;
only your click registers, authorizes or loads it. Disconnect unloads and removes the server.

Every agent also gets IMACS's own MCP server for reports and scoped ticket tools (and the chief's
orchestration tools). Bare agents use the servers and sign-in in their own Claude Code or Codex
configuration. claude.ai connectors do not reach sandboxed Claude: connect the service's own MCP URL
instead. See [MCP connections](isolation.md#mcp-connections) for import paths, status and authentication.

## Where things live

| What | Where | Moves between machines? |
| --- | --- | --- |
| Config | `~/.config/imacs/`: `config.toml`, `config.d/<host>.toml`, `workspaces/`, `themes/` | Yes; see [Moving to a new machine](moving-machines.md) |
| Repository workflow | `IMACS.md` and `.imacs/` in the repository | With the repository; see [Repository workflows](repo-workflow.md) |
| State | `~/.imacs` on macOS and existing installs, `~/.local/state/imacs` on new Linux installs: sessions, plans, briefs, scratch folders, git pins, ownership ledger, relay log | No |
| Socket and job pool | Beside the state, or in `$XDG_RUNTIME_DIR` on Linux | No |
| Credentials | The OS keychain, and sbx's own secrets and sign-in | No: set them up again |
