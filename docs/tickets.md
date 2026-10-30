# Tickets

IMACS plans can follow an external ticket system. A plan and each of its tasks can carry a ticket link.
IMACS reads those tickets live and shows them on the board. It moves their states as the work moves,
comments on them as agents report, and tells the chief when someone changes a ticket.

One generic layer serves every system. IMACS reaches the ticket system's MCP server itself. A per-workspace
`[tickets]` config says which MCP tool implements each generic operation and where the tool's result keeps
each field. Nothing in IMACS knows any particular system: swapping Relay for Linear, or for anything else
with an MCP server, changes only the config.

## Design

- **Links, not copies.** A link holds `system`, `id`, `key`, `url`, `title`, `state` and `updated_at`, optional suggested `branch` and is
  saved with the plan. Ids and states are opaque strings. IMACS stores the system's own state names, and
  the `id` stays what the link was made with, because that is what the operations address. IMACS also keeps
  `synced_at` (when it last read the ticket), `pushed_state` (the state it last set) and `error` (why the
  last ticket operation failed).
- **Two shapes of work.** In `board` mode, a board created in the ticket system maps to one plan, and its
  tickets map to the plan's tasks. The ticket says *what* to do; the task brief says *how*. In `ticket`
  mode, one ticket maps to one plan, and the plan's progress is reported on that ticket.
- **IMACS's status is authoritative.** IMACS pushes a state when its own task status changes, never because the
  ticket differs from it. A state someone set in the ticket system therefore stands until IMACS's status
  changes again. That change reaches the chief as a `ticket` event, and the chief decides whether the plan
  should follow. IMACS never dispatches, accepts or closes anything because a ticket moved, since those
  steps start agents, merge branches and close sessions.
- **One system at a time.** A link remembers the system it was made for. IMACS reads, updates and comments
  only on links of the configured system. After a switch, links of the old system are left alone, and their
  cards say so, so old ids, briefs and reports never reach unrelated tickets; link tickets of the new system
  instead, or switch back. A link made while no system was configured takes the system that first reads it.
- **Never in the way.** Ticket operations run one at a time on a worker thread with a per-request timeout.
  Failures show on the card and in `imacs_plan_get`, and IMACS carries on. After a connection failure,
  jobs fail at once for 15 seconds instead of each waiting for a server that is down. A failed state
  update is retried on the next heartbeat or sync, not on every tick.
- **No secrets in plans or replies.** One cleaner, built from a job's credentials (the `token_env` value and
  secret-looking `server.env` values), cleans every string IMACS keeps or shows from a ticket system:
  errors, every ticket field, URLs, agent reads (property names included), webhook fields, links the chief
  gives, and the doctor's output. The worker cleans each result with the credentials of the job that
  produced it, so a result arriving after a rotation is cleaned with the old ones. A connection remembers
  the token it authenticated with, and a job with another token reconnects.
  - A known credential is masked in place in any encoding (raw, percent-encoded, JSON- or `\u`-escaped, in
    any mix), even when whitespace or a line break splits it. When it only shows once the text is decoded,
    unescaped and stripped of whitespace, the whole field is replaced by `[withheld: contained a credential]`.
  - Errors also lose bearer and basic credentials, `Authorization` values, URL user info, secret-looking query
    values and long runs that look like tokens, and are bounded to 600 characters. Card and webhook fields
    are bounded to 2000; agent ticket reads keep whole strings. Bounds cut only after cleaning, between words.
  - URLs always lose user info. A query or fragment parameter goes only when its decoded name is a credential
    name (`access_token`, `token`, `api_key`, `apikey`, `password`, `passwd`, `secret`, `client_secret`,
    `refresh_token`, `id_token`, `auth`, `authorization`, `sig`, `signature`, `x-amz-signature`,
    `x-amz-credential`, `x-amz-security-token`) or its value holds a known credential; `issueKey`,
    `assignee` and every other parameter stay, so the card's link still opens the ticket.

## Configuration

`[tickets]` goes in a workspace file (`~/.config/imacs/workspaces/<host>/<name>.toml`, or a flat
`workspaces/<name>.toml` shared by every host, next to `projects`) or in the main `config.toml`. A
workspace's section wins over the main config's, and `config.d/<host>.toml` can override single keys of the
main config's, such as the server command on one machine. A workspace can turn tickets off with
`system = ""`. Both files reload while IMACS runs; changing the effective config reads every link again. A
broken workspace section is reported and the last good one stays in use. Saving a workspace from IMACS
keeps its `[tickets]` section key for key, though not its comments. The chief's prompt is fixed when the
chief starts, so restart the chief after changing ticket settings or the declarative `[workflow]` settings.

