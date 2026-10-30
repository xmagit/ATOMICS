# Agent isolation

IMACS runs each agent in one of two modes:

| Mode | Where the agent runs | What it can reach |
| --- | --- | --- |
| `sandbox` (default) | Its own [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) microVM, made with `sbx` | Its worktree, git's write folders, the folders you mount read-only, and the network sbx's policy allows |
| `bare` | Directly on this machine, in the tile's terminal | Everything you can reach: your files, credentials, SSH agent and network |

Sandboxes are the default because an agent works unattended for long stretches. In a sandbox, a
mistake or a prompt injection reaches only what the sandbox mounts: the agent's worktree and its
repository's git objects and refs. Bare mode is how IMACS ran agents before sandboxes, unchanged.

## Requirements

Sandboxed agents need `sbx` v0.50.1 or newer, signed in to Docker:

```sh
brew install --cask docker/tap/sbx    # other platforms: https://github.com/docker/sbx-releases
sbx login
```

Before it makes or reattaches to a sandbox, IMACS checks that `sbx` is installed, recent enough and
signed in. **IMACS never falls back to bare.** If a check fails, the agent does not start. Its tile
and a toast say what to do, and a plan task dispatched to it is blocked with the reason. To run agents
on this machine anyway, set `mode = "bare"`. `imacs doctor` shows whether sandboxes are ready.

## Settings

```toml
[isolation]
mode = "sandbox"             # or "bare"

[isolation.sandbox]
memory = "8g"                # per sandbox; unset = 8g
cpus = 4                     # unset = [resources] jobs ÷ test_slots, at least 2
template = "registry.example.com/agent-sandbox:1"   # container image; unset = the agent's own
mounts = ["~/docs"]          # extra host folders, always read-only
network = "balanced"         # "balanced", "allow-all" or "deny-all"
ssh_agent = false            # let sandboxes sign with this machine's SSH agent
mcp = ["notion"]             # MCP servers registered with `sbx mcp add`, loaded into every sandbox
allow_hosts = { "~/repos/app" = ["nuget.example.com"] }   # by repository main checkout

[[agents]]
name = "aider"
command = ["aider"]
isolation = "bare"           # this agent runs on this machine, whatever mode says
```

| Key | Meaning |
| --- | --- |
| `mode` | Where agents run, unless their own `isolation` says otherwise. |
| `memory` | Memory for each sandbox, in `sbx create`'s units. Unset means `8g`. `""` leaves it to sbx, which gives each sandbox half of this machine's memory. |
| `cpus` | CPUs for each sandbox. Unset means one test run's share of the compiler job pool: `[resources] jobs ÷ test_slots`, at least 2 (7 on a 16-core machine with the resource defaults). `0` leaves it to sbx, which gives each sandbox every core. |
| `template` | Container image for every sandbox instead of the agent's own. |
| `mounts` | Host folders each sandbox sees read-only, at the same path. |
| `network` | The preset IMACS uses when sbx has no global network policy yet (`sbx policy init`). A policy that is already set up is left alone, since it covers every sandbox on the machine. Change it with `sbx policy`. |
| `allow_hosts` | Hosts a repository's sandboxes may reach beyond the policy, keyed by the repository's main checkout. They are added to each new sandbox (`sbx policy allow network --sandbox`). |
| `ssh_agent` | Off by default: IMACS runs `sbx` without `SSH_AUTH_SOCK`, so there is no agent to forward. Turn it on to let sandboxed agents sign, and so push, as you. A fixed `ssh.agentSocketPath` in sbx's own settings bypasses this. |
| `mcp` | MCP servers registered with `sbx mcp add`. IMACS loads them into each new sandbox (`sbx mcp load`). |
| `isolation` in `[[agents]]` | `"sandbox"` or `"bare"` for one agent. |
| `isolation` in a plan task | `"sandbox"` or `"bare"` for one task, set by the chief in `imacs_plan_set` and shown on its board card. Use `"bare"` for work that needs this machine, such as macOS-native builds or screenshot QA. |

A repository that requires sandboxes wins over everything. Otherwise a plan task's `isolation` wins,
then the agent's, then `mode`. A task keeps its isolation when it is resumed, from the board or after a
restart.

