# Configuration

ATOMICS reads its settings from TOML files and picks up saved changes while it runs. The chief can help set
ATOMICS up for your machines and repositories: it reads the effective configuration, runs the doctor, and
proposes changes. It never writes a config file itself. Every change it proposes waits in the ATOMICS window as
a diff until you apply or reject it.

## Files and scopes

| Source | File | Applies to | Takes |
| --- | --- | --- | --- |
| `default` | built into ATOMICS | everything | every setting; ATOMICS writes them, with comments, into a new `config.toml` |
| `main` | `~/.config/atomics/config.toml` | every machine that shares the file | every setting |
| `host` | `config.d/<host>.toml` beside it | this machine | every setting |
| `workspace` | `workspaces/<host>/<name>.toml`, or a flat `workspaces/<name>.toml` shared by every host | the open workspace | `projects`, `[identity]`, `[tickets]`, `[workflow]`, `[isolation]`, `[chief] prompt` and how its chief starts (`[chief] cwd`, `resume`, `autostart`) |
| `repo` | `<main checkout>/.atomics/config.toml` | one repository | allowlisted declarative workflow settings; see [Repository workflows](repo-workflow.md) |
| `theme` | `themes/<name>.toml` beside `config.toml` | whatever selects the theme | theme tokens; see [themes](themes.md) |