| Key | Meaning |
| --- | --- |
| `system` | The system's name, shown on cards and to the chief. Empty turns tickets off. |
| `mode` | `"board"` (default): a board is a plan, its tickets are tasks. `"ticket"`: a ticket is a plan. |
| `comments` | Who comments: `"agents"` (default) for agents' reports plus IMACS's lifecycle notes, `"chief"` for lifecycle notes and the chief's own comments only, `"off"` for none. |
| `branch` | Optional branch template: `{key}`, `{key_lower}`, `{key_number}` (numeric suffix of the key), `{slug}` (ticket title), `{branch}` (system suggestion). |
| `workflow` | Free text added to the chief's prompt: how your team works with tickets. |
| `timeout_secs` | Longest wait for one MCP request (default 30). |
| `server` | The MCP server; see below. |
| `states` | The system's state names for `planned`, `running`, `review`, `done`, `blocked` and any lifecycle stage. Unmapped statuses are never pushed. |
| `operations` | Which tool implements each operation; see below. |

### The server

A stdio server is a command. IMACS starts it with your login shell's `PATH`, so `npx` and Homebrew tools
work even when IMACS runs from the Dock:

```toml
[tickets.server]
command = ["relay-mcp", "--stdio"]
env = { RELAY_PROFILE = "work" }   # optional extra environment
```

A streamable HTTP server is a URL. Name the environment variable holding a bearer token; IMACS reads it from
its own environment or your login shell, and never from the config file:

```toml
[tickets.server]
url = "https://tickets.example.com/mcp"
token_env = "TICKETS_TOKEN"
```