`[isolation]` can be set in `config.toml`, in `config.d/<host>.toml`, or in a workspace file. A workspace's
`[isolation]` merges key by key, the same way a host file does. Settings apply to agents started
afterwards. A sandbox keeps its mounts, limits and allowed hosts for its whole life.

A repository's `.imacs/config.toml` can only tighten isolation:

```toml
[isolation]
mode = "sandbox"
```

With this setting, every agent working in that repository runs sandboxed, even under `mode = "bare"`, an agent's own
`isolation = "bare"` or a plan task's. A repository cannot set `mode = "bare"` or any `[isolation.sandbox]` key. Those
settings are ignored with a doctor warning, because a cloned repository must not widen what an agent can reach.

## Agents

| Agent | Runs in its sandbox as |
| --- | --- |
| `kind = "claude"` | `claude` |
| `kind = "shell"` | `shell` |
| any other agent | the sbx agent with its program's name: `codex`, `gemini`, `copilot`, `cursor`, `opencode`, … |

The rest of the agent's `command` and IMACS's own arguments go after `sbx run … --`: the session id,
`--resume`, extra arguments and the first prompt. An agent sbx has no agent for is refused in sandbox
mode. Set `isolation = "bare"` on it to run it on this machine.

In a sandbox the VM is the boundary: sbx runs Codex with `sandbox_mode = "danger-full-access"` and
`approval_policy = "never"`, and Claude without permission prompts. The host's own sandbox and
permission flags would conflict with that, so IMACS drops them from the agent's command and a task's
arguments, and keeps the rest, such as the model and reasoning effort:

| Agent | Dropped in a sandbox |
| --- | --- |
| Codex | `-s`/`--sandbox`, `-a`/`--ask-for-approval`, `--full-auto`, `--dangerously-bypass-approvals-and-sandbox`, `--add-dir`, and `-c`/`--config` overrides of `sandbox_mode`, `approval_policy` and `sandbox_workspace_write.*` |
| Claude | `--permission-mode`, `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions`, `--add-dir` and its folders |

A task planned with `-m gpt-6.1-sol -c model_reasoning_effort=high -s workspace-write -a on-request`
runs in its sandbox with `-m gpt-6.1-sol -c model_reasoning_effort=high`. Bare agents get every argument.

The chief always runs bare, whatever `mode`, a workspace or a repository says, and after a restart too.
It needs this machine's tools: `imacs ctl`, git across repositories, builds and screenshots.

## Lifecycle

1. **Start.** A sandboxed agent's tile appears at once. In the background, IMACS checks `sbx` and creates
   a sandbox named `imacs-<session id>`. With the agent image cached this takes about 4 seconds; an
   agent's first sandbox pulls its image, which can take a minute.
2. **Attach.** The tile's terminal runs `sbx run --name imacs-<session id> <agent> -- <args>`.
3. **Idle.** When the agent exits or its tile closes, IMACS stops the sandbox (`sbx stop`). It keeps its
   state and starts again on the next `sbx run` in about 2 seconds.
4. **Restart.** IMACS remembers each session's sandbox. After a restart, or when you resume a session,
   the agent reattaches to the same sandbox, where its conversation lives.
5. **Cleanup.** Removing a worktree in the cleanup dialog, or a completed plan's worktrees in plan
   cleanup, also removes its sandboxes (`sbx rm`) and IMACS's MCP registration for each. The worktree
   cleanup dialog also lists **sandboxes whose worktree is gone**, so you can remove orphans. IMACS only
   lists and removes sandboxes whose names start with `imacs-`.

## What a sandbox sees

The worktree is mounted read-write at the same path as on your machine. Git also needs the repository's
shared folders, and only some of them are mounted:

| Agent works in | Also mounted | Not mounted |
| --- | --- | --- |
| a linked worktree (every task IMACS creates) | the common dir's `objects`, `refs` and `logs`, and the worktree's own `worktrees/<id>`, read-write | the repository's `hooks`, `config` and everything else in `.git` |
| a main checkout | nothing more; its `.git/hooks` and `.git/config` are mounted read-only over the writable checkout | |

