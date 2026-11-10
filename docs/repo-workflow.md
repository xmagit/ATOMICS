# Repository workflows

Commit `ATOMICS.md` and `.atomics/` in a repository to share its workflow with the team. ATOMICS reads them
from the project's **main checkout**, even when an agent works in a linked worktree. Edits made only in an
agent's worktree take effect after they reach the main checkout.

Opening a project trusts these instructions in the same way as `AGENTS.md` or `CLAUDE.md`: they are
instructions to the chief and coding agents. Review them when opening an unfamiliar repository.
The declarative config has a separate, explicit allowlist: a cloned repository cannot configure an agent
program, ticket server, credentials or approval rules, or make ATOMICS execute its test/QA hints.

Before reading `ATOMICS.md`, `.atomics/*.md` or `.atomics/config.toml`, ATOMICS resolves symlinks and requires
the resulting path to stay inside the canonical main checkout. Links to files inside that checkout are
allowed. The `.atomics` directory is checked before listing topics, so a link outside the checkout is
not walked. Only regular files are read; devices, FIFOs, sockets and directories are refused.
Each read consumes at most 65,536 bytes (64 KiB). Files reaching that limit are refused in full,
including configuration files, rather than using partial instructions or settings. Refused paths produce
doctor warnings naming the file or directory and the reason, and contribute no content to prompts or
agent briefs. These checks limit which files a repository can import; review the accepted instructions
themselves before trusting an unfamiliar repository.

## Instructions

An example `ATOMICS.md`:

```markdown
# Team workflow

Reference the ticket before starting. Name branches issue/<ticket number>-<short description>.
Title each PR #<ticket key> - <concrete change>, for example #ENG-1234 - Preserve invoice currency.
Include the ticket link and a short description of the resulting behavior in the PR.

While working, run cargo check and the tests for changed modules. Before opening the PR, run
cargo fmt --all -- --check and cargo clippy --workspace --all-targets -- -D warnings.
Run the full suite once through atomics queue --class test -- cargo test --workspace.
Start QA instances through atomics queue --class qa -- <command> with isolated state.

Every plan ends with a review. Resolve review findings before opening a PR. Report the PR URL
with atomics_plan_stage; CI and deployment automation move the ticket's later states.
See .atomics/testing.md for test fixtures and .atomics/review.md for review expectations.
```

Put longer topics in `.atomics/*.md`. ATOMICS includes `ATOMICS.md`, then topics in filename order, in a
per-project chief prompt section, naming the source files. Each project's entire section is capped at
12,000 characters and ends with a truncation notice when necessary. Keep the root file concise and put
important conventions first. The agent's brief gets the complete `ATOMICS.md`, paths to additional topics,
and the accepted declarative settings with their source path.

The project panel's **Repository workflow** shows the instruction files and configuration; hover to read
them. ATOMICS polls their content fingerprints once per second, including topic additions/deletions, so
same-size edits with unchanged timestamps are detected. Saved settings affect subsequent operations;
new agent briefs and chief starts read fresh instructions. A running chief's startup prompt stays fixed:
restart the chief to give it the updated prompt. Existing branches stay pinned.

## Declarative settings

An example `.atomics/config.toml`:

```toml
base_ref = "origin/main"
pr_title = "#{key} - {title}"
test_commands = [
  "cargo check --workspace",
  "atomics queue --class test -- cargo test --workspace",
]
qa_commands = ["atomics queue --class qa -- env ATOMICS_HOME=/tmp/atomics-qa ATOMICS_CONFIG=/tmp/atomics-qa/config.toml ATOMICS_SOCKET=/tmp/atomics-qa/atomics.sock atomics"]
review_policy = "End every plan with a review; resolve findings before opening its PR."

[worktrees]
warm = ["target"]

[tickets]
branch = "issue/{key_number}-{slug}"

[workflow]
done = "ci_passed"
stages = ["accepted", "integrated", "pr_opened", "ci_passed", "security_approved", "merged", "deployed"]
```