Servers that sign in with OAuth in a browser, such as Linear's, run through a local bridge that handles
the sign-in and speaks stdio. [`mcp-remote`](https://www.npmjs.com/package/mcp-remote) is one:

```toml
[tickets.server]
command = ["npx", "-y", "mcp-remote", "https://mcp.linear.app/mcp"]
```

The bridge opens the browser on first use and stores the token for later runs. Run the command once in a
terminal to sign in before IMACS needs it.

### Ticket-aware branches

Set `branch = "issue/{key_number}-{slug}"`, or `branch = "{branch}"` to use the system's suggestion.
The optional operation result path `branch = "gitBranchName"` reads that suggestion (leave it unset
when the server has none). IMACS uses a task's ticket, falling back to the plan ticket in ticket mode.
Task branches using the plan ticket append `-<task-id>` to keep sibling tasks and the integration
branch separate. In ticket mode the plan's integration branch uses the template too.

Names are checked with `git check-ref-format --branch` and the usual base-branch guard. Missing fields,
unknown placeholders or invalid names fall back to the existing `imacs/...` names. Naming is pinned
before branch creation: existing task and integration branches never change after a sync or config edit.

### Operations

Each operation names a tool, its arguments, and where its result keeps the ticket's fields:

```toml
[tickets.operations.get]
tool = "get_issue"
arguments = { id = "{key}" }
result = { id = "uuid", key = "id", url = "url", title = "title", state = "status.name", updated_at = "updatedAt" }
```

| Operation | Used for | Placeholders |
| --- | --- | --- |
| `get` | Reading one ticket: on plan load, on the heartbeat, after each push, after a `ticket_event`. | `{id}`, `{key}` |
| `list` | Board mode: reading a plan's whole board in one call. `items` is the JSON path of the ticket array. | `{board}`, `{board_key}` |
| `update_state` | Moving a ticket as its task moves. | `{id}`, `{key}`, `{state}` |
| `comment` | Lifecycle comments and `imacs_ticket_comment`. | `{id}`, `{key}`, `{body}` |
| `create` (optional) | `imacs_ticket_link` with `create`. `result` must map `id`. | `{title}`, `{body}`, `{board}`, `{board_key}` |

`{system}` works everywhere. `{key}` is the ticket's key when known, otherwise its id. `{board}` is the
plan's link: the board in board mode, the plan's ticket in ticket mode. Placeholders work anywhere in an argument
string, and arguments may be numbers, booleans, arrays and tables. A placeholder whose value is unknown
fails the operation with a message naming it; other text in braces is left alone.

**Results.** The paths read from the tool result's `structuredContent` when it has one. Otherwise IMACS
parses its text content as JSON, and failing that uses the text itself. Paths are dot-separated keys and
array indexes (`status.name`, `issues.0.id`, also `$.issues[0].id`). An empty path leaves a field unread.
A path that is not set reads the field of the same name (`id`, `key`, `url`, `title`, `state`,
`updated_at`). Numbers and booleans become text. A path that stops at an object shows that object on the
card, so it is easy to spot and fix. When none of the paths match, the error quotes the start of the
result, which tells you where the fields really are. `imacs_ticket_sync` is a quick way to see it.

## What happens when

| When | What IMACS does |
| --- | --- |
| A plan loads (startup, `imacs_plan_set`, reopening, linking) | Reads its links. In board mode with `list` configured, one call reads the board and every task ticket on it. |
| Every `[chief] heartbeat_secs` | Reads every open plan's links and retries failed state updates. |
| A task's status changes | Sets its ticket to the mapped state. In ticket mode it sets the plan's ticket to the plan's overall status: blocked if any task is blocked, running while work goes on, review when all remaining work awaits review, done when the plan completes. A ticket linked to a task that has not started is left alone. |
| After every push | Reads the ticket again, so the card shows what the system now says. |
| Dispatch | Comments with the implementation brief and acceptance checks, signed by the chief's model. |
| An agent reports | Comments with the report, signed with the agent's model, for example "This is GPT-6.1 Sol (high), the IMACS agent on task t2 (Add the --json flag)." The model comes from the task's `-m`/`--model` and effort arguments. |
| A task is accepted or blocked | Comments with the note that came with it: signed by the chief through `imacs_plan_update`, "for the user" from the board, or by IMACS when an agent exits or cannot start. |
| The plan completes | Ticket mode: comments on the plan's ticket. |
| Someone changes a linked ticket | The card shows the new state. A state the chief did not set arrives as a `ticket` event in `imacs_wait` with `from`, `to` and `means` (the IMACS status the new state maps to). When that differs from active work's status, the card says "ticket says running". After acceptance or a recorded lifecycle stage, movement is expected progress: the event has `expected: true`, and the card has no drift note. |

Comments go to the task's ticket. A task without one comments on the plan's ticket in ticket mode, and
not at all in board mode. Comments follow `comments`. With `"off"`, `imacs_ticket_comment` refuses too.

## Lifecycle and completion

`[workflow]` is declarative and independent of the ticket provider. It belongs in the main config
(with host overrides) or the active workspace file (which wins as a whole section); a project may override it in `.imacs/config.toml` (see [Repository workflows](repo-workflow.md)). It does not run commands or poll CI.

```toml
[workflow]
done = "pr_opened"
# Successful stages in order; replace the list to insert custom milestones.
stages = ["accepted", "integrated", "pr_opened", "ci_passed", "merged", "deployed"]

[tickets.states]
planned = "Todo"
running = "In Progress"
# For a system without PR automation:
pr_opened = "In Review"
# Optional: done = "Done" (only if opening a PR means done for your team)
```

For a team whose acceptance criterion includes CI:

```toml
[workflow]
done = "ci_passed"
stages = ["accepted", "integrated", "pr_opened", "ci_passed", "security_approved", "merged", "deployed"]

[tickets.states]
pr_opened = "In Review"
ci_failed = "Blocked"
done = "Done"
```

The default is `done = "accepted"`, preserving today's behavior. Task status `done` still means
accepted for dependency dispatch and agent cleanup; it does not bypass the configured plan completion
gate. All tasks, including final review, must be accepted and finish their usual integration/cleanup
checks before the plan completes. A plan stage covers every task; alternatively every implementation
task (excluding final review) must reach the milestone separately. Only then does IMACS complete the
plan, post its closing ticket comment and push the optional `done` mapping. Explicitly closing a plan
as completed also respects the gate; archiving remains available.

The ordered successful stages start with `accepted`; custom names use lowercase letters, digits and
underscores and appear in that list in their desired order. A later successful stage proves earlier
milestones were reached. `ci_failed` is always allowed as a side event, cannot be the completion gate,
and never proves CI passed. Once reached, a milestone stays reached; a later failure does not reopen
a completed plan. Its optional `ci_failed` state mapping still applies. The latest recorded event, comparing plan and task histories, is shown on
the card, including a clickable PR or CI URL. Full event history persists with the plan and appears in
`imacs_plan_get`. A consecutive duplicate event (same stage, URL and note) has no second comment or state push.

The chief reports a stage through one tool:

```json
{"name":"imacs_plan_stage","arguments":{"plan":"parser","stage":"pr_opened","url":"https://github.com/example/repo/pull/42"}}
```

Add `task` to target one accepted task. IMACS's own successful integration records `integrated` too
(integration may be recorded while a task is in review). Other stages require acceptance. We use a
separate stage tool instead of a `pr` field on `imacs_plan_update`: plans and tasks share the same
milestones, and a PR is one of several possible completion criteria. The configured workflow and
stage state maps appear in the chief's prompt. The chief can report stages on completed plans too.

External systems use `imacs_stage_event` or the control socket with the same stage/URL/note fields:

```sh
imacs ctl '{"cmd":"stage_event","plan":"parser","stage":"ci_passed","url":"https://ci.example/runs/12"}'
imacs ctl '{"cmd":"stage_event","system":"linear","key":"ENG-123","stage":"merged"}'
```

Identify one target with `plan` and optional `task`, or by a linked ticket's `system` plus `id`/`key`.
Unknown and ambiguous targets fail without mutation. Events never run commands or dispatch work.
A command-based CI poller is a follow-up; this implementation accepts chief reports and webhook events.

After a plan or task is accepted or records a stage, further external ticket movement is expected
progress. The chief still receives the ticket event, marked `expected: true`; IMACS suppresses the
"ticket says …" note. Active work without acceptance or stages retains its existing drift notes and
`expected: false` events. A plan's milestone also covers its tasks.

Stage comments, such as `PR opened: <url>` and `CI passed`, follow the existing comment policy and
signature rules. Stage state names are arbitrary config keys under `[tickets.states]`; unmapped
stages leave the ticket alone. At the configured completion stage, the `done` mapping takes precedence over that stage's mapping,
when both are configured. Other explicit stage mappings apply even after completion; an unmapped
later successful milestone can also satisfy the done gate. A `ci_failed` mapping takes precedence over
`done`.
State failures use the existing heartbeat/sync retries; comments are best effort, as for other lifecycle
comments. No ticket operation is required to advance IMACS's lifecycle.

## The board

Each card shows a chip with its ticket's key and state; a click opens the ticket in the browser, and
hovering shows its title, system, last update and when IMACS last read it. A failed operation shows in
red beneath the chip until an operation on that ticket succeeds. Failed state updates are retried; a
failed comment is not, and neither is a comment still queued when IMACS quits or restarts. The plan header shows the linked board or ticket. In board mode it adds how many
of the board's tickets have no task yet, plus a **sync** button that reads everything again and retries
failed updates.

## Chief tools

The chief's prompt gains a section describing the configured system, mode, state map, comment policy and
`workflow` text. Its tools, all pre-approved:

- `imacs_plan_set` takes `ticket` on the plan and on each task. An omitted ticket keeps its link; linking
  the same ticket again keeps what IMACS knows about it.
- `imacs_ticket_link {plan, task?, ticket | create | unlink}` attaches a ticket to the plan (its board or
  ticket) or to a task, then reads it. With `create: {title, body}` it creates the ticket through the
  `create` operation and attaches it. In board mode, linking the plan answers with the board's tickets.
- `imacs_ticket_sync {plan?}` reads the plan's (or every open plan's) tickets now, retries failed updates,
  and answers with each plan's links once the retried updates and the reads after them are done. In board
  mode the answer includes which of the board's tickets belong to which task, so tickets without a task stand
  out. Links of another system are named in its errors.