These mounts are enough to commit, branch and read history. The agent cannot add a git hook or change
git config that would later run on your machine. Other worktrees, the rest of your home folder, the IMACS
socket and your credentials stay outside the sandbox.

### Git on your machine in a sandboxed folder

The sandbox can still write the files that tell git where a repository is: a worktree's `.git` file, its
`<common>/worktrees/<id>/commondir`, and a main checkout's `.git/commondir`. If git on your machine followed
them, they could lead it to a config of the agent's own, and git would run that config's `core.fsmonitor`
on your machine. IMACS runs git in these folders itself: it polls diffs, checks refs, cleans up worktrees,
commits a task before merging it, and runs `imacs doctor`.

So when a sandbox is made, before it runs, IMACS records where git finds the folder's repository: the work
tree, its git folder and the common dir, in `git-pins/<sandbox>.json` in IMACS's state folder, which no
sandbox mounts. From then on IMACS runs git in that folder with `GIT_DIR`, `GIT_COMMON_DIR` and
`GIT_WORK_TREE` taken from the record, and never reads them from the folder again, also after a restart and
for bare agents in the same folder. The record goes when cleanup removes the sandbox.

- **Layouts git did not make are refused.** A git folder that does not point back at its worktree, or a
  repository inside the worktree or inside another repository's objects, refs, logs or worktrees, stops
  the agent from starting.
- **`extensions.worktreeConfig` is refused.** With it, git reads `config.worktree` in the worktree's own git
  folder, which the sandbox can write, as repository config. IMACS checks this each time a sandboxed agent
  starts. Unset it, or set `isolation = "bare"` for agents in that repository.
- **Submodules are not entered.** A submodule the agent adds could point git at a folder of its choosing,
  so `git status` and `git diff` run with `--ignore-submodules=dirty`. Changes to a submodule's commit still
  show; changes inside it do not.
- **Worktree cleanup checks for changes with the recorded locations**, as strictly as `git worktree remove`
  does, then removes the worktree with `--force`. `git worktree remove` would check with a git of its own
  that follows the worktree's files. Worktrees with submodules are left to you.
- **A folder outside any repository stays that way.** IMACS does not run git in it or in any folder below
  it while the record exists, whatever `.git` the agent creates there.

### Symbolic links in a worktree

An agent can make a symbolic link in its worktree that points at any file on your machine, such as a key
or a credentials file. IMACS shows a link the way git diffs one: as its target path. In the diff, an
untracked link is a new file whose only line is that path, and in the file viewer a link says where it
points. IMACS never opens the file a link points to, or a path through a linked folder. This holds in every
worktree, bare agents' too.

An agent started in a folder outside any repository gets that folder. IMACS refuses to sandbox an agent
in your home folder or any folder above it, since the sandbox would mount the whole home folder
read-write. Long briefs for agents other than Claude are written to a folder of the session's own,
mounted read-only.

## How sandboxed agents talk to IMACS

- **Reports and ticket tools.** For each sandbox, IMACS registers its MCP server with
  `sbx mcp add imacs-<session id> --command …` and loads it into the sandbox (`sbx mcp load`). The
  server runs on your machine as `imacs mcp --agent`
  and offers the same tools as for bare agents (`imacs_report` and the agent ticket tools). It identifies
  its session by the sandbox's name, which stays the same across IMACS restarts.
- **Status.** Hooks run `imacs hook` on your machine, and a sandbox does not have it. A sandboxed agent's
  status comes from its screen, like any agent without hooks: it is working while it shows
  `esc to interrupt`. A question it asks before its first turn, such as a choice menu, a trust, update
  or sign-in screen, shows as needs you.
- **Folder trust.** A new sandbox's Codex has never seen its folder, and would stop at "Trust this
  folder?". IMACS starts sandboxed Codex with the mounted folder trusted
  (`-c 'projects={ "<folder>" = { trust_level = "trusted" } }'`); the sandbox is the boundary.
- **Steering and review comments.** These are typed into the tile's terminal, as for any agent.
- **Approvals.** Sandboxed agents run unattended: Claude uses `bypassPermissions`, and Codex uses
  `approval_policy = "never"`. The VM mounts and sbx network policy replace IMACS approvals. IMACS
  does not show approval banners or enqueue permission requests for these sessions.
