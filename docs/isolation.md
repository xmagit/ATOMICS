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
# With workspace-specific isolation/ticket settings:
imacs --workspace daily setup
```

Setup creates a missing IMACS config and checks git, Claude Code, Codex, sbx (at least
v0.50.1), and the OS keychain helper. It displays installation commands and waits while you
run them in another terminal. On macOS it uses `security`; on Linux install
`libsecret-tools` and unlock a Secret Service keyring (for example GNOME Keyring).
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
is missing, stores Claude's subscription setup-token in the OS keychain, and registers its
lookup with sbx before removing the built-in `anthropic` secret. It reads a missing OpenAI
API key over stdin. A Linear ticket profile (including a workspace override) adds the
`sbx mcp add linear` and `sbx mcp auth linear` steps. Browser authentication runs with private
child output; if a browser does not open, run the indicated authentication command in a host
terminal and retry setup. It also offers to remove a stored GitHub credential and disable
SSH forwarding, so the corresponding readiness checks can pass.

Each change asks for confirmation. EOF, declining a step, or Ctrl-C stops setup; running it
again checks actual state and skips completed steps. Existing unlocked Claude keychain
entries and OpenAI secrets are reused. No progress file contains credentials. All token input
is hidden on a terminal; pipes can supply newline-terminated answers and credentials over
stdin. Tokens are never command arguments, printed output, logs, or IMACS configuration.
Unknown secret names are ignored even if a token was accidentally filed in the NAME column.

Setup verifies every sandbox check and finishes by running `imacs doctor`. Doctor and
Settings → Sandbox show the same readiness checks, including the keychain lookup when the
subscription route is configured. Doctor also includes independent host/workspace diagnostics
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

Credential checks report only known names: `anthropic`, the custom `CLAUDE_CODE_OAUTH_TOKEN`
route, `openai`, and a warning for `github`. They discard unknown secret names and all raw
credential-command errors: sbx can accidentally store a token in the NAME column. Do not paste
`sbx secret ls` output into reports. A stored `github` credential can let agents push as you.
SSH checks also warn about sbx's global `ssh.agentForwardingEnabled` and a fixed
`ssh.agentSocketPath`, in addition to IMACS's `ssh_agent` setting. The runtime removes
`SSH_AUTH_SOCK` when IMACS forwarding is off; a fixed sbx socket can bypass that protection.

## Claude with a subscription

Run **`imacs setup` in a trusted host terminal** (`imacs sandbox setup` is an alias). It confirms
the credential change, then reuses the `imacs-claude-setup-token` OS keychain entry (account
`imacs`) when readable, or asks you to paste a new
token from `claude setup-token`. Pasted input has terminal echo disabled. A pipe can supply the
answers and token over stdin; the command accepts no token argument. The helper writes the
keychain through stdin, discards keychain and sbx output, and registers a lookup command with
sbx. It verifies the lookup first, then removes the built-in `anthropic` secret if present.
If lookup verification fails, the built-in secret stays in place.

**Remove the built-in `anthropic` secret for subscription OAuth.** Its generated `apiKeyHelper`
outranks `CLAUDE_CODE_OAUTH_TOKEN`, so leaving both configured can make Claude use the wrong
credential. This is a global sbx credential change and affects other sandboxes using that secret.
Restart with a new sandbox after changing credential configuration. The helper never prints a
token, passes one on argv, or writes one to a worktree or IMACS config.

The registration is equivalent to this on macOS (only a lookup command is on argv):

```sh
sbx secret set-custom --host api.anthropic.com --env CLAUDE_CODE_OAUTH_TOKEN \
  --placeholder 'sk-ant-oat01-{rand}' \
  --command '/usr/bin/security find-generic-password -a imacs -s imacs-claude-setup-token -w'
sbx secret rm anthropic
```

On Linux, install `libsecret-tools` and run with an unlocked Secret Service keychain in your
desktop session. The lookup command is:

```sh
secret-tool lookup service imacs-claude-setup-token account imacs
sbx secret set-custom --host api.anthropic.com --env CLAUDE_CODE_OAUTH_TOKEN \
  --placeholder 'sk-ant-oat01-{rand}' \
  --command '/usr/bin/secret-tool lookup service imacs-claude-setup-token account imacs'
```

To provision the Linux entry yourself, `secret-tool store --label='IMACS Claude subscription'
service imacs-claude-setup-token account imacs` reads its value from stdin. Use a trusted terminal
or a password-manager pipe; do not put the value in shell history. The macOS helper uses
`security -i` with the keychain write command on stdin. Keychain access may require approval
from your OS. Keep command helpers and their dependencies outside folders writable by agents.
For OpenAI, use sbx's interactive `sbx secret set openai`; for an Anthropic API key instead of a
subscription, use its interactive `sbx secret set anthropic` flow.

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

Load the appropriate `kvm_intel` or `kvm_amd` module if `/dev/kvm` is missing. For subscription
credentials, install `libsecret-tools` and unlock the desktop keychain as described above.

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
   lists and removes sandboxes whose names start with `imacs-`.

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

The agent cannot add a git hook or change git config that would later run on your machine. Other
worktrees, the rest of your home folder, the IMACS socket and your credentials stay outside the sandbox.

### Git on your machine in a sandboxed folder

In a linked worktree the sandbox can still write the files that tell git where a repository is: the
worktree's `.git` file and its `<common>/worktrees/<id>/commondir`. If git on your machine followed them,
they could lead it to a config of the agent's own, and git would run that config's `core.fsmonitor` on your
machine. (A main checkout's `.git/commondir` would do the same, which is one reason its `.git` is read-only.)
IMACS runs git in these folders itself: it polls diffs, checks refs, cleans up worktrees, commits a task
before merging it, and runs `imacs doctor` and `imacs sandbox template build`.

So when a sandbox is made, before it runs, IMACS records where git finds the folder's repository: the work
tree, its git folder and the common dir, in `git-pins/<sandbox>.json` in IMACS's state folder, which no
sandbox mounts. From then on IMACS runs git in that folder with `GIT_DIR`, `GIT_COMMON_DIR` and
`GIT_WORK_TREE` taken from the record, and never reads them from the folder again, also after a restart and
for bare agents in the same folder. The record goes with its sandbox: when cleanup removes it, when making
it fails, and when IMACS starts and sbx no longer has it, however it was removed. A record whose folder no
longer matches it stays, since the agent may have changed the files git would follow; IMACS logs it, and
removing the folder (or `git-pins/<sandbox>.json`, once you have checked the folder) lets it go.

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