- `imacs_ticket_comment {plan, task?, text}` adds implementation details or decisions to the ticket,
  signed with the chief's model.
- `imacs_wait` with `ticket` in `statuses` receives changes made in the ticket system.

A board-mode session typically starts with the chief linking the board the team created, reading its tickets
from the answer, and writing a plan with one task per ticket.

## Inbound events

External systems and webhook bridges report changes through the control socket or the `imacs_ticket_event`
MCP tool. Fields given apply to the matching links at once, then IMACS reads them again. `board` reads a
board-mode plan's whole board again. Any of `system`, `id`, `key`, `board`, `state`, `title`, `url` and
`updated_at` may be given; `id` and `key` match either the link's id or its key.

```sh
imacs ctl '{"cmd":"ticket_event","system":"relay","key":"RLY-42","state":"Blocked"}'
imacs ctl '{"cmd":"ticket_event","board":"board-7"}'
```

## Examples

### Relay: a board per plan

The board is created in Relay first, and the plan hooks into it. Relay's exact tool names are not known yet: every tool name, argument, result path and state below
is an editable placeholder. Use the names your Relay MCP server lists. The `get` operation must accept
both a ticket id and the board id if agents need to read the plan link. Run doctor to check the mapped
tools, then sync to check the result paths. No provider code needs changing.