- **Model credentials.** These reach the agent through sbx's credential proxy, not through files in the
  sandbox.

## Trust model

- **The worktree and its branch.** A sandboxed agent can change anything in its worktree, and commit.
- **Ref recovery.** Refs live in the shared common dir. IMACS saves a host-only snapshot before a
  sandboxed agent starts, checks it at turn boundaries and on exit/close, and restores changed, deleted
  or newly created refs outside the task branches. A persistent red board alert and a blocked report
  announce recovery or a failed restore. The task branch, the branches of other running agents and of
  unfinished plan tasks are exempt so legitimate commits survive. A done task's branch is not: moving it
  is restored. This is recovery after a turn, not write isolation: shared refs cannot
  identify which concurrent writer moved a ref. Review the diff and branch before integrating.
- **Objects are data.** Objects an agent writes stay on your machine, and git never executes them. Hooks
  and config, which do run on your machine, are out of reach, and git on your machine finds them through
  IMACS's record, not the files the agent can write (see
  [Git on your machine in a sandboxed folder](#git-on-your-machine-in-a-sandboxed-folder)).
- **The network** is whatever sbx's policy allows, plus `allow_hosts`. `allow-all` gives agents the
  whole internet, which an injected prompt can use to send away code from the worktree.
- **What you add widens the sandbox.** `mounts` expose folders read-only. `ssh_agent = true` lets an
  agent sign, and push, as you. Every `mcp` server runs on your machine with your permissions and can do
  whatever its tools do.
- **Bare agents run as you.** In bare mode, an agent can reach everything you can: your files, keys,
  network and other repositories. Use it for agents you supervise, or that sbx cannot run.
- **Repositories only tighten.** A repository's `.imacs/config.toml` can require sandboxes, and then no
  agent or plan task there runs bare. It cannot turn them off, add mounts or hosts, or forward your SSH agent.
- **The chief can mark tasks bare.** A plan task's `isolation = "bare"` runs that agent as you. You see
  it on the card when you approve the plan.
- **sbx's machine-wide settings apply to every sandbox,** including IMACS's: the global network policy,
  `ssh.agentForwardingEnabled` and `ssh.agentSocketPath`. Changing a mode or setting in IMACS is a
  `runs commands` change when the chief proposes it.

## Limits

- Sandboxed agents do not share IMACS's compiler job pool, and cannot run `imacs queue`. Their limits are
  `cpus` and `memory`, which default low enough for several agents at once. sbx's own defaults, every
  core and half the memory for each sandbox, would oversubscribe the machine.
- Refs that `git gc` or `git pack-refs` packed on your machine are not visible inside a sandbox, because
  `packed-refs` is not mounted. Refs a fetch updates are written loose and are visible. A task's branch
  is loose until it is packed, so avoid `git gc` in a repository while sandboxed agents work in it: a
  branch packed meanwhile disappears inside the sandbox.
- Switching modes does not move a running conversation. Sessions resumed after a switch start in the new
  mode, and a conversation that lived in a sandbox is not on your machine.

## Host resource display

Each sandbox tile shows the VM process tree's host resident memory (GiB) and CPU; the top bar totals
these trees when space permits. `100% CPU` means one host core, so a multi-core VM can exceed 100%.
IMACS samples `ps` off the UI thread every two seconds and sums each process once. These are host
process measurements, not the configured memory/CPU limits or guest application usage. Shared daemon
processes and `sbx run` clients are excluded. A dash means no matching VM process or sampling failed;
hover for details. The total reports how many sandbox trees were found, so partial samples are visible.

The sampler assumes the PoC process shape: one VM tree per sandbox, whose VM root command contains
the complete sandbox name `imacs-<session>` (as an argument or path component). It recognizes nerdbox,
sailor, vfkit, qemu, cloud-hypervisor and krun VM executables, then follows child PIDs. Synthetic recorded
`ps` fixtures cover this mapping; validation against real sbx VMs belongs to the e2e task. If sbx changes
its process shape or uses opaque VM IDs, the display stays unavailable until that mapping is updated.