| Key | Type | Meaning |
| --- | --- | --- |
| `base_ref` | string | Base ref for project diffs, worktrees, branch guards, integration and cleanup. New worktrees fetch its remote first; a local `main` or `master` starts them from the remote's default branch instead. |
| `worktrees.warm` | array of strings | Relative cache folders or glob patterns to expand in the main checkout and clone into new worktrees. `*` matches one path component; `**` matches directory levels. Patterns look at most 8 levels deep and match at most 256 folders in all (later ones start cold, with a warning). A matched folder is cloned whole, and the pattern that matched it looks no further inside; a later pattern still can. Folders are cloned in pattern order, and one whose destination already exists is skipped: list `target` before `target/debug`, or `target/debug`'s clone creates `target` and the rest of `target` starts cold. Existing path and CoW safeguards apply. |
| `tickets.branch` | string | Branch template; overrides the workspace's `[tickets] branch` for this project. |
| `cache.shared` | array of strings | Known safe compiler/download cache folders linked to a per-repository store; the list below is exhaustive. |
| `cache.env` | table of strings | Known cache or lean-build variables, with path values relative to and inside the checkout; the list below is exhaustive. |
| `cache.outputs` | array of strings | Relative disposable output folders ending in a known build-output name, listed below. Tracked folders and shared stores are always protected. |
| `pr_title` | string | PR title template hint for agents, conventionally `{key}` and `{title}`. ATOMICS does not create PRs or expand this hint. |
| `test_commands`, `qa_commands` | arrays of strings | Command hints passed to agents. ATOMICS never executes them. |
| `review_policy` | string | Review instructions passed to agents. |
| `workflow.done` | string | Completion milestone, overriding the workspace workflow for this project. |
| `workflow.stages` | array of strings | Successful stages in order, starting with `accepted`; custom stages are supported. |
| `isolation.mode` | `"sandbox"` only | Every agent in this repository runs in a sandbox, whatever the user's config or the agent's own `isolation` says. A repository can tighten isolation, never loosen it: `"bare"` is ignored with a warning. See [Agent isolation](isolation.md). |
| `isolation.sandbox.template` | string | Saved local template for this repository, overriding the workspace template. It applies only once `atomics sandbox template build` has built that tag from this repository; any other tag is ignored with a warning. See [Warm templates](isolation.md#warm-templates). |

All other keys are **ignored with doctor warnings**, including `agents`, `[chief]`, `[approvals]`,
`[tickets.server]`, ticket operations, resource settings and other `[isolation.sandbox]` keys. Invalid value types are also ignored and
warned about; an invalid workflow is ignored as a section. A malformed or unreadable repository config is
ignored with a warning; removing it restores workspace/main settings. Warnings include the file and key,
without quoting rejected values.

The merge order is defaults → main → host → workspace → repo. Repository values overlay only the
allowlisted keys: `[tickets] branch` does not replace the workspace's ticket provider or server.
Repository `[workflow]` replaces the workspace workflow **as a whole**, so omitted workflow fields take
the built-in defaults. A plan uses its project's workflow, falling back to its first task's repository when
no project was selected. Put separate project workflows in separate plans.

Branch placeholders and branch validation are described in [tickets.md](tickets.md#ticket-aware-branches).
Lifecycle milestones are described in [Lifecycle and completion](tickets.md#lifecycle-and-completion).
The chief's `atomics_config_get` with `project` shows these effective settings and `source: "repo"`, plus
file existence and warnings. Without `project` it shows workspace settings. The configuration proposal
tool edits main, host and workspace files only; edit and review repository workflow files through Git.

## Shared caches and disposable output

A repository may configure only these concurrency-safe folders in `cache.shared`:

| Folder | Tool and reason |
| --- | --- |
| `.cache/sccache` | Compiler cache; content keyed results and a cache server support concurrent clients. Cargo still uses a separate `target` per worktree. |
| `.npm/_cacache` | npm's content-addressed download cache, rather than the installed dependency tree. |
| `.cache/pip` | pip's HTTP and wheel caches, rather than a virtual environment or installed packages. |
| `.yarn/cache` | Yarn's package archive cache, rather than install state or `node_modules`. |
| `.nuget/packages` | NuGet's package cache, rather than MSBuild's mutable `bin` and `obj` output. |

Cargo's registry/git downloads, npm downloads, pip downloads and NuGet packages normally already use
user-global caches. Keep those defaults unless a repository specifically needs local cache paths.
This feature does not redirect them automatically. Gradle build trees, Maven targets, virtual environments,
`node_modules`, `.next`, `bin`, `obj` and Cargo `target` are mutable installed/build state and are not
repository-allowlisted shared caches. Do not share one `CARGO_TARGET_DIR`: Cargo's directory lock would
serialize agents. Sharing sccache results leaves independent Cargo build directories free to build concurrently.

Repository `cache.env` accepts these variables only:

- `npm_config_cache`, `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR`, `YARN_CACHE_FOLDER`, `NUGET_PACKAGES`:
  relative paths inside the checkout, without `..`, `.git`, or links escaping the main checkout.
- `RUSTC_WRAPPER`: `sccache` or the empty string.
- `CARGO_INCREMENTAL`, `SCCACHE_IGNORE_SERVER_IO_ERROR`: `0` or `1`.
- `CARGO_PROFILE_DEV_DEBUG`, `CARGO_PROFILE_TEST_DEBUG`: `0`, `1`, `2`, `none`, `limited`, `full`,
  `line-tables-only`, or `line-directives-only`.
- `SCCACHE_CACHE_SIZE`: an unsigned size, optionally ending in `K`, `M`, `G`, or `T`.

`cache.outputs` accepts relative folders whose final component is `target`, `bin`, `obj`, `build`,
`dist`, `.next`, or `out`. No globs are expanded. These names only make a folder eligible: cleanup still
refuses tracked files, non-ignored untracked work, case-mismatched paths, symlinks, shared caches,
the main checkout and worktrees used by processes.
Your own config can name other relative output folders or shared folders and any environment variable;
known unsafe sharing choices produce Settings and doctor warnings.

```toml
[cache]
shared = [".npm/_cacache"]
outputs = ["web/dist", "src/app/bin", "src/app/obj"]
env = { npm_config_cache = ".npm/_cacache", CARGO_PROFILE_TEST_DEBUG = "line-tables-only" }
```

Host and sandbox shared stores occupy separate namespaces. Repository configuration cannot set `SCCACHE_DIR`;
ATOMICS chooses an isolated compiler store and server. Stores containing tracked files are refused.

Repository cache arrays replace the corresponding user arrays; `cache.env` supplies variables only when the
user has not explicitly set the same key.
Omitted cache keys keep user settings and detected defaults. See [Configuration](configuration.md#build-caches-and-disk-space)
for Cargo defaults, disk thresholds and cleanup actions.
