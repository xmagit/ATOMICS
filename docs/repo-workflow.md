# Repository workflows

Commit `IMACS.md` and `.imacs/` in a repository to share its workflow with the team. IMACS reads them
from the project's **main checkout**, even when an agent works in a linked worktree. Edits made only in an
agent's worktree take effect after they reach the main checkout.

Opening a project trusts these instructions in the same way as `AGENTS.md` or `CLAUDE.md`: they are
instructions to the chief and coding agents. Review them when opening an unfamiliar repository.
The declarative config has a separate, explicit allowlist: a cloned repository cannot configure an agent
program, ticket server, credentials or approval rules, or make IMACS execute its test/QA hints.

Before reading `IMACS.md`, `.imacs/*.md` or `.imacs/config.toml`, IMACS resolves symlinks and requires
the resulting path to stay inside the canonical main checkout. Links to files inside that checkout are
allowed. The `.imacs` directory is checked before listing topics, so a link outside the checkout is
not walked. Only regular files are read; devices, FIFOs, sockets and directories are refused.
Each read consumes at most 65,536 bytes (64 KiB). Files reaching that limit are refused in full,
including configuration files, rather than using partial instructions or settings. Refused paths produce
doctor warnings naming the file or directory and the reason, and contribute no content to prompts or
agent briefs. These checks limit which files a repository can import; review the accepted instructions
themselves before trusting an unfamiliar repository.

## Instructions

An example `IMACS.md`:

```markdown
# Team workflow

Reference the ticket before starting. Name branches issue/<ticket number>-<short description>.
Title each PR #<ticket key> - <concrete change>, for example #NEO-1234 - Preserve invoice currency.
Include the ticket link and a short description of the resulting behavior in the PR.

While working, run cargo check and the tests for changed modules. Before opening the PR, run
cargo fmt --all -- --check and cargo clippy --workspace --all-targets -- -D warnings.
Run the full suite once through imacs queue --class test -- cargo test --workspace.
Start QA instances through imacs queue --class qa -- <command> with isolated state.

Every plan ends with a review. Resolve review findings before opening a PR. Report the PR URL
with imacs_plan_stage; CI and deployment automation move the ticket's later states.
See .imacs/testing.md for test fixtures and .imacs/review.md for review expectations.
```

Put longer topics in `.imacs/*.md`. IMACS includes `IMACS.md`, then topics in filename order, in a
per-project chief prompt section, naming the source files. Each project's entire section is capped at
12,000 characters and ends with a truncation notice when necessary. Keep the root file concise and put
important conventions first. The agent's brief gets the complete `IMACS.md`, paths to additional topics,
and the accepted declarative settings with their source path.

The project panel's **Repository workflow** shows the instruction files and configuration; hover to read
them. IMACS polls their content fingerprints once per second, including topic additions/deletions, so
same-size edits with unchanged timestamps are detected. Saved settings affect subsequent operations;
new agent briefs and chief starts read fresh instructions. A running chief's startup prompt stays fixed:
restart the chief to give it the updated prompt. Existing branches stay pinned.

## Declarative settings

An example `.imacs/config.toml`:

```toml
base_ref = "origin/main"
pr_title = "#{key} - {title}"
test_commands = [
  "cargo check --workspace",
  "imacs queue --class test -- cargo test --workspace",
]
qa_commands = ["imacs queue --class qa -- env IMACS_HOME=/tmp/imacs-qa IMACS_CONFIG=/tmp/imacs-qa/config.toml IMACS_SOCKET=/tmp/imacs-qa/imacs.sock imacs"]
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
| `base_ref` | string | Base ref for project diffs, worktrees, branch guards, integration and cleanup. |
| `worktrees.warm` | array of strings | Relative cache folders to clone from the main checkout when creating worktrees. Existing path and CoW safeguards apply. |
| `tickets.branch` | string | Branch template; overrides the workspace's `[tickets] branch` for this project. |
| `pr_title` | string | PR title template hint for agents, conventionally `{key}` and `{title}`. IMACS does not create PRs or expand this hint. |
| `test_commands`, `qa_commands` | arrays of strings | Command hints passed to agents. IMACS never executes them. |
| `review_policy` | string | Review instructions passed to agents. |
| `workflow.done` | string | Completion milestone, overriding the workspace workflow for this project. |
| `workflow.stages` | array of strings | Successful stages in order, starting with `accepted`; custom stages are supported. |
| `isolation.mode` | `"sandbox"` only | Every agent in this repository runs in a sandbox, whatever the user's config or the agent's own `isolation` says. A repository can tighten isolation, never loosen it: `"bare"` is ignored with a warning. See [Agent isolation](isolation.md). |
| `isolation.sandbox.template` | string | Image or saved local template for this repository, overriding the workspace template. See [Warm templates](isolation.md#warm-templates). |

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
The chief's `imacs_config_get` with `project` shows these effective settings and `source: "repo"`, plus
file existence and warnings. Without `project` it shows workspace settings. The configuration proposal
tool edits main, host and workspace files only; edit and review repository workflow files through Git.