```toml
[tickets]
system = "relay"
mode = "board"
comments = "agents"
workflow = "Link the existing Relay board to the plan and one ticket to each task. Product writes what and why; the chief comments implementation details. IMACS posts signed agent reports and maps states live. Use imacs_report for reports, without a second ticket comment."
server = { command = ["relay-mcp"] }
states = { planned = "Todo", running = "In Progress" }

[tickets.operations.get]
tool = "get_ticket"
arguments = { ticket = "{id}" }
result = { id = "ticket.id", key = "ticket.key", url = "ticket.url", title = "ticket.title", state = "ticket.status", updated_at = "ticket.updated_at" }

[tickets.operations.list]
tool = "list_tickets"
arguments = { board = "{board}" }
items = "tickets"
result = { id = "id", key = "key", url = "url", title = "title", state = "status", updated_at = "updated_at" }

[tickets.operations.update_state]
tool = "update_ticket"
arguments = { ticket = "{id}", status = "{state}" }

[tickets.operations.comment]
tool = "add_comment"
arguments = { ticket = "{id}", body = "{body}" }

[tickets.operations.create]
tool = "create_ticket"
arguments = { board = "{board}", title = "{title}", description = "{body}" }
result = { id = "ticket.id", key = "ticket.key", url = "ticket.url", title = "ticket.title", state = "ticket.status" }
```

### Linear: an issue per plan

Linear's MCP server addresses issues by identifier (`ENG-123`) and accepts a state type, name or id when
saving. Check the result paths against your server with one `imacs_ticket_sync`: the error quotes the
result if they do not match.

```toml
[tickets]
system = "linear"
mode = "ticket"
comments = "chief"
workflow = "One Linear issue per plan; the issue says what and why, the plan says how."
server = { command = ["npx", "-y", "mcp-remote", "https://mcp.linear.app/mcp"] }
states = { planned = "Todo", running = "In Progress" }

[tickets.operations.get]
tool = "get_issue"
arguments = { id = "{key}" }
result = { id = "", key = "id", url = "url", title = "title", state = "status", updated_at = "updatedAt", branch = "gitBranchName" }

[tickets.operations.update_state]
tool = "save_issue"
arguments = { id = "{key}", state = "{state}" }

[tickets.operations.comment]
tool = "save_comment"
arguments = { issueId = "{key}", body = "{body}" }

[tickets.operations.create]
tool = "save_issue"
arguments = { team = "ENG", title = "{title}", description = "{body}" }
result = { id = "id", key = "id", url = "url", title = "title", state = "status" }
```

### Let PR automation own review and done

The shipped profiles map only planned and running. Acceptance in IMACS does not necessarily mean
merged or deployed. Let your ticket system's GitHub integration or other automation move the ticket to
review when the PR opens and to done when it deploys. Unmapped states and stages are never pushed.

For teams without that automation, map `pr_opened = "In Review"` to report an actual PR, or
`review = "In Review"` to report IMACS review before acceptance. Map `done = "Done"` only when the
configured completion milestone really means done for your team. Add `blocked = "Blocked"` if that
state should follow IMACS. Backlog and cancellation remain user decisions.

The profile uses `comments = "chief"`; change it to `"agents"` to post signed agent reports and enable
scoped agent tools. Link the issue to the plan with `ticket = { id = "ENG-123" }`. The optional create
operation needs your real team in place of `ENG`. Check tool names and result paths against your
server version: doctor validates offered tool names, and sync validates results. For a server returning
`identifier` separately from a UUID `id`, set `result = { id = "id", key = "identifier", url = "url",
title = "title", state = "status", updated_at = "updatedAt", branch = "gitBranchName" }` on get and create.

