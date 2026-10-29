# Tickets

IMACS plans can follow an external ticket system. A plan and each of its tasks can carry a ticket link.
IMACS reads those tickets live and shows them on the board. It moves their states as the work moves,
comments on them as agents report, and tells the chief when someone changes a ticket.

One generic layer serves every system. IMACS reaches the ticket system's MCP server itself. A per-workspace
`[tickets]` config says which MCP tool implements each generic operation and where the tool's result keeps
each field. Nothing in IMACS knows any particular system: swapping Relay for Linear, or for anything else
with an MCP server, changes only the config.

## Design

- **Links, not copies.** A link holds `system`, `id`, `key`, `url`, `title`, `state` and `updated_at` and is
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
- **No secrets in plans or replies.** Everything a ticket server sends is cleaned before it leaves the
  worker, using the credentials of the job that produced it (the `token_env` value and secret-looking
  `server.env` values), so a result arriving after a rotation is cleaned with the old ones:
  - Errors lose those credentials in any encoding (raw, percent-encoded, JSON- or `\u`-escaped), bearer and
    basic credentials, `Authorization` values, URL user info, secret-looking query values, and long runs
    that look like tokens. A message whose credential is split across lines or content blocks is withheld
    whole, and every error is bounded to 600 characters.
  - Every ticket field read (title, key, state, URL, update time) loses those credentials too, and URLs
    lose user info and secret-looking query and fragment parameters, so the card's link never carries one.
  - Webhook fields and links the chief gives are cleaned the same way when they arrive.

## Configuration

`[tickets]` goes in a workspace file (`~/.config/imacs/workspaces/<host>/<name>.toml`, or a flat
`workspaces/<name>.toml` shared by every host, next to `projects`) or in the main `config.toml`. A
workspace's section wins over the main config's, and `config.d/<host>.toml` can override single keys of the
main config's, such as the server command on one machine. A workspace can turn tickets off with
`system = ""`. Both files reload while IMACS runs; changing the effective config reads every link again. A
broken workspace section is reported and the last good one stays in use. Saving a workspace from IMACS
keeps its `[tickets]` section key for key, though not its comments. The chief's prompt is fixed when the
chief starts, so restart the chief after changing `mode`, `comments`, `states` or `workflow`.

| Key | Meaning |
| --- | --- |
| `system` | The system's name, shown on cards and to the chief. Empty turns tickets off. |
| `mode` | `"board"` (default): a board is a plan, its tickets are tasks. `"ticket"`: a ticket is a plan. |
| `comments` | Who comments: `"agents"` (default) for agents' reports plus IMACS's lifecycle notes, `"chief"` for lifecycle notes and the chief's own comments only, `"off"` for none. |
| `workflow` | Free text added to the chief's prompt: how your team works with tickets. |
| `timeout_secs` | Longest wait for one MCP request (default 30). |
| `server` | The MCP server; see below. |
| `states` | The system's state names for `planned`, `running`, `review`, `done` and `blocked`. Unmapped statuses are never pushed. |
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
| Someone changes a linked ticket | The card shows the new state. A state the chief did not set arrives as a `ticket` event in `imacs_wait` with `from`, `to` and `means` (the IMACS status the new state maps to). When that differs from the task's status, the card says "ticket says running". |

Comments go to the task's ticket. A task without one comments on the plan's ticket in ticket mode, and
not at all in board mode. Comments follow `comments`. With `"off"`, `imacs_ticket_comment` refuses too.

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
states = { planned = "Todo", running = "In Progress", review = "In Review", done = "Done", blocked = "Blocked" }

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
states = { planned = "Todo", running = "In Progress", done = "Done" }

[tickets.operations.get]
tool = "get_issue"
arguments = { id = "{key}" }
result = { id = "", key = "id", url = "url", title = "title", state = "status", updated_at = "updatedAt" }

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

[Linear's default workflow](https://linear.app/docs/issue-property#status-overview) is Backlog → Todo →
In Progress → Done → Canceled. IMACS maps planned to Todo, running to In Progress and done to Done.
Review and blocked have no default equivalents, so both remain unmapped and leave the issue's state
alone. Add `review = "In Review"` or `blocked = "Blocked"` only if your team created those states.
Backlog and Canceled remain user decisions; IMACS does not cancel an issue when a task is blocked.

The profile uses `comments = "chief"`; change it to `"agents"` to post signed agent reports and enable
scoped agent tools. Link the issue to the plan with `ticket = { id = "ENG-123" }`. The optional create
operation needs your real team in place of `ENG`. Check tool names and result paths against your
server version: doctor validates offered tool names, and sync validates results. For a server returning
`identifier` separately from a UUID `id`, set `result = { id = "id", key = "identifier", url = "url",
title = "title", state = "status", updated_at = "updatedAt" }` on get and create.

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