`ATOMICS_CONFIG` moves the main file; `config.d/` and `workspaces/` move with it. `ATOMICS_HOST` overrides the
machine name. See [Using ATOMICS on several machines](../README.md#using-atomics-on-several-machines) for host
names and portable paths.

### The open workspace

`atomics --workspace NAME` (or `-w NAME`) opens a saved workspace; the top bar's **Workspace: NAME** item names the
one open and opens another from the saved ones (the Project menu lists them too, and saves the open projects as
one). ATOMICS remembers the workspace open last (`workspace` in `state/store.json`). A start that names neither a
project nor a workspace, such as from the Dock or `atomics` in a folder outside any repository, opens it again with
the projects that were open, so its `[tickets]`, `[workflow]` and chief settings apply as before. Naming a project
(`atomics ~/repos/app`), or starting `atomics` in a repository or a folder below its root, opens just that project,
with no workspace, as it always has: another repository's workspace settings never apply to it, and the next bare
start opens that project again rather than the earlier workspace. A workspace whose file is gone
is not opened again. `atomics_restart` relaunches with the workspace open at that moment in place of any
`--workspace` the first start had, so a workspace picked after the start survives the restart.

With no ticket system in effect, every ticket tool and the Tickets panel say why: no workspace is open (and which
saved ones have `[tickets]`), the open workspace has no `[tickets]`, or its section does not load.

The scope of a proposed change is `main`, `host`, `workspace` or `theme`. Use `host` for anything that is true of one
machine only: its core count, its programs' paths, its repositories. A workspace change may touch `[tickets]`, `[workflow]`,
`[isolation]`, `[identity]` and `[chief] prompt`; open and close its projects in ATOMICS instead. A theme change writes one theme file; showing
that theme is a separate `main` or `host` change to `[theme] name`.

## Merge order

ATOMICS starts from the built-in defaults, then applies `config.toml`, then this host's file:

- Tables merge key by key. A host file with only `[font] size` keeps every other font setting.
- Arrays and other values replace what came before. A host's `agents` or `repos` replaces the whole list.
- A workspace's `[tickets]` replaces the main config's section as a whole: a key it leaves out has its
  default, not the main config's value. A workspace can turn tickets off with `system = ""`.
- A workspace's `[workflow]` also replaces the whole section. A repo may override it for its project.
- A workspace's `[isolation]` merges key by key, like a host file. A repo may only tighten it to
  `mode = "sandbox"`; see [Agent isolation](isolation.md). A config file that names no `[isolation] mode` means
  `sandbox`; the file a new install starts with says `bare`.
- Repository config overlays only its documented declarative keys; `[tickets] branch` keeps the workspace provider.
- Identity keys resolve from the user-owned project override, then workspace, then main/host config, then Git.
- `[chief] prompt` adds up instead of replacing; see below.

A main, host or workspace file that does not parse leaves ATOMICS on the last configuration that loaded.
The top bar or toast says so, and `atomics doctor` names the file. A broken repository config is ignored
with a warning; see [Repository workflows](repo-workflow.md).

## Build caches and disk space

Non-Shell agents in trusted linked Cargo worktrees default to `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=line-tables-only`, and `CARGO_PROFILE_TEST_DEBUG=line-tables-only`.
When `sccache` is on the session's PATH, ATOMICS selects it as `RUSTC_WRAPPER`, unless a wrapper
is already configured. Bare agents use `$XDG_CACHE_HOME/atomics/sccache-host-v2`
(or `~/.cache/atomics/sccache-host-v2`) with `SCCACHE_CACHE_SIZE=10G` and a separate server socket
for each effective store. Sandboxes use private compiler stores under
`$ATOMICS_HOME/build-caches/sandboxes-v2/`; these are never mounted from the host compiler store.
The VM and dev shell include sccache. A missing sccache wrapper is disabled with a warning;
server I/O errors fall back to rustc unless explicitly configured otherwise.
Shells, the chief, and agents in the main checkout retain their inherited build profile.
Bare sessions preserve inherited environment overrides; sandbox preparation detects sccache inside
the sandbox image. Explicit `[cache] env` values override the defaults for subsequent sessions.

```toml
[cache]
shared = [".npm/_cacache"]
env = { npm_config_cache = ".npm/_cacache", CARGO_PROFILE_DEV_DEBUG = "full" }
outputs = ["web/dist", "src/app/bin", "src/app/obj"]

[resources]
disk_warn_gib = 30
disk_min_gib = 10
```

`shared` links absent folders in new linked worktrees to a store under
`$ATOMICS_HOME/build-caches/host-repos-v2/<repository-id>/` for bare agents, or
`build-caches/sandbox-repos-v2/<repository-id>/` for sandboxed agents. The stores are separate trust domains;
links to another domain are never reused. Links into legacy or forbidden stores are removed on the
next worktree use at configured shared paths and the five repository-allowlisted cache paths, including
allowlisted links left after cache configuration was removed. Repair never scans build output or dependency
trees. Store contents are kept; no cached data is migrated. Link repair pins and rechecks the link through
its parent directory descriptor before unlinking it. Unix has no atomic unlink conditioned on an inode;
an outside writer replacing a link after that final check can still race repair. Each failed or removed
cache folder is skipped independently, and its cache environment path is dropped for that launch. Cache setup errors never refuse a session.
Stores containing tracked files are refused. Checkout-relative cache paths with non-ASCII components
are skipped because Git cannot reliably verify their index case aliases; Unicode checkout roots and
cache directories outside repositories remain supported. Existing real folders are left alone, and the main checkout is
never linked. Existing sandboxes retain their mounts and start without cache environment settings for
missing mounts, with a hint to recreate them. Relative cache environment paths resolve from the worktree root.
All sandboxes of one repository share its `sandbox-repos-v2` package/download stores and can modify each
other's cached packages: they form one repository trust domain. Different repositories and bare host
builds use separate stores. Compiler stores under `sandboxes-v2` remain private to each sandbox.

The CoW `[worktrees] warm` setting still makes independent copies. A `target` warmed from a main-checkout
build with a different Cargo profile will rebuild under the lean agent profile. Use matching profiles when
warming Cargo output, or set `warm = []` and rely on compiler-cache reuse across independent targets.

In your own configuration, `shared` accepts any relative folder and `env` accepts any variable.
Sharing `target`, `node_modules`, `bin`, or `obj` is unsafe for concurrent worktrees: Settings and doctor
warn about these entries. Do not set one common `CARGO_TARGET_DIR`: Cargo locks that directory and
serializes builds across agents. Each worktree keeps its own target; sccache shares reusable compilation
results. Enabling incremental compilation with sccache also requires opting out of the wrapper or
configuring sccache's incremental policy. [sccache's Rust documentation](https://github.com/mozilla/sccache/blob/main/docs/Rust.md)
describes its cacheable compilation modes; metadata-only `cargo check` is not the measurement of reuse.

`outputs` adds disposable output folders. Cargo worktrees automatically include `target`.
Your own configuration may name any relative output folder. The directory itself must be ignored by a
tracked `.gitignore` at or above it, or by `.git/info/exclude`. Cleanup verifies the matching rule with
`git check-ignore -v --no-index` and disables the user's global `core.excludesFile` for both ignore and status
checks. An untracked ignore file at or above the output cannot declare it disposable, even if that file
hides itself with `*`. Nested `.gitignore` files below an already-ignored output are allowed: Git does not
read them to decide whether the output is ignored. Review tracked ignore rules and `.git/info/exclude`
carefully: they declare which untracked files are disposable. Repository configuration has a stricter
allowlist, described in
[Repository workflows](repo-workflow.md#shared-caches-and-disposable-output).

Acceptance plus integration, and session closure, schedule output cleanup even when the worktree is kept.
Only trusted linked worktrees are eligible. Cleanup refuses tracked files, non-ignored untracked files,
case-mismatched or non-ASCII output paths (Git cannot reliably verify Unicode index case aliases),
nested repositories and registered worktrees, mount/device boundaries, links outside
the worktree, shared stores and the main checkout. Removal uses directory descriptors with `O_NOFOLLOW`
and never traverses symlinks or crosses filesystem devices (or Linux mount identities). On Linux,
cleanup requires `statx` with `STATX_MNT_ID` (Linux 5.8 or newer); kernels without it refuse all output
cleanup because mount identity cannot be verified. Preflight checks write and search access on every
directory and the output parent before deleting anything. If permissions or other conditions change
during deletion, a toast and log report partial removal and the bytes left (or that size is unavailable).
Cleanup is scheduled once when a task finishes or a session closes. Automatic cleanup retries an exiting
process up to five times, with waits of 5, 15, 30 and 60 seconds, then stops. Starting a session cancels pending
retries. Live sessions are protected. A manual cleanup skips busy/protected worktrees without queuing retries;
the toast names skipped worktrees and asks you to close their sessions before retrying.
Resume and restart preserve output.

Disk sampling continues without an open Machine window. Below `disk_warn_gib`, the board and Machine
view show a warning with the largest output folders and every refused output with its reason. Hard-linked
files count once across the inventory. Refused outputs that cannot be safely measured show that their size
is unavailable. The load button uses the warning colour, and the
chief receives a `low_disk` heartbeat finding. Tree-size inventory runs on a worker at most every 120 seconds
or after requested cleanup; dispatch checks use the cached inventory. Below `disk_min_gib`, new task dispatches,
new worktrees and new non-chief agent sessions are refused after checking their destination filesystem.
The chief, Shell sessions and resumed sessions continue with a warning, so cleanup and restart recovery remain available.
The board, Machine view and
`atomics_list` offer **Clean build caches of idle worktrees** (`atomics_clean_build_caches` for the chief).
The action applies the same cleanup safeguards; it keeps worktrees and shared stores.

For a reproducible two-worktree comparison and its measured scope, see [Build cache measurement](build-cache-measurement.md).

## Terminal clipboard

`[terminal] clipboard = "focused"` (the default) lets a terminal program write the host clipboard
with OSC 52 only while its tile or editor has keyboard focus in the focused desktop window. A short
notice names the source of each write. Background and sandboxed sessions cannot write the clipboard;
rejected writes are discarded. Set `clipboard = "off"` to disable OSC 52 writes entirely. This setting
does not affect copying selected text yourself. Terminals cannot read the host clipboard with OSC 52.
OSC 52 writes are limited to 1 MiB of decoded text and one accepted write per second per terminal.
Primary/secondary-selection writes are ignored. The emulator disables OSC 52 decoding whenever
clipboard access is denied, including for sandboxed sessions and `clipboard = "off"`.

## Plans and report evidence

Every plan ends with a final review. With both Claude and Codex configured, ATOMICS chooses the other
provider from the one assigned most of the plan's tasks (excluding the review). A tie chooses Codex.
If the other provider is unavailable, it uses a configured coding agent. Set `[plan] review_agent = "codex"`
(or another configured agent name) in the main or host config to override this choice. The review card
explains the selection; an override naming an unconfigured agent falls back to the default choice
with an explanation on the card. A review that has already started keeps its agent.

Agents can attach acceptance evidence to `atomics_report` using optional
`checks: [{check, command, exit, output_tail}]`. `exit` is an integer or `null` when unavailable.
Cards show checks in a collapsed section; `atomics_plan_get` includes compact evidence marked as
untrusted agent data. Reports retain at most 12 checks, 240 characters per check label, 512 per command,
2048 at the end of each output, and 8000 in the summary. Terminal control characters are stripped,
with line breaks and tabs kept. Reports saved before checks were introduced still load.

## Secrets and session scope

Add secrets in **Settings → Credentials → Add secret** or with `atomics secret add NAME`.
The value is accepted on stdin only (masked in a terminal), never as a flag or argument.
`atomics secret list` shows metadata and last relay use; `atomics secret rm NAME` removes one.
Secret values and scope metadata live together in the existing credential catalog, outside TOML.

Use `--scope everywhere` (default), `--scope workspace:daily`, or
`--scope project:/path/to/main/checkout`. A project's scope follows all its linked worktrees, like
`[identity.projects]`; a workspace scope applies only while that saved workspace is open.
Every new bare or Docker session receives its secrets in scope automatically. Changes require a
new or resumed session. A name is unique in the catalog, so remove it before replacing its value.

With `--host api.example.com`, the session receives a placeholder and `NAME_RELAY_URL`. Configure
the tool to use that base URL: the relay forwards only to the named host and adds the real value.
Known services (for example `--host openai`) select their host and auth style. Authentication can
be bearer, `--header x-api-key`, `--basic-user api` (value = password), or `--query api_key`.
`--method GET,POST` and `--path-prefix /v1` limit requests. With no host the secret is **exposed:
agents can read and leak its real value** from their session environment. This warning also
appears in Settings and CLI listings. Docker setup is automatic; no user-run `sbx` step is needed.

Copy requires fresh OS authentication in a native window on the credential machine. Remote
windows, protocol clients, ctl and MCP cannot copy or display values. Linux's fresh-auth polkit
policy ships in the package and NixOS module; for a standalone install enable it with
`sudo atomics install --secret-copy-policy`. A missing policy or OS prompt fails closed. See
[Settings → Credentials](isolation.md#settings--credentials) for routes and prerequisites.

## Commit identity and word guard

Set a default in your main config (or its host override):

```toml
[identity]
name = "Example Contributor"
email = "contributor@example.com"
forbidden_words = ["contoso"]

# Also in your user config, keyed by the project's main checkout, never a task worktree.
[identity.projects."~/repos/app"]
name = "Project Contributor"
email = "project@example.com"
forbidden_words = ["contoso", "internal-example"]
```

A workspace file can set the same three keys in `[identity]`. Each key uses the most specific
setting: `identity.projects.<main checkout>` in the user's main/host config, then the open
workspace's `[identity]`, then main/host `[identity]`, then Git's own name and email. Paths expand
`~` and resolve to the main checkout, including when a task runs in a linked worktree. An omitted
word list inherits; `forbidden_words = []` clears it. Matching is case-insensitive and finds literal
substrings. Repository `.atomics/config.toml` files cannot set identities or project identity maps.

The explorer's project row shows its active identity; its **Change identity…** menu opens
**Settings → Identity**. Choose the main config, the open workspace or a project, enter the name,
email and words, then review and Apply the proposed change. Workspace changes update the display
and existing ATOMICS worktrees. Existing commits keep their original authorship. Remove an override
in the config to inherit from the next scope.

The form prefills only keys stored in the selected scope, and proposes only keys you change.
Inherited values shown in the project row are not copied into an override. Blank fields inherit:
blanking an existing value removes that scope's key. Blank forbidden words never clear an inherited
guard; **Clear inherited words** explicitly writes `forbidden_words = []`. Enter words one per line.

New and reused ATOMICS worktrees receive a conditional repository-local Git config include for just
that worktree. Its identity file lives under the common Git directory's `atomics-identities/`, outside
the sandbox's writable directories. This preserves the main checkout's Git identity and does not
enable `extensions.worktreeConfig`. ATOMICS also supplies the selected identity explicitly for its
automatic task commit and integration merge, for both author and committer.
With no configured name or email, ATOMICS creates no identity file or Git config entry. Removing the
last configured name/email removes ATOMICS's identity include and restores Git's own settings.

Before integration, ATOMICS checks the task branch name, every new commit's raw author, committer,
message and added lines, plus the current diff and all untracked files. It also checks text it will
put into its own commits. A forbidden word or an author email different from the configured email
blocks integration before staging or merging. A mailmap cannot hide a different author. Matches in
earlier commits still block even if later commits removed them. Fix the content or commit history,
then integrate again.

Author email comparison is case-insensitive. It applies only to task commits not already reachable
from the plan's integration branch or the base. Accepted history retains its original authorship,
so enabling an identity mid-plan does not reject authors inherited from already integrated work.
The forbidden-word checks still examine the task's diff and commit metadata.

Each untracked file is scanned up to 4 MiB. An oversized file blocks integration with its file name,
line 1 and an oversize reason; it is never silently skipped or included in the diagnostic text.

Readiness checks show guard failures on the board and in the chief's plan tools. Findings contain
only a file, a 1-based line number and the reason, never the matched word, email or surrounding text.
Commit metadata uses `.git/objects/<commit>` with raw commit-object line numbers; branch names use
`.git/refs/heads/<branch>:1`; pending automatic messages use `.git/COMMIT_EDITMSG` or `.git/MERGE_MSG`.
The guard checks author email when configured; it does not rewrite commits.

`atomics doctor` reports the resolved name and email for every open project (including the saved
workspace) and warns if a workspace defines an identity but a missing key still comes from Git's
global identity.

ATOMICS reads workspace identity independently of other workspace settings. An unrelated validation
error, such as an invalid projects list, does not block agents or integration when identity can be
determined. An invalid `[identity]`, unreadable file, or TOML syntax error that prevents determining
identity blocks those operations until fixed, preserving existing worktree settings meanwhile.

## The chief's prompt

The chief chooses suitable task agents in `[chief] prefer` order (agent names from `[[agents]]`).
The default is `["codex", "claude"]`; `[]` leaves the choice to task fit. Set these preferences in
`config.toml` or this host's `config.d/<host>.toml`:

```toml
[chief]
prefer = ["codex", "claude"]
min_left_percent = { codex = 10, claude = 20 }
```

`min_left_percent` sets an advisory floor per provider (`codex` or `claude`), from 0 to 100, including
fractional percentages. By default no minimums are configured. Agent variants share their provider's
quota: Claude agents use `kind = "claude"`, and Codex agents use `kind = "generic"` with a `codex`
command (an absolute path is allowed). The preference names agents; the minimums name providers.

`atomics_list` returns `chief.prefer` and `chief.min_left_percent`, plus `usage.claude` and `usage.codex`.
Each provider has `session` and `weekly` windows with `left_percent` (quota **left**, counting down from
100 to 0), `resets_at` (Unix seconds, UTC) and `expired`. `observed_at` is the source's Unix timestamp;
`age_secs` is how old that reading is, even if ATOMICS just polled it. A missing window is `null`;
before any reading, `observed_at` and `age_secs` are also `null`. Expired windows remain visible as
historical readings and do not establish current quota. These are the same account readings the sidebar
shows, shared across local and sandboxed agents, not a per-task allowance or spending limit.

The chief checks the age and reset time, favors providers whose reported, unexpired session and weekly
windows meet their minimum, and explains its choice and any exception or unknown/stale reading in each
task's brief. `atomics_plan_set` saves the plan and returns `warnings` when either unexpired window is
strictly below its provider minimum. It never refuses a plan for quota; a missing or expired reading does
not trigger a quota warning. **Settings → Chief** edits the preference and minimums through a proposal.

`[chief] wake_secs` wakes an idle or turn-done chief every 180 seconds by default; set it to `0` to disable,
or to at least `30` seconds. Set it in `config.toml` or this host's `config.d/<host>.toml`.
Work is open when an open plan has a running or review task, or a non-chief agent session exists.
A wake waits a full interval after the last turn ended or the last wake; starting a turn resets the timer.
It is suppressed while the chief works, has a pending permission or approval, has received terminal typing in
the last 60 seconds, or has native keystrokes or a paste pending since its last turn ended or last submit (Enter).
Suggested prompts do not count as user input. Before any native input history exists, the terminal screen is a
fallback that ignores styled or dim suggestion text.
The single line is `ATOMICS wake: <n> new events, <m> heartbeat findings (<short kinds>). Call atomics_wait and act; if nothing changed, post one short line.`
Events are counted since the previous wake; findings and their short kinds come from the latest fleet check
(`none` when empty). `[chief] heartbeat_secs` separately controls that check (default 300 seconds), delivered
through `atomics_wait`.
When finished agents can be cleaned up, the wake appends a short list (up to five session ids and reasons).
Only idle agents with no turn or pending messages and a clean worktree qualify: their task is accepted,
their branch is merged, or they have no task and no changes. The chief should close each listed session
with `atomics_close` or say why it should stay open; listing one does not close it.

Where and how the chief starts: `[chief] cwd` is its folder (`~/` allowed; unset, the main checkout of the
project on screen), `resume` names a Claude session id it continues instead of starting a new conversation,
and `autostart = true` starts it with ATOMICS unless a restart already brings it back. A workspace file's
values win, so `atomics --workspace dev` can open one conversation as the chief of several projects:

```toml
# workspaces/<host>/dev.toml
projects = ["~/Projects/backend", "~/Projects/ATOMICS"]

[chief]
cwd = "~/Projects"
resume = "aaffe6ab-0000-0000-0000-000000000000"
autostart = true
```

The chief starts with a built-in way of working (`[chief] system_prompt` replaces it). Your own instructions
go in `[chief] prompt`, in any of the three files. Each file's text adds to the others' instead of replacing it.
The chief's prompt is, in order:

1. the built-in prompt, or `system_prompt`;
2. the ticket workflow, when tickets are configured;
3. `[chief] prompt` from `config.toml`, then `config.d/<host>.toml`, then the workspace file;
4. a bounded section for each open project from `ATOMICS.md`, `.atomics/*.md` and declarative repo settings, naming its source files.

The files are read when the chief starts, so a change takes effect the next time it starts. For example:

```toml
# config.toml
[chief]
prompt = "Keep briefs under ten lines. Prefer codex for well-specified changes."

# config.d/work-mac.toml
[chief]
prompt = "This laptop has 8 cores: run at most three agents at once."

# workspaces/work-mac/backend.toml
[chief]
prompt = "Every backend plan ends with a migration review task."
```

## The chief's tools

Three tools serve self-configuration. They are available only in the chief's own MCP server
(`atomics mcp --chief`) and are pre-approved for it, because none of them can change anything without you.
Task agents never get them.

### atomics_config_get

Returns the effective configuration:

- `values`: every setting as `key → {value, source}`, where `source` is `default`, `main`, `host` or
  `workspace` or `repo` (when `project` is given). `key` limits the result to keys under a dotted prefix, such as `tickets` or `layout.sidebar_width`.
- `files`: each file with whether it exists and, when it does not parse, the line and column of the error.
- `load_error`: why the files that parse would not load together, naming the key.
- `chief_prompt`: each `[chief] prompt` section with its source, in the order the chief's prompt adds them.
- `workspace`: the open workspace and its projects.
- `project`, when one is given: its main checkout, whether it is open, the sources that apply to it and where
  a new worktree would go.

### atomics_config_propose

Proposes a change to one file:

| Argument | Meaning |
| --- | --- |
| `scope` | `main`, `host`, `workspace` or `theme`. |
| `theme` | With scope `theme`: the theme's name, new or existing (letters, digits, `-` and `_`). |
| `remove` | Dotted keys to take out of the file first, such as `layout.sidebar_width`. Removing a table removes all of it. |
| `toml` | The change. Tables merge into the file key by key; arrays and other values replace. |
| `reason` | One sentence, shown to you above the diff. |

ATOMICS checks the change before you see it, and refuses it with the reason when:

- the change is not valid TOML, removes a key the file does not set, or sets nothing new;
- the file does not parse now (fix it by hand; a proposal never rewrites a broken file);
- the result would not load. The error names the key, for example
  `layout.sidebar_width: invalid type: string "wide", expected f32`. A change to `config.toml` must load on its
  own too, since other machines share it without this host's overrides;
- a workspace change touches anything but `[tickets]`, `[workflow]` and `[chief] prompt`, or its ticket/workflow settings would not load.
- a theme change names no theme or an unusable name, sets a key themes do not have or a value that is not
  usable (such as a color that is not `#rrggbb` or an unknown syntax theme), or extends a theme that does not
  exist or extends itself in a cycle.

It also warns, without refusing, about keys ATOMICS does not know, about a main-file setting this host's file
overrides, about a `[tickets]` change the open workspace's section replaces, about a theme file that replaces a
built-in theme, and about a theme that is not the one on screen.

The tool returns a proposal id, the changed keys and the warnings, never the diff: its context lines may hold
secrets. At most five proposals wait at once.

### atomics_doctor

Runs the same checks as `atomics doctor` and returns them as JSON: `checks`, each with `ok`, the `check`,
a `hint` when it failed and whether it is only a `warning`, and `facts` such as the host name and the loaded
files. `ok` is true when the required checks pass: git, and a config that loads; every other failed check is a
`warning`.

## The Settings window

Settings is one window, the same on the desktop and in a browser, and it fits a phone. Its sections lead with what
people change. A wide window has a section list beside its controls; a narrow window has one column and a back row.
Each window remembers its last section, starting with Agents. The Settings icon stays beside Actions when the top
bar folds and has room for it. Each section reads what it shows as it opens:

| Section | What is in it |
| --- | --- |
| Agents | The configured agents, whether they run sandboxed, what they need, with [a click or the exact command](isolation.md#fresh-machine-clicks-or-one-guided-command) for each step, the [sandbox checks](isolation.md#check-sandbox-setup) and the Claude hooks. |
| Chief | Model, dock, autostart, heartbeat and wake intervals, task agent preference and quota minimums. Review and Apply saves edited fields in main or host config. |
| Identity | The active identity per project, and a form to review and apply a main, workspace or project override. |
| Connections | [MCP servers](isolation.md#mcp-connections) for sandboxed agents. |
| Credentials | [What the OS keychain holds](isolation.md#settings--credentials), never a value, and Azure CLI access. |
| Remote devices | Pairing and paired devices, as in the Remote window. |
| Shortcuts | Fixed navigation keys and editable configured bindings. Invalid keys and conflicts on this client's OS block the proposal. Blank disables a binding. |
| Theme | The [themes](themes.md) with their colors. A click shows one at once and saves it. |
| Updates | This version and the new one, its notes, update progress and errors. A remote client shows the host's version. See [Updates](#updates). |
| Getting started | Reopen the setup screen. |
| Advanced | The config files, **Edit config**, the effective configuration and raw proposals. |

The Chief model picker offers aliases accepted by its command (Claude Code: `fable`, `opus`, `sonnet`, `haiku`)
and a custom full model name. It writes `[chief] model`, translated to `--model` on launch. An existing model flag
in a higher layer's `chief.args` wins over a lower layer's `model` and shows as **Custom (args)**. Within one
layer, explicit `model` replaces only the model flag at launch and preserves the rest of `args`. `-m` is a model
flag only for commands named `claude` or `codex`; wrappers keep their other short flags. Model names cannot be
blank, start with `-`, or contain whitespace. Alias signatures name the Claude family and mark it as an alias;
only a full model ID identifies a concrete version. Model and autostart apply on the next chief start;
intervals and dock changes apply while it runs.

Closing Settings discards unsaved Chief and Shortcut edits, while remembering the selected section. Review
creates a separate pending proposal, which remains available to Apply or Reject after Settings closes.

What can be undone acts at once: picking a theme, the Azure CLI switch, connecting and disconnecting a server. Only what
cannot asks first: removing or replacing a credential, removing unknown entries, revoking a device. **Edit config**
opens the file in an editor on the host, so only a window on the host has it; a remote client proposes a change under
Advanced instead, and a view-only client reads everything and changes nothing. Changes made under Advanced, like the
chief's, and edits under Chief and Shortcuts wait for you as proposals.

## Updates

```toml
[update]
check = true   # look for a new version on start and every 6 hours
source = ""    # a checkout to build new versions from; empty = signed releases
```

ATOMICS looks once on start and again every 6 hours, with a 20-second limit and no retries. A look that fails stays
quiet and shows only in Settings. `check = false` stops looking. `atomics update --check` asks in any case, and
`atomics update` installs from a terminal. See [Updates](how-it-works.md#updates) for what happens then.

`source` points at a checkout. The check compares it with its `origin/main`, and the button moves it there and runs its
install script. That script runs what the checkout holds, with no signature behind it, so a proposal that changes
`source` is labelled "runs commands".

## VM modules (macOS)

```toml
[vm]
modules = ["~/repos/team/team.nix"]
```

Set this in the main config or this Mac's `config.d/<host>.toml`. A host override replaces the whole
list. `atomics vm update` adds these NixOS module files to the guest build; the first `atomics vm setup`
also applies them after booting a new instance. `~` expands to this Mac's home folder.

A module in a folder shared with the guest at the same path is imported **in place**, so relative
imports and nearby files work. Outside the shares only the module file is copied, with a warning:
relative imports and file paths in it will not resolve. Put a team's module and its supporting files
in a shared repository. Modules run Nix code and can change the guest system; a proposal changing
`vm.modules` is labelled "runs commands". See [Your own NixOS modules](nixos-vm.md#your-own-nixos-modules).

`atomics vm update [--yes] [--no-restart]` keeps the VM's disk and sign-ins while updating its system
and Mac wrappers. It uses the HEAD revision of `[update] source` when that checkout is shared with the
guest; otherwise it uses the release matching this Mac's ATOMICS version. It prints the source it used.
This command does not fetch or advance a checkout. See [Updating the VM in place](nixos-vm.md#updating-the-vm-in-place).

## Reviewing a proposal

The oldest proposal opens in the ATOMICS window, above Settings if that is open. It shows the file and scope, who proposed it and why, and a
unified diff of the whole file. Settings that can make ATOMICS do more than look different are labeled above the
diff:

| Label | Settings |
| --- | --- |
| runs commands | `agents`, `approvals`, `chief.command`, `chief.args`, `editor`, `github.actions`, `isolation`, `tickets.server.command`, `tickets.server.env`, `tickets.server.url`, `update.source`, `vm.modules` |
| sends credentials | `github.hosts`, `github.repos`, `github.client_id`, `github.app_slug`, `github.azure_devops`, `tickets.server.token_env`, `tickets.server.oauth`, `tickets.http`, `tickets.operations.*.http`, `voice.provider`, `voice.base_url`, `voice.api_key_env`, `voice.api_key` |
| instructs agents | `chief.prompt`, `chief.system_prompt`, `steer`, `tickets.workflow` |

A theme change also shows the theme itself above the diff: bars, agent rows, a terminal with its ANSI colors, a
diff, the status colors and an accent button, drawn in the proposed colors through whatever it extends. Its
fonts and sizes show once applied.

**Apply** is enabled a moment after the proposal appears, so a click meant for something else cannot apply it.
**Reject** discards it. An optional note goes back to the chief with either.

When you apply a change, ATOMICS:

1. checks that the file still reads as it did when the change was proposed, and fails the proposal otherwise;
2. copies the old file to `<file>.<unix time>.bak` beside it, with the same permissions;
3. writes the new text through a temporary file in the same folder, keeping the file's permissions. A
   symlinked config, such as one in a dotfiles repository, stays a link and its target is changed;
4. reloads the configuration at once.

The edit keeps the file's comments and layout. A replaced value keeps the comments above its key and after it,
new keys keep comments written above them in the change, and new tables go at the end of the file.

The chief learns the outcome from a `config` event in `atomics_wait`:

```json
{ "status": "config", "config": { "proposal": "config-2", "outcome": "applied", "scope": "host",
  "file": "~/.config/atomics/config.d/work-mac.toml", "backup": "~/.config/atomics/config.d/work-mac.toml.1798000000.bak",
  "note": "Looks right", "chief": "the chief's prompt and launch settings take effect when the chief starts again" } }
```

`outcome` is `applied`, `rejected` or `failed` (with `error`). `backup` is missing when the change created the
file. Each config file keeps its newest ten backups, with one copy per distinct content, including applies in the
same second. Cleanup is best-effort and always keeps the snapshot just returned, including when the clock moves
backwards. A cleanup failure never changes a successful apply into a failure. Applying an unchanged file adds no
backup. Waiting proposals block `atomics_restart` and do not survive a restart.

## Trust model

- **Only you apply.** The tools read and propose. No tool, control request or `atomics ctl` command applies a
  proposal: only the Apply button in the ATOMICS window writes a config file. That is why the chief may use the
  tools without asking.
- **Judge the diff, not the sender.** "From" names the session that called the tool, as far as ATOMICS can tell.
  Any program that can reach your ATOMICS socket can propose a change, just as it can send any other control
  request, so read what the change does. The labels point at the settings that matter most: an applied
  `runs commands` change can start any program as you, and an `approvals` change lets agents run commands
  without asking.
- **Secrets stay out of the tools' answers.** `atomics_config_get` and `atomics_doctor` mask the values of
  secret-looking names (names containing `TOKEN`, `KEY`, `SECRET`, `PASSWORD`, `AUTH` or `CREDENTIAL`) in
  `tickets.server.env`, `tickets.http`, operation arguments and `http` keys, and `NAME=value` assignments; the argument after a flag such as
  `--api-key`, or `--token=…`; URL user info and credential query parameters; bearer and basic credentials;
  and `voice.api_key`, which reads `***` when set. Every string is also cleaned of the values those settings
  hold and of the tokens `token_env` and `api_key_env` name, as far as ATOMICS knows them. `token_env` and
  `api_key_env` stay names. A file that does not parse is reported by line and column, never by quoting the
  line, which may hold a secret.
- **Keep secrets in the environment.** The chief runs as you, and its own tools can read files you can read,
  subject to its permission prompts. Masking keeps secrets out of ATOMICS's answers; it cannot make a file
  private. Put tokens in environment variables and name them with `token_env` and `api_key_env`.
  `tickets.server.oauth = true` signs in to a URL server with browser OAuth instead; its tokens live only in the OS
  keychain, never in a config file (see [tickets](tickets.md#signing-in-with-oauth)).
- **The diff is yours.** It shows the whole file, secrets included, on your screen only. The backup holds the old
  file with the same permissions.
- **Editing by hand still works.** ATOMICS reloads files you save yourself. A proposal made before your edit fails
  when applied instead of overwriting it.

Repository workflow files are trusted project instructions like `AGENTS.md` and `CLAUDE.md`.
Only declarative keys are accepted from repository config; see [Repository workflows](repo-workflow.md)
for the allowlist, precedence, reload behavior and examples.
