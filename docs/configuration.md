# Configuration

IMACS reads its settings from TOML files and picks up saved changes while it runs. The chief can help set
IMACS up for your machines and repositories: it reads the effective configuration, runs the doctor, and
proposes changes. It never writes a config file itself. Every change it proposes waits in the IMACS window as
a diff until you apply or reject it.

## Files and scopes

| Source | File | Applies to | Takes |
| --- | --- | --- | --- |
| `default` | built into IMACS | everything | every setting; IMACS writes them, with comments, into a new `config.toml` |
| `main` | `~/.config/imacs/config.toml` | every machine that shares the file | every setting |
| `host` | `config.d/<host>.toml` beside it | this machine | every setting |
| `workspace` | `workspaces/<host>/<name>.toml`, or a flat `workspaces/<name>.toml` shared by every host | the open workspace | `projects`, `[tickets]`, `[workflow]` and `[chief] prompt` |
| `repo` | `<main checkout>/.imacs/config.toml` | one repository | allowlisted declarative workflow settings; see [Repository workflows](repo-workflow.md) |

`IMACS_CONFIG` moves the main file; `config.d/` and `workspaces/` move with it. `IMACS_HOST` overrides the
machine name. See [Using IMACS on several machines](../README.md#using-imacs-on-several-machines) for host
names and portable paths.

The scope of a proposed change is `main`, `host` or `workspace`. Use `host` for anything that is true of one
machine only: its core count, its programs' paths, its repositories. A workspace change may touch `[tickets]`, `[workflow]` and
`[chief] prompt`; open and close its projects in IMACS instead.

## Merge order

IMACS starts from the built-in defaults, then applies `config.toml`, then this host's file:

- Tables merge key by key. A host file with only `[font] size` keeps every other font setting.
- Arrays and other values replace what came before. A host's `agents` or `repos` replaces the whole list.
- A workspace's `[tickets]` replaces the main config's section as a whole: a key it leaves out has its
  default, not the main config's value. A workspace can turn tickets off with `system = ""`.
- A workspace's `[workflow]` also replaces the whole section. A repo may override it for its project.
- Repository config overlays only its documented declarative keys; `[tickets] branch` keeps the workspace provider.
- `[chief] prompt` adds up instead of replacing; see below.

A main, host or workspace file that does not parse leaves IMACS on the last configuration that loaded.
The top bar or toast says so, and `imacs doctor` names the file. A broken repository config is ignored
with a warning; see [Repository workflows](repo-workflow.md).

## The chief's prompt

The chief starts with a built-in way of working (`[chief] system_prompt` replaces it). Your own instructions
go in `[chief] prompt`, in any of the three files. Each file's text adds to the others' instead of replacing it.
The chief's prompt is, in order:

1. the built-in prompt, or `system_prompt`;
2. the ticket workflow, when tickets are configured;
3. `[chief] prompt` from `config.toml`, then `config.d/<host>.toml`, then the workspace file;
4. a bounded section for each open project from `IMACS.md`, `.imacs/*.md` and declarative repo settings, naming its source files.

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
(`imacs mcp --chief`) and are pre-approved for it, because none of them can change anything without you.
Task agents never get them.

### imacs_config_get

Returns the effective configuration:

- `values`: every setting as `key → {value, source}`, where `source` is `default`, `main`, `host` or
  `workspace` or `repo` (when `project` is given). `key` limits the result to keys under a dotted prefix, such as `tickets` or `layout.sidebar_width`.
- `files`: each file with whether it exists and, when it does not parse, the line and column of the error.
- `load_error`: why the files that parse would not load together, naming the key.
- `chief_prompt`: each `[chief] prompt` section with its source, in the order the chief's prompt adds them.
- `workspace`: the open workspace and its projects.
- `project`, when one is given: its main checkout, whether it is open, the sources that apply to it and where
  a new worktree would go.

### imacs_config_propose

Proposes a change to one file:

| Argument | Meaning |
| --- | --- |
| `scope` | `main`, `host` or `workspace`. |
| `remove` | Dotted keys to take out of the file first, such as `layout.sidebar_width`. Removing a table removes all of it. |
| `toml` | The change. Tables merge into the file key by key; arrays and other values replace. |
| `reason` | One sentence, shown to you above the diff. |

IMACS checks the change before you see it, and refuses it with the reason when:

- the change is not valid TOML, removes a key the file does not set, or sets nothing new;
- the file does not parse now (fix it by hand; a proposal never rewrites a broken file);
- the result would not load. The error names the key, for example
  `layout.sidebar_width: invalid type: string "wide", expected f32`. A change to `config.toml` must load on its
  own too, since other machines share it without this host's overrides;
- a workspace change touches anything but `[tickets]`, `[workflow]` and `[chief] prompt`, or its ticket/workflow settings would not load.

It also warns, without refusing, about keys IMACS does not know, about a main-file setting this host's file
overrides, and about a `[tickets]` change the open workspace's section replaces.

The tool returns a proposal id, the changed keys and the warnings, never the diff: its context lines may hold
secrets. At most five proposals wait at once.

### imacs_doctor

Runs the same checks as `imacs doctor` and returns them as JSON: `checks`, each with `ok`, the `check`,
a `hint` when it failed and whether it is only a `warning`, and `facts` such as the host name and the loaded
files. `ok` is true when the required checks pass: git, and a config that loads.

## Reviewing a proposal

The oldest proposal opens in the IMACS window. It shows the file and scope, who proposed it and why, and a
unified diff of the whole file. Settings that can make IMACS do more than look different are labeled above the
diff:

| Label | Settings |
| --- | --- |
| runs commands | `agents`, `approvals`, `chief.command`, `chief.args`, `editor`, `tickets.server.command`, `tickets.server.env`, `tickets.server.url` |
| sends credentials | `tickets.server.token_env`, `voice.provider`, `voice.base_url`, `voice.api_key_env`, `voice.api_key` |
| instructs agents | `chief.prompt`, `chief.system_prompt`, `steer`, `tickets.workflow` |

**Apply** is enabled a moment after the proposal appears, so a click meant for something else cannot apply it.
**Reject** discards it. An optional note goes back to the chief with either.

When you apply a change, IMACS:

1. checks that the file still reads as it did when the change was proposed, and fails the proposal otherwise;
2. copies the old file to `<file>.<unix time>.bak` beside it, with the same permissions;
3. writes the new text through a temporary file in the same folder, keeping the file's permissions. A
   symlinked config, such as one in a dotfiles repository, stays a link and its target is changed;
4. reloads the configuration at once.

The edit keeps the file's comments and layout. A replaced value keeps the comments above its key and after it,
new keys keep comments written above them in the change, and new tables go at the end of the file.

The chief learns the outcome from a `config` event in `imacs_wait`:

```json
{ "status": "config", "config": { "proposal": "config-2", "outcome": "applied", "scope": "host",
  "file": "~/.config/imacs/config.d/work-mac.toml", "backup": "~/.config/imacs/config.d/work-mac.toml.1798000000.bak",
  "note": "Looks right", "chief": "the chief's prompt and launch settings take effect when the chief starts again" } }
```

`outcome` is `applied`, `rejected` or `failed` (with `error`). `backup` is missing when the change created the
file. Waiting proposals block `imacs_restart` and do not survive a restart.

## Trust model

- **Only you apply.** The tools read and propose. No tool, control request or `imacs ctl` command applies a
  proposal: only the Apply button in the IMACS window writes a config file. That is why the chief may use the
  tools without asking.
- **Judge the diff, not the sender.** "From" names the session that called the tool, as far as IMACS can tell.
  Any program that can reach your IMACS socket can propose a change, just as it can send any other control
  request, so read what the change does. The labels point at the settings that matter most: an applied
  `runs commands` change can start any program as you, and an `approvals` change lets agents run commands
  without asking.
- **Secrets stay out of the tools' answers.** `imacs_config_get` and `imacs_doctor` mask the values of
  secret-looking names (names containing `TOKEN`, `KEY`, `SECRET`, `PASSWORD`, `AUTH` or `CREDENTIAL`) in
  `tickets.server.env`, operation arguments and `NAME=value` assignments; the argument after a flag such as
  `--api-key`, or `--token=…`; URL user info and credential query parameters; bearer and basic credentials;
  and `voice.api_key`, which reads `***` when set. Every string is also cleaned of the values those settings
  hold and of the tokens `token_env` and `api_key_env` name, as far as IMACS knows them. `token_env` and
  `api_key_env` stay names. A file that does not parse is reported by line and column, never by quoting the
  line, which may hold a secret.
- **Keep secrets in the environment.** The chief runs as you, and its own tools can read files you can read,
  subject to its permission prompts. Masking keeps secrets out of IMACS's answers; it cannot make a file
  private. Put tokens in environment variables and name them with `token_env` and `api_key_env`.
- **The diff is yours.** It shows the whole file, secrets included, on your screen only. The backup holds the old
  file with the same permissions.
- **Editing by hand still works.** IMACS reloads files you save yourself. A proposal made before your edit fails
  when applied instead of overwriting it.

Repository workflow files are trusted project instructions like `AGENTS.md` and `CLAUDE.md`.
Only declarative keys are accepted from repository config; see [Repository workflows](repo-workflow.md)
for the allowlist, precedence, reload behavior and examples.
