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

## Fresh machine: one guided command

After installing IMACS, run this in a trusted host terminal:

```sh
imacs setup
# With one workspace's isolation/ticket settings:
imacs setup --workspace daily
```

Setup creates a missing IMACS config and checks git, Claude Code, Codex and sbx (at least
v0.50.1). It displays installation commands and waits while you run them in another terminal.
Linux also requires Ubuntu 24.04+, accessible KVM and membership of the `kvm` group.
After changing groups, log out and back in, then rerun setup.

The current [Docker installation instructions](https://docs.docker.com/ai/sandboxes/install/)
are:

```sh
# macOS (Homebrew):
brew trust docker/tap
brew install docker/tap/sbx
# Ubuntu, sbx only:
curl -fsSL https://get.docker.com | sudo REPO_ONLY=1 sh
sudo apt install docker-sbx
```

Setup then guides Docker browser sign-in, initializes the global network policy only if it
is missing, and reads a missing OpenAI API key over stdin. Claude agents run bare by default;
when they run sandboxed, setup asks for an Anthropic API key over stdin instead (see
[Claude runs bare by default](#claude-runs-bare-by-default)). A Linear ticket profile adds the `sbx mcp add linear` and `sbx mcp auth linear`
steps: the named workspace's with `--workspace`, otherwise the main config's or any saved workspace's,
since setup cannot know which workspace IMACS will open. Browser authentication runs with private
child output; if a browser does not open, run the indicated authentication command in a host
terminal and retry setup. It also offers to remove a stored GitHub credential and disable
SSH forwarding, so the corresponding readiness checks can pass.

Each change asks for confirmation. EOF, declining a step, or Ctrl-C stops setup; running it
again checks actual state and skips completed steps. Existing Anthropic and OpenAI
secrets are reused. No progress file contains credentials. All token input
is hidden on a terminal; pipes can supply newline-terminated answers and credentials over
stdin. Tokens are never command arguments, printed output, logs, or IMACS configuration.
A token can end up filed as a secret's name, or a service name as its sandbox scope, as with
`sbx secret set anthropic <token>`. Setup lists these misfiled secrets by count and scope only and,
with your consent, removes each one, passing its name from sbx's listing straight to `sbx secret rm`
without ever showing it.

After its secret steps, setup reads sbx's listing again through doctor's own credential checks and
stops at the first warning, so it never calls done a secret doctor reports missing. It then verifies
every sandbox check and finishes by running `imacs doctor`. Doctor and
Settings → Sandbox show the same readiness checks. Doctor also includes independent host/workspace diagnostics
(running IMACS, optional editors, hook trust and repository cleanup); setup does not claim to
have resolved those warnings. Start a new sandbox after changing credentials.

## Check sandbox setup

`imacs doctor` (when sandbox mode is configured) and **Settings → Sandbox** share the same
read-only checks. Settings checks run in the background; use **Refresh sandbox checks** after
changing sbx. They check the installed minimum version and Docker sign-in, global network policy,
model credential names, registered MCP servers and their authorization, and SSH forwarding.
An unrelated failed `sbx diagnose` check, such as SSH client configuration, does not hide a
successful Authentication check. Missing setup produces warnings; doctor still only requires git
and valid IMACS configuration. IMACS continues to refuse sandbox launches that fail its runtime
preflight and never falls back to bare.

The policy display describes the **current global rules**, not IMACS's `network` default. sbx
exposes rules rather than preserving an immutable preset name: standard rules are reported as
balanced, allow-all or deny-all, and changed rules as custom. Review custom rules with
`sbx policy ls`. Checks never initialize or replace a policy or start an OAuth authorization flow.

Credential checks report only known names: `anthropic` (checked only while Claude agents run
sandboxed), `openai`, a warning for `github`, and a leftover `CLAUDE_CODE_OAUTH_TOKEN` custom route
as unused, counting global secrets only. A service secret under a
name sbx has no service for, or in a scope that is not an existing sandbox, is a misfiled secret:
doctor warns with their count and scopes, never their names, since sbx can store a token in the NAME
column. Checks discard all raw credential-command errors. Do not paste
`sbx secret ls` output into reports. A stored `github` credential can let agents push as you.
SSH checks also warn about sbx's global `ssh.agentForwardingEnabled` and a fixed
`ssh.agentSocketPath`, in addition to IMACS's `ssh_agent` setting. The runtime removes
`SSH_AUTH_SOCK` when IMACS forwarding is off; a fixed sbx socket can bypass that protection.

## Claude runs bare by default

The built-in `claude` agent has `isolation = "bare"`: Claude agents run on this machine with your
Claude login, as the chief always does, while Codex stays sandboxed. A Claude subscription cannot
authenticate inside a local sandbox with sbx v0.50.1, as found end to end on 2026-10-30:

- **A custom route.** A `sbx secret set-custom --env CLAUDE_CODE_OAUTH_TOKEN --host api.anthropic.com`
  secret gives the VM a placeholder (`sk-ant-oat01-` and 16 random characters). With the built-in
  `anthropic` secret removed (`SBX_CRED_ANTHROPIC_MODE=none`), sbx's proxy strips both
  `Authorization` and `x-api-key` from requests to api.anthropic.com and adds nothing: Claude says
  "Not logged in" and the API answers `401 x-api-key header is required`. A daemon restart does not
  change this.
- **The built-in secret holding a subscription token.** Stored as sbx's `anthropic` secret, with
  `--command` reading it from the OS keychain, the token gets `SBX_CRED_ANTHROPIC_MODE=apikey`: sbx
  writes an `apiKeyHelper` into the VM's Claude settings, and its proxy sends the token as
  `x-api-key`, which the API rejects with `401 invalid x-api-key`.

`sbx secret set --oauth` covers only OpenAI on local sbx; Anthropic OAuth is cloud-only. Sandboxed
Claude on a subscription is planned through an IMACS host credential relay.

To sandbox Claude meanwhile, store an Anthropic API key (`sk-ant-api…`) in sbx's built-in
`anthropic` secret and set the claude agent's `isolation = "sandbox"` (or remove its `isolation`
under `mode = "sandbox"`). With Claude agents sandboxed, `imacs setup` asks for the key and sends it
to `sbx secret set anthropic` over stdin; it refuses a subscription token. Doctor and Settings →
Sandbox then check for the `anthropic` secret. While Claude runs bare they say sandboxed Claude is
not configured, as information rather than a warning. A config created before this default keeps its
`[[agents]]` entry for claude: add `isolation = "bare"` to it. Codex sandboxes use sbx's `openai`
secret, OAuth or API key; use sbx's interactive `sbx secret set openai` to change it.

## Linux hosts

Use **Ubuntu 24.04 or newer**, hardware virtualization enabled in firmware, working KVM
(`/dev/kvm`), and membership in the **kvm** group. Install the Linux sbx release from the
[official releases](https://github.com/docker/sbx-releases), then run `sbx login`.
Doctor and Settings check the Ubuntu version, whether KVM opens read/write and reports the
supported API, and the current session's group membership. A device that exists but is not
accessible is a failed check.

```sh
sudo usermod -aG kvm "$USER"
# Log out and back in so the desktop session gets the group, then:
id -Gn
imacs doctor
```

Load the appropriate `kvm_intel` or `kvm_amd` module if `/dev/kvm` is missing.

## Tickets and direct Linear access

IMACS's ticket layer stays on the **host** in both modes. Its existing ticket configuration and
credentials do not need to be copied into a VM. Scoped ticket tools and reports reach the host
through the per-sandbox IMACS MCP server.

If agents also need Linear's tools directly, register and authorize the server on the host:

```sh
sbx mcp add linear --url https://mcp.linear.app/mcp
sbx mcp auth linear
```

Add `mcp = ["linear"]` to `[isolation.sandbox]` to load it into new sandboxes. Settings and doctor
report a configured server that is missing, needs authorization, or is unavailable; they also
check registered direct servers. These checks never invoke `sbx mcp auth` themselves. MCP
servers run with host permissions, so authorize the services you intend agents to use.

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

A repository's `.imacs/config.toml` can require isolation and select its template:

```toml
[isolation]
mode = "sandbox"

[isolation.sandbox]
template = "imacs-cox:local"
```

With this setting, every agent working in that repository runs sandboxed, even under `mode = "bare"`, an agent's own
`isolation = "bare"` or a plan task's. A repository can also set `[isolation.sandbox] template`, overriding the workspace template. It cannot
set `mode = "bare"` or any other `[isolation.sandbox]` key. Those settings are ignored with a doctor
warning, because a cloned repository must not widen what an agent can reach. Review template images
as code: their contents run inside the VM, with the worktree and git write mounts described below.

## Warm templates

Set `[isolation.sandbox] template = "imacs-cox:local"` in the repository's main checkout,
`.imacs/config.toml`, or in its workspace file, then build it from the checkout you want to warm:

```sh
imacs sandbox template build /path/to/cox
imacs --workspace backend sandbox template build cz3backend --agent claude \
  --dotnet-project soa-src/Cosmoz3.Shared/Cosmoz3.Contracts/Cosmoz3.Contracts.csproj
```

A project is a directory (including a linked worktree), or a project's basename in `--workspace`.
Main and host settings load first, the selected workspace merges over them, and the main checkout's
repository template wins. A workspace template applies to all its projects; use each repository's
config for different templates in a workspace with several projects. Choose the agent image with
`--agent codex` (default), `claude` or `shell`; use the same agent when consuming the saved template.
Tags are local to this host and architecture. sbx recognizes saved snapshots and uses its local image
store; it does not need a registry push. Existing agents keep their existing sandboxes.

The command checks the checkout as it would for an agent's sandbox: it refuses a git layout git did not
make and `extensions.worktreeConfig`, and runs git there with the locations IMACS recorded when a sandbox
has worked in it (see [Git on your machine in a sandboxed folder](#git-on-your-machine-in-a-sandboxed-folder)).
It then creates a temporary sandbox from the agent's original image with an empty disposable
primary workspace (sbx requires it to be writable), mounts the checkout and
its git metadata read-only, and copies tracked source (including tracked local edits) into its own
filesystem. Untracked credentials, host build output and host toolchain directories are excluded.
It installs rustup and stable Rust (including rustfmt and Clippy) for IMACS, runs
`cargo check -p imacs-core`, then stops the builder and saves with `sbx template save`. Cargo's registry, toolchain
and build cache live in the snapshot: the template's Cargo config uses `~/.cache/imacs/target`.
On macOS this avoids mixing Linux and macOS artifacts in the host's `target`.

For .NET, it installs the SDK pinned by `global.json`, or .NET 8 if no SDK is pinned, builds the
selected project and captures the sandbox's NuGet package cache. The default project is
`soa-src/Cosmoz3.Shared/Cosmoz3.Contracts/Cosmoz3.Contracts.csproj`; use `--dotnet-project` for another
repository-relative project. SDK installation and restore need the repository's allowed network hosts.
Private feeds require credentials provisioned inside the sandbox; IMACS never copies host NuGet
credentials into an image. Build failures report sbx's error and do not save a template. The temporary
sandbox is removed on success or failure; the source staging directory is removed before saving.
The ownership ledger also tracks the builder's git-pin record and disposable primary workspace.
A live builder process keeps these resources out of startup cleanup. If the process is interrupted or
removal fails, the next sweep can remove the owned sandbox, record and workspace together.

Rebuild after changing toolchains or dependencies. Review where an image will be shared: snapshots
contain downloaded packages and build artifacts, and can contain data written by repository build
scripts. No IMACS MCP registration or host credentials are added to the template builder.

### Measured on macOS

On 2026-10-30, a real sbx v0.50.1 sandbox on an Apple silicon Mac running macOS 26.5
used 2 CPUs, 4 GiB RAM and the stock `codex-docker` image (`982f4518e7e5`). Rust was
stable 1.99.1 for `aarch64-unknown-linux-gnu`. One sandbox ran at a time.

| Step | Cold image | Saved template |
| --- | ---: | ---: |
| `cargo check -p imacs-core` (Bash `time`, command only) | 16.479 s | 0.128 s |
| Full `sbx exec` (cold includes source staging and toolchain installation) | 26.099 s | 0.435 s |
| `sbx create` | 3.570 s | 2.816 s |

The cold run used the preparation script above: tracked source copied into a temporary directory,
rustup installed inside the VM, and Cargo's cache under `~/.cache/imacs/target`, with two Cargo jobs.
After source staging was removed, the builder was stopped (1.165 s) and saved (21.247 s) with
`sbx template save`. The cold sandbox was removed before creating a fresh sandbox from that tag.
The template run checked the same source from its read-only host mount at a different absolute path;
`bash -lc` found Cargo without additional PATH setup and reused the snapshot's internal cache.
No host `target` or toolchain cache was mounted. These are single samples on a shared machine,
excluding sandbox creation and toolchain installation from the command-only comparison.

The measurement script under `target/t6-bench/` recorded logs and `timings.json` and used an exit/signal
trap for cleanup. Both `imacs-bench-t6-cold` and `imacs-bench-t6-warm` were removed, together with
`imacs-bench-t6:local`. Final `sbx ls --json` returned `[]`; `sbx template ls --json` contained only the
three pre-existing stock images: `docker/sandbox-templates:claude-code-docker` (`4a94d9144a70`),
`docker/sandbox-templates:codex-docker` (`982f4518e7e5`) and
`docker/sandbox-templates:shell-docker` (`b178ca62ec2e`). No secrets, settings or policies were changed.

## Linux warm folders

On Linux, IMACS explicitly mounts existing directories from `[worktrees] warm` read-write, at the
same paths inside the sandbox. These are the worktree's independent reflink copies, so one task does
not modify another worktree's cache. Missing directories, absolute paths, parent traversal, `.git`
components and symlinks are skipped, so in a main checkout they sit beside its read-only `.git`, never in
it. On macOS these mounts are omitted: its compiled artifacts are incompatible with the Linux VM. Other
configured host mounts stay read-only.

When `target` is a warm folder, `CARGO_TARGET_DIR` points to the worktree's `target`, overriding a
saved template's internal Cargo cache. Reuse assumes matching Linux architecture, toolchain and
system libraries; Cargo still rebuilds stale inputs. On other hosts the template supplies Linux caches.

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
It needs this machine's tools: `imacs ctl`, git across repositories, and builds and screenshots of bare
tasks. Sandboxed work it checks inside the sandbox (see [Checking sandboxed work](#checking-sandboxed-work)).

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
   lists and removes only sandboxes recorded in this home's `sandbox-ownership.json`. Quitting waits for sandbox preparation
   and stop jobs to finish; stopped sandboxes remain available for resume.

Completed plans automatically remove their clean task worktrees and delete task branches contained in
the integration branch, using `git branch -d`. Archiving removes clean task worktrees and keeps their
branches. Integration worktrees stay available. Both behaviors can be disabled independently:

```toml
[cleanup]
completed_plans = true
archived_plans = true
```

Dirty worktrees, main checkouts, and worktrees used by another session are kept. If
`[plan] close_done_agents = false`, running task sessions also prevent automatic worktree removal.
Startup runs cleanup in the background. It removes orphaned sandboxes and MCP registrations without
session records only when this `IMACS_HOME` recorded their creation in its ownership ledger. Other
homes' resources and unknown `imacs-*` names are left alone, including resources from before the ledger
existed; IMACS refuses to attach to an unowned sandbox. The sweep also removes stale briefs, unused warm worktrees, and cache clone stages older than
a day. Resources created after startup began are left for a later sweep. Unreadable ownership records
stop the sweep. Cleanup failures appear in the toast, `imacs doctor`, and the Clean up… dialog. On macOS, doctor
also checks Spotlight indexing and explains how to exclude project and worktree folders.

## What a sandbox sees

The worktree is mounted read-write at the same path as on your machine. Git also needs the repository's
shared folders, and only some of them are mounted:

| Agent works in | Also mounted | Not mounted |
| --- | --- | --- |
| a linked worktree (every task IMACS creates) | the common dir's `objects`, `refs` and `logs`, and the worktree's own `worktrees/<id>`, read-write | the repository's `hooks`, `config` and everything else in `.git` |
| a main checkout | its whole `.git` folder, read-only, as one mount over the writable checkout | |

In a linked worktree these mounts are enough to commit, branch and read history. In a main checkout the agent
can edit files but not commit: anything less than the whole `.git` read-only would let it move `.git` aside
and put one of its own in its place, config and all. A mount point cannot be moved, and sbx enforces
read-only mounts on your machine's side of the share: root in the VM, which can unmount or remount them,
still cannot write there. Tasks commit in linked worktrees anyway. A main checkout's sandbox made before
IMACS mounted `.git` this way is refused; remove it (`sbx rm <name>`) and start the agent again.

The repository's `hooks` and `config` stay on your machine, but that alone does not keep the agent's code
from running there. A linked worktree's sandbox writes the worktree, its own `worktrees/<id>` folder, and
the shared objects, refs and logs, and git on your machine reads all of them: followed, they can lead it to
a git config or a hook the agent wrote. The next section says how, and how IMACS keeps its own git from
following them; git you run yourself is not covered. Other worktrees, the rest of your home folder, the
IMACS socket and your credentials stay outside the sandbox.

### Git on your machine in a sandboxed folder

In a linked worktree the sandbox can still write the files that tell git where a repository is: the
worktree's `.git` file and its `<common>/worktrees/<id>/commondir`. If git on your machine followed them,
they could lead it to a config of the agent's own, and git would run that config's `core.fsmonitor` on your
machine. (A main checkout's `.git/commondir` would do the same, which is one reason its `.git` is read-only.)
It also writes the other files there, which git on your machine reads from any checkout of the repository:

- **`<common>/worktrees/<id>/gitdir`** says where the worktree is, and `git worktree list` lists whatever
  folder it names: a repository the agent planted inside that same git folder, with a config of its own,
  or any other folder on your machine. Git run in a planted repository runs its config's `core.fsmonitor`.
- **`HEAD`, `commondir`, `gitdir`, `index` and `locked`** there are read without checking what they are.
  Replaced by a FIFO, git waits on it for good; replaced by a link to `/dev/zero`, it reads without end.
  Listing worktrees, `git status` and deleting a branch all read them.
- **Hooks and an fsmonitor named by path in the checkout**, as `core.hooksPath = .husky` sets up, run
  programs the agent can rewrite.

IMACS runs git in these repositories itself: it polls diffs, lists worktrees, checks refs, cleans up
worktrees, commits a task before merging it, and runs `imacs doctor` and `imacs sandbox template build`.
Git you run yourself, and tools that run git for you, such as an editor that opens every worktree git lists,
follow what these files say. Until the sandbox is gone, run git for a sandboxed worktree from IMACS or the
main checkout, and never open a folder `git worktree list` names inside a `.git` folder.

So when a sandbox is made, before it runs, IMACS records where git finds the folder's repository: the work
tree, its git folder and the common dir, in `git-pins/<sandbox>.json` in IMACS's state folder, which no
sandbox mounts. From then on IMACS runs git in that folder with `GIT_DIR`, `GIT_COMMON_DIR` and
`GIT_WORK_TREE` taken from the record, and never reads them from the folder again, also after a restart and
for bare agents in the same folder. The record goes with its sandbox: when cleanup removes it, when making
it fails, and when IMACS starts and sbx no longer has it, however it was removed. If removing an owned
sandbox leaves a folder whose git metadata changed, its named record is replaced by a
`git-pins/retained-*.json` folder record. The folder stays pinned across restarts until its metadata is
repaired or the folder is removed; otherwise a later status scan could follow the sandbox's redirect.
Worktree removal checks for changes using the pins and atomically restores the recorded git pointers
before asking Git to remove a clean linked checkout. Main checkouts and dirty or locked worktrees stay.

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
  it while the record exists, whatever `.git` the agent creates there. Once the sandbox is gone and the
  folder still has no repository, the record goes.
- **Git never runs inside a recorded git folder.** IMACS refuses to run git in any folder inside a
  recorded git folder or common dir, where the agent can plant a repository.
- **Listed worktrees are checked before IMACS uses them.** A worktree `git worktree list` names is trusted
  when it is the folder recorded for a sandbox of that repository, or when its `.git` names one of the
  repository's `worktrees/<id>` folders and that folder names it back. A folder listed twice is trusted
  neither time. Others are listed, never entered: the Clean up… dialog marks them *untrusted* and never
  removes them, and the sidebar, the startup sweep, plan cleanup and task integration leave them alone.
- **Every checkout of a repository with a sandbox** gets the same treatment, the main checkout and bare
  agents' worktrees included: git runs with `GIT_DIR`, `GIT_COMMON_DIR` and `GIT_WORK_TREE` set from the
  record or, for the repository's other checkouts, from their `.git` and the recorded common dir, and with
  `-c core.fsmonitor=false -c core.hooksPath=/dev/null`. So no hook runs for git that IMACS runs in such a
  repository, including your own: a `pre-commit` hook does not run when IMACS commits a task before merging it.
- **Special files stop git.** Before git runs in a repository with a sandbox, IMACS checks that each
  sandboxed worktree's git folder holds only files and folders. A FIFO, socket or link there stops IMACS's
  git in that repository, with an error naming it, until you remove it; the folder also stays pinned after
  its sandbox goes.
- **Git has time limits.** IMACS stops git it runs after 2 minutes, or 30 for checkouts, commits, merges
  and fetches, in case the agent swaps in such a file after the check or puts one elsewhere git reads.
  The worktree list IMACS refreshes every 30 seconds is read off the window's thread.

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
- **Account usage.** The sidebar's Claude and Codex limits come from the agents' own clients, which in a
  sandbox write them inside the VM: Codex in its usual session log, and Claude through a status line that
  only saves its input to `/tmp/imacs-statusline.json` (IMACS gives a sandboxed Claude no other
  settings). About once a minute, IMACS reads both back from sandboxes whose agent is attached, with one
  `sbx exec` each: the end of the newest Codex rollout and Claude's last status line, bounded in the VM and
  on your machine. The agent can write these files, so a report never counts as newer than the moment it
  was read; at worst an agent misstates the limits it shows.

## Checking sandboxed work

Builds and tests run the agent's code. For a sandboxed session the chief does not run them on your
machine, where that code would run as you. It runs them in the session's sandbox with the chief-only
`imacs_exec` tool, which `imacs ctl` also takes:

```sh
imacs ctl '{"cmd":"exec","session":"<id or title>","command":"cargo test -p imacs-core","timeout_secs":900}'
```

- **Where.** IMACS runs `sbx exec -w <worktree> imacs-<session id> timeout -k 10 <secs> bash -lc '<command>'`.
  The command runs in the session's worktree, at the path the sandbox mounts it, through a login shell that
  finds a template's toolchains. When the session's agent has exited, its sandbox is stopped: `sbx exec`
  starts it, and IMACS stops it again afterwards. A sandbox an agent still runs in stays up.
- **Slots.** The command waits for a test slot (`"class": "qa"` for a QA slot), the same slots `imacs queue`
  gives whole test runs on this machine. Checks across sandboxes stay within `[resources] test_slots` and
  `qa_slots`, together with every agent's full test runs. The session's tile shows the slot while the check
  waits and runs.
- **Time.** `timeout_secs` (default 600, at most 3600) bounds the wait for a slot, and then the run. In the
  VM, coreutils' `timeout` stops the command (exit 124) and kills it 10 seconds later if it ignores that:
  stopping `sbx exec` on your machine alone would leave the command running in the VM.
- **What comes back.** The exit code, whether it timed out, how long it waited and ran, and the last 40,000
  bytes of stdout and stderr, with each stream's full size. No environment is passed in, and none is logged:
  in IMACS's slot list, the values of `NAME=value` words in a command show as `NAME=…`.
- **Afterwards.** The command ran agent code, which can move refs, so IMACS checks the session's refs as
  after a turn (see [Trust model](#trust-model)).
- **Bare sessions are refused.** Their work is already on this machine: the chief runs their checks in their
  worktree, through `imacs queue` for whole suites.

The chief reads every diff through `imacs_diff`, never with git in a sandboxed worktree: IMACS runs that git
with the locations it recorded, while git run there by hand would follow files the agent can write (see
[Git on your machine in a sandboxed folder](#git-on-your-machine-in-a-sandboxed-folder)). In sandbox mode,
a plan's final review task runs in a sandbox of its own, and its brief has it run the plan's builds and
tests there, in a worktree that starts from the integration branch.

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
  agent or plan task there runs bare. It can choose its template, but cannot turn sandboxes off, add mounts or hosts, or forward your SSH agent.
- **The chief can mark tasks bare.** A plan task's `isolation = "bare"` runs that agent as you. You see
  it on the card when you approve the plan.
- **sbx's machine-wide settings apply to every sandbox,** including IMACS's: the global network policy,
  `ssh.agentForwardingEnabled` and `ssh.agentSocketPath`. Changing a mode or setting in IMACS is a
  `runs commands` change when the chief proposes it.

## Limits

- Sandboxed agents do not share IMACS's compiler job pool, and cannot run `imacs queue`. Their limits are
  `cpus` and `memory`, which default low enough for several agents at once. sbx's own defaults, every
  core and half the memory for each sandbox, would oversubscribe the machine. The chief's checks in a
  sandbox (`imacs_exec`) do wait for IMACS's test and QA slots.
- Refs that `git gc` or `git pack-refs` packed on your machine are not visible inside a sandbox, because
  `packed-refs` is not mounted. Refs a fetch updates are written loose and are visible. A task's branch
  is loose until it is packed, so avoid `git gc` in a repository while sandboxed agents work in it: a
  branch packed meanwhile disappears inside the sandbox.
- Switching modes does not move a running conversation. Sessions resumed after a switch start in the new
  mode, and a conversation that lived in a sandbox is not on your machine.

## Host resource display

Each sandbox tile's header shows a box icon and **Sandbox**, followed by the VM process tree's host
resident memory (GiB) and CPU; the top bar totals these trees when space permits. `100% CPU` means one host core, so a multi-core VM can exceed 100%.
IMACS samples `ps` off the UI thread every two seconds and sums each process once. These are host
process measurements, not the configured memory/CPU limits or guest application usage. Shared daemon
processes and `sbx run` clients are excluded. Without a matching VM process, or when sampling fails, the
badge shows **Sandbox** alone; hover for details. The total reports how many sandbox trees were found, so partial samples are visible.

The sampler assumes the PoC process shape: one VM tree per sandbox, whose VM root command contains
the complete sandbox name `imacs-<session>` (as an argument or path component). It recognizes nerdbox,
sailor, vfkit, qemu, cloud-hypervisor and krun VM executables, then follows child PIDs. Synthetic recorded
`ps` fixtures cover this mapping. If sbx changes its process shape or uses opaque VM IDs, the display
stays unavailable until that mapping is updated. That is the case with sbx v0.50.1 on macOS (checked
end to end on 2026-10-30): each VM runs as `containerd-shim-nerdbox-v1 -namespace docker -id <container
id>`, without the sandbox's name, so tiles show **Sandbox** without memory and CPU. The shim keeps the
sandbox's worktree open, which a future mapping could use.