### Checking a profile

Copy **one** example into the main config or the workspace file; both examples are also commented in
the default config template. Uncomment just the chosen block. For OAuth, run the bridge command once
in a terminal to sign in, as described in [Linear's MCP setup](https://linear.app/docs/mcp). Then run:

```sh
imacs --workspace work doctor
# Main config only:
imacs doctor
```

Doctor uses the workspace's entire `[tickets]` section when present, including `system = ""` to disable
it; otherwise it uses the main config after host overrides. It initializes the server, follows every
`tools/list` page and checks every configured operation's tool, including optional create. It never
calls a ticket operation or writes a ticket. Startup, authentication, timeout and missing-tool failures
are warnings with configuration hints, preserving doctor's existing exit-code policy. The server is
closed after the check. A successful check proves tool availability, not the argument/result mapping:
use `imacs_ticket_sync` on a linked plan to verify the latter.

### Step by step: Relay

1. **Install the server on this machine.** IMACS starts the ticket server itself, with your login
   shell's `PATH`. Install your Relay MCP server so its command runs in a terminal; `relay-mcp` in the
   profile is a placeholder for it.
2. **Copy the profile.** Put the [Relay block](#relay-a-board-per-plan) in the workspace that should
   follow Relay (`~/.config/imacs/workspaces/<host>/<name>.toml`), or in `config.toml`. A new
   `config.toml` has it commented out between `BEGIN RELAY PROFILE` and `END RELAY PROFILE`.
3. **Point it at the server.** A stdio server is `server = { command = [...] }`. It gets IMACS's
   environment, your login shell's `PATH` and `server.env`; prefer a server that signs in on its own over
   a token in `server.env`, which would sit in the config file. A server reached over HTTP is a URL and
   the name of the variable holding its bearer token:

   ```toml
   [tickets.server]
   url = "https://tickets.example.com/mcp"
   token_env = "RELAY_TOKEN"
   ```

   Export `RELAY_TOKEN` in your login shell's startup file. IMACS reads it from its own environment or
   that shell, never from the config, and `token_env` applies to URL servers only.
4. **Map the tools.** Replace every tool name, argument, result path and state name with what your
   server offers; the profile's are placeholders.
5. **Check.** `imacs --workspace <name> doctor` prints `relay ticket MCP server starts and offers all
   mapped tools`, or `relay ticket MCP check failed (warning)` with the reason. It never calls an
   operation.
6. **Use it.** Restart the chief, whose prompt is fixed when it starts. Ask it to link the board to a plan
   (`imacs_ticket_link`), then to run `imacs_ticket_sync`: a result path that does not match fails with
   the start of the server's answer quoted.

### Step by step: Linear

1. **Install Node.js**, so `npx` runs the bridge.
2. **Copy the profile.** Put the [Linear block](#linear-an-issue-per-plan) in the workspace or
   `config.toml` (commented out between `BEGIN LINEAR PROFILE` and `END LINEAR PROFILE` in a new
   `config.toml`). Replace `ENG` in the `create` operation with your team's key.
3. **Sign in through the bridge once**, in a terminal:

   ```sh
   npx -y mcp-remote https://mcp.linear.app/mcp
   ```

   It opens Linear's sign-in in your browser (through `xdg-open` on Linux) and keeps the token for later
   runs; stop it once it has connected. IMACS then starts the same command with no browser and no
   `token_env`. See [Linear's MCP setup](https://linear.app/docs/mcp).
4. **Check.** `imacs --workspace <name> doctor` prints `linear ticket MCP server starts and offers all
   mapped tools`. A bridge that was never signed in fails here, with the hint to authenticate it in a
   terminal first.
5. **Use it.** Restart the chief, link an issue to a plan with `ticket = { id = "ENG-123" }`, and run
   `imacs_ticket_sync` once to check the result paths.
6. **Optionally, Linear's own tools in sandboxes.** With a Linear profile configured, `imacs setup`
   registers and authorizes sbx's Linear server (`sbx mcp add linear --url https://mcp.linear.app/mcp`,
   `sbx mcp auth linear`). Add `mcp = ["linear"]` to `[isolation.sandbox]` to load it into new sandboxes.
   Agents then use Linear with your authorization, through sbx's gateway; see [Tickets and direct Linear
   access](isolation.md#tickets-and-direct-linear-access).

The Linear profile has `comments = "chief"`, which gives agents no ticket tools. Set `"agents"` to let
them read their ticket and comment on it (see below).

### Agent access

Agents use IMACS's own MCP server. They never receive the external ticket server or its credentials.
When the effective workspace config has `comments = "agents"`, they can call:

- `imacs_agent_ticket_get {}` to read their own task's ticket, or `{plan: true}` to read the plan's board
  or ticket. Both call only the configured `get` operation and return the full result, including details
  the server supplies beyond the card's fields. Neither lists other task tickets or retries state pushes.
- `imacs_agent_ticket_comment {text}` to add a signed comment to their own task's linked ticket. It
  requires an explicit task link; it does not fall back to the plan link. In ticket mode, reports still
  use the existing lifecycle fallback to the plan ticket.

The MCP process takes the session id from `IMACS_PANE`, and the host resolves that exact session to one
task in an open plan. Tool arguments cannot select another session, task, plan or ticket. Access is
refused for missing or ambiguous task ownership, a different configured system, absent links or
operations, and both reads and comments are refused with `comments = "chief"` or `"off"`. Agents cannot
create tickets, change their states, link tickets, or use chief tools through `mcp --agent`.

`imacs_report` remains the only report path. IMACS already posts reports as signed comments according
to the comment policy. Explicit comments are for implementation details or questions, not duplicate
reports. Config changes are checked on each request, so the tools may be advertised while access is
disabled.

### Ticket credentials and the credential relay

| Credential | Kept | Used by |
| --- | --- | --- |
| A stdio server's own sign-in, such as the Linear bridge's | Wherever that server keeps it, on this machine | The server IMACS starts |
| A `token_env` bearer token | An environment variable of IMACS or your login shell | IMACS's MCP client, sent only to `url` |
| sbx's Linear authorization | sbx, on this machine | sbx's MCP gateway, for sandboxes with `mcp = ["linear"]` |
| A Linear API key stored with `imacs setup --credential linear` | The OS keychain | The credential relay, for sessions granted it |

Ticket credentials stay out of agents: IMACS runs the ticket server on this machine, and agents reach
tickets only through `imacs_agent_ticket_get` and `imacs_agent_ticket_comment`, scoped to their own task.
For a sandboxed agent that MCP server runs on this machine too. Every ticket string IMACS keeps or shows
is cleaned of the job's credentials first (see [Design](#design)).

Through the [credential relay](isolation.md#settings--credentials), agents reach the Claude subscription
(sandboxed Claude, and bare Claude with `bare_relay`) and, when enabled, Azure. Nothing ticket-related goes
through it today: the store's Linear entry has a relay rule (`POST /graphql` on `api.linear.app`), but no
session is granted a placeholder for it yet. Bare agents run as you, so they can read what you can,
including a token exported in your shell's startup files or a bridge's stored sign-in.

## Tests

`crates/imacs/examples/fake_ticket_server.rs` is a fake ticket system behind a stdio MCP server. It speaks
two unrelated tool dialects: nested structured results under other field names, and flat JSON text
addressed by key. It keeps its tickets, every call it receives, and tools told to fail or hang in a state
file. The tests in `crates/imacs/src/app/tickets.rs` drive IMACS against it:

- link round trips and loading;
- the operation map with its JSON paths;
- live refresh and pushes on each lifecycle event, in both modes;
- inbound `ticket_event`;
- failures shown on the card, timeouts and the backoff;
- the board's chips and header;
- the same steps against both dialects, where only the config differs;
- the actual Relay and Linear examples extracted from the default template, with the server command
  replaced by the fixture;
- agent session scoping and denied reads/comments without external calls;
- doctor startup, missing tools, pagination, and workspace precedence.

`crates/imacs/src/mcp_client.rs` covers stdio and streamable HTTP. For a focused run in a fresh
worktree, build the fixture first, then run the affected tests:

```sh
cargo build -p imacs --example fake_ticket_server
cargo test -p imacs ticket
```

Run the complete suite through `imacs queue --class test -- cargo test --workspace`.
