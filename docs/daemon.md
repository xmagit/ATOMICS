# A native host with a browser client

This is a migration design, not an implemented daemon or wire API.
The destination is one native host per user, with native and browser egui clients.
The browser compiles for wasm32 and renders with WebGPU; the host needs no display or GPU.
Closing a window or losing a connection must leave agents, editors, hooks and plans running.
The desktop application remains usable at every phase below.

## Ownership and current source map

Paths in the next two lists are relative to `crates/imacs/src/`.
These are existing files and types; the split describes responsibilities to extract from them.

- `app.rs`: move the runtime parts of `App` and `Session` to the host: agent launch and resume,
  session identity, status tracking, delivery retries, held messages, approvals, plans and persistence.
  `AgentLaunch`, `Pending`, `PendingApproval`, `StatusEvent` and `Waiter` belong with that runtime.
  Worktree creation, integration, cleanup, filesystem watching and their timers also belong there.
  None of this work may depend on `eframe::App::update` or a repaint being requested.
- `terminal.rs`: the host owns the PTY, child process, emulator, input queue and terminal queries
  currently inside `Terminal`, `Shared` and `Listener`. Keep alacritty emulation native.
  Replace the listener's egui wakeups with runtime notifications; answer color and size queries locally.
  `Terminal::screen_text` stays useful for MCP, but lacks the cells and modes needed for a UI.
- `diff_view.rs`: move `DiffWorker`, `Cache`, `Job`, `WorkerResult` and preparation of `Prepared`
  and `FileView` to the host. Retain coalescing, file limits, untracked budgets and cache reuse.
  Strip egui repaint callbacks from workers and publish completed revisions instead.
- `highlight.rs`: run `Highlighter` on the host. Convert `Span` and `Spans` into byte ranges and
  plain RGBA values at the boundary; `egui::Color32` is not part of the protocol.
- `ipc.rs`: the host owns the Unix listener, `Hub`, `Channels`, `HookMessage`, `ControlMessage`,
  `ControlOutcome` and `ApprovalReply`. Hook replies and control requests work without any client.
  Preserve ancestry/pane routing, queued external-session messages and approval expiry semantics.
- `hook_client.rs` and `mcp.rs`: remain native CLI adapters to the host's local socket.
  Keep the restricted `imacs mcp --agent` report surface distinct from the chief's control tools.
- `setup.rs` and `doctor.rs`: native program discovery, version probing, hook installation and diagnostics.
  `main.rs`: separate native CLI dispatch and host startup from window/browser startup.

The client keeps these responsibilities:

- `app.rs`: the UI portion of `App`, including tabs, dialogs, selection, chief docking, focus,
  steering drafts, board drawing and layout. Runtime actions become explicit commands.
- `terminal.rs`: painting and `Metrics`, keyboard/pointer handling, selection and local clipboard use.
  The client renders a cell cache instead of locking an emulator or holding a PTY sender.
- `diff_view.rs`: rendering, `DiffState`, `ExplorerState`, `DiffAction`, scrolling and comment drafts.
  Dirty scheduling moves to the host; accepted comments are shared host state for MCP and review delivery.
- `theme.rs`: `Palette`, fonts, shortcuts and egui styling remain client code.
- `voice.rs`: native microphone capture through `Recorder` stays on the native client; the browser
  uses its microphone permission flow. Audio upload and transcription are optional negotiated features.

Paths below are relative to `crates/imacs-core/src/`, which is UI-free but not yet browser-safe.

- Host: `git.rs` (`Worktree`, `WorktreeEntry`, `Integration`, `Merge`), filesystem operations in
  `workspace.rs` (`Workspace`), `paths.rs`, `store.rs` (`Store`, `SessionRecord`) and `plan.rs` persistence.
- Shared data/logic: `diff.rs` (`WorktreeDiff`, `FileDiff`, `Hunk`, `DiffLine`, `LineKind`, `SplitRow`),
  `session.rs` (`AgentStatus`), `plan.rs` (`Plan`, `Task`, `PlanStatus`, `TaskStatus`, `Report`),
  `files.rs` (`Dir`), `layout.rs` (`Layout`, `LayoutConfig`, `DiffSplit`, `DiffPlacement`) and
  `steer.rs` (`ReviewComment`). Keep filesystem/process functions outside the browser dependency graph.
- Host: `hook.rs` (`HookRequest`, `HookReply`, `HookEvent`), `approvals.rs` (`ApprovalPolicy`,
  `ApprovalRequest`, `Decision`) and execution of `control.rs` (`ControlRequest`, `ControlResponse`).
  Share only the data required by a client; approval policy evaluation remains authoritative on the host.
- `config.rs` (`Config`) splits host settings/secrets from a safe client settings projection.
  `voice.rs` transcription HTTP calls run on the host so provider credentials never reach the client.

Keep reusable logic and unit tests in the Rust 2024 core, with native dependencies behind target boundaries.
Do not assume existing structs can be serialized unchanged: some contain native handles, egui types,
shared pointers or fields that must remain private. Explicit wire projections separate those concerns.

## Client protocol

Serve versioned static client assets and a same-origin WebSocket endpoint from the host.
Start with JSON text messages and the `imacs.v1` WebSocket subprotocol, over WSS for remote use.
The following lowercase names are proposed wire tags, not existing Rust types or commands.
Each envelope carries `kind`, `request_id` when correlated, and a negotiated major/minor version.
Host messages carry a boot `epoch`; IDs and revisions are opaque strings, including sequence numbers,
so browser number precision cannot corrupt cursors. Paths are host paths, never browser filesystem paths.

| Direction | Tags | Payload and meaning |
| --- | --- | --- |
| Client → host | `hello` | Supported versions, capabilities, client ID, prior epoch and last applied event cursor. |
| Host → client | `welcome` | Chosen version, capabilities, limits, epoch, client role and resume/reset decision. |
| Client → host | `subscribe`, `unsubscribe` | Session screens, worktree diffs/files and plans of interest; visible/focused hints. |
| Host → client | `state_snapshot` | Sessions, projects, plan summaries and pending approvals at a single event cursor. |
| Host → client | `session_upsert`, `session_removed` | Stable session ID, agent, title, cwd, worktree, status, chief flag and task link. |
| Client → host | `session_command` | Start, close, steer or open an editor; return `command_result` with delivery state. |
| Client → host | `terminal_input`, `terminal_resize` | Session ID, control lease, input sequence or rows/columns/cell pixels. |
| Host → client | `screen_snapshot`, `screen_delta` | Session ID, screen generation, base/new revision, cells, cursor and modes. |
| Client → host | `history_get` | Session ID and bounded scrollback range by stable line ID. |
| Host → client | `history_chunk` | Retained lines or an explicit indication that the requested history was evicted. |
| Host → client | `diff_summary`, `diff_file`, `file_content` | Worktree ID, revision, base commit, paths, hunks, counts and highlight spans. |
| Client → host | `diff_get`, `file_get`, `review_command` | Bounded content request, or add/remove/send comments against a diff revision. |
| Host → client | `plan_upsert`, `plan_removed`, `task_report` | Explicit plan/task IDs, revisions, approval state, integration branch and report. |
| Client → host | `plan_command` | Set, approve, dispatch, update, integrate or archive a specified plan/task. |
| Host → client | `approval_pending`, `approval_resolved` | Approval ID, session, request details, deadline, revision and final disposition. |
| Client → host | `approval_decide` | Approval ID and expected revision, decision and optional message. |
| Both | `ack`, `resync`, `ping`, `pong` | Applied event/screen cursors, reset request and application heartbeat. |
| Host → client | `command_result`, `error` | Request ID, operation ID if applicable, result or stable error code and explanation. |

Major mismatch fails before subscription; minor changes are additive and capability-gated.
Unknown optional fields are ignored; unknown commands fail explicitly. Never silently ignore a mutation.
Keep native CLI/MCP compatibility through an adapter for `ControlRequest` and `ControlResponse`.
UI commands such as activate, resize-window and screenshot target a chosen client, with a clear error
when none is attached; they do not acquire global focus or write arbitrary server-side screenshot paths.

The host serializes state transitions and assigns a monotonic event cursor; per-entity revisions detect
stale plan edits and reviews. Replies distinguish accepted work from a completed merge or delivered message.
Mutations carry an operation ID: journal acceptance before side effects and retain the outcome for retries.
A retry of that ID returns the known outcome; a crash during execution yields an explicit uncertain result
until reconciled against worktrees/session records. Never promise exactly-once process or git effects.
Expired operation IDs cannot be rerun as new work automatically; reconnect requires fresh user intent.
PTY input uses an ordered sequence within a control lease and is never replayed across a disconnect.
Bound message sizes, subscriptions, pending operations and queues; reject excess work with a typed error.

## Terminal streaming and diff cost

The host consumes every PTY byte even when no client is attached; the UI receives emulator state,
not raw ANSI output, screenshots or the whole scrollback on each paint.
A screen snapshot includes dimensions, generation, visible rows, cursor position/shape/visibility,
terminal modes, title and palette. Cells include text with combining characters, display width,
wide-cell continuation flags, attributes and indexed/default/RGB colors.
Keep terminal color-query answers consistent with the host's configured terminal palette.

Use emulator damage information to send changed row runs, plus cursor/mode changes and scroll operations.
Every delta names the snapshot generation and exact base revision; resize or alternate-screen switches
start a new generation with a snapshot. A mismatch requests a snapshot instead of guessing at missing cells.
Preserve wrapped-line metadata and stable retained line IDs for selection/copy and independent history views.
Scrolling one client's history never changes another client's viewport or the host's live-screen position.

Coalesce focused output at roughly the existing 100 ms cadence, visible output at 200 ms, and stop screen
traffic for unsubscribed panes. Input echo can flush immediately; idle terminals send no cell traffic.
Copy state under a short emulator lock and encode/send after unlocking; never block the PTY on a socket.
Share snapshots between subscribers at the same revision and deduplicate repeated style runs.
Use bounded outbound queues: discard superseded screen deltas and replace them with a current snapshot.
Disconnect persistently slow clients rather than delaying approvals or accumulating unlimited memory.
Chunk snapshots/history/diffs below the negotiated frame budget, with transfer IDs and bounded assembly;
apply a completed snapshot atomically. Compression or binary encoding can follow measured JSON costs.

Clients encode keys/paste using the latest published modes; the host validates lease and size limits.
Encode terminal mouse reports from cell coordinates; selection and clipboard permission stay on the client.
Clipboard escape sequences require a client opt-in; a remote process cannot silently set a browser clipboard.
Resizing comes only from the controller and is debounced; viewers scale or letterbox the canonical grid.

Diffs are computed once per worktree/revision, with `WorktreeDiff`'s merge-base and partial-content semantics.
Send summaries first and fetch hunks/files on demand; retain current binary/large-file notices and budgets.
Send highlight byte ranges and RGBA colors keyed by content revision and syntax theme, never egui text jobs.
Changing the syntax theme invalidates those spans; split/unified layout and glyph placement remain local.
Reject comments against replaced content until the client refreshes its line anchor.

## Reconnect and concurrent clients

Browser sleep and WebSocket loss detach clients without closing sessions or changing task status.
Reconnect with jittered backoff; show cached content as disconnected and disable mutations until synchronized.
Within the same epoch, replay retained state events after the acknowledged cursor, then resume live events.
If the cursor is too old, send a snapshot at cursor N and queue events after N while it is transferred.
Screens resume independently by generation/revision or receive fresh snapshots; event replay does not replay input.
Keep the replay ring bounded by bytes and age. It is an optimization, not the persistence mechanism.

On host restart issue a new epoch, reload plans and session records, and send a full reset.
PTYs and emulator history do not survive process/pod death; saved sessions are resume candidates,
not proof of live children. Do not redispatch tasks or redeliver uncertain input automatically.
Pending approvals expire, session-only allow rules clear, and clients refresh their approval lists.

Several authenticated clients may observe the same user host. Selection, drafts, scrolling, layout and
selected plan are per client; plans, accepted comments, reports and agent state are shared.
Use one explicit controller lease per terminal for typing and resizing; viewers can request a visible takeover.
Lease expiry/disconnect fences late input with a new lease token. Opening a tab never steals control.
Approvals are shared host transactions: the first valid decision for an unexpired ID wins, and every
client sees the resolution. A stale decision cannot approve the next request in that session.
Keep the existing hook timeout/fallback behavior and held-message rules when no client answers.

## Remote authentication and isolation

Default to loopback and a private local socket; expose remote access only through an explicit deployment.
Protect the HTTP assets/bootstrap and WebSocket upgrade, not merely the first message after connecting.
For tailnet use, terminate HTTPS at a tailnet ingress, restrict access with tailnet identity/ACLs, and map
the authorized identity to the user host. A private network address alone is not authorization.
For an auth proxy, require login and route a stable authenticated subject to its single-user host.
Trust identity headers only from that proxy; strip incoming copies and block direct access to the host port.

Use a Secure, HttpOnly, same-site session cookie; validate the exact Origin on every WebSocket upgrade,
including loopback deployments. Protect bootstrap/mutating HTTP endpoints against CSRF.
Do not put bearer tokens in URLs, browser storage, source bundles, terminal streams or logs.
For local desktop/bootstrap use, exchange a short-lived, one-use pairing secret for the session cookie.
Revalidate session expiry/revocation on active connections and disconnect revoked clients.
Gate every command by authenticated user and role; read-only viewers cannot type, approve or run git.
Resolve worktree IDs on the host and reject traversal/symlink escapes in file requests.
Terminal contents, diffs, prompts and reports are private data; use no-store responses and redact logs.
The Unix socket stays private to the host user with a mode-0700 parent and mode-0600 socket.
Agent hooks and MCP use that socket inside the user's environment; do not publish it through ingress.

## Container state and credentials

Run one non-root host per user/pod with a stable UID and no display server, GPU or Docker socket.
Persist a private user home and repositories/worktrees on user-scoped volumes, at stable absolute paths
so git worktree metadata and recorded cwd values remain valid across pod replacement.
Set `IMACS_HOME=/data/imacs`, `IMACS_CONFIG=/data/config/imacs/config.toml` and
`IMACS_SOCKET=/run/imacs/imacs.sock` explicitly. The socket and PID/lease files are ephemeral.
Persist `sessions.json`, `plans/`, `briefs/`, saved workspaces and the operation journal with restrictive modes.
Use atomic replacement for records and backups; version on-disk changes and test restoration before rollout.
Enforce one writer per volume with a host lock and deployment fencing; a rolling update must not start
two hosts against the same worktrees. Retain project files and uncommitted changes on shutdown.

Provision agent CLI credentials and git/SSH credentials per user through secret mounts or a private,
writable credential volume when token refresh requires it. They never belong in images or client state.
Headless authentication needs an explicit provider login/device flow or supplied credential; do not rely
on a desktop keychain being present. Scope tokens, rotate them and keep secret backups separately protected.
Do not mount a developer's entire home or another user's credentials into a pod.
Install hook settings for the container's binary/socket paths; preserve agent routing isolation at launch.
On termination stop accepting mutations, flush durable state, stop children with a bounded grace period,
then exit. Readiness reports successful state load/socket binding; liveness is independent of attached clients.
Bound scrollback, disk/log growth, child processes and CPU/memory; alert before storage exhaustion.

## Migration: each phase ships

1. **Browser-safe boundaries.** Separate native dependencies from shared data/layout logic while the
   existing desktop still owns its runtime. Ship unchanged desktop behavior and a wasm compilation check.
   Gate: core unit tests, native regression suite and wasm target build; no browser feature is claimed yet.
2. **Runtime without painting.** Extract host ownership behind an in-process command/event boundary;
   the desktop remains the only client. Replace repaint-driven timers with host scheduling.
   Gate: with no frames drawn, hooks reply, approvals expire, held messages release, waits wake,
   agents exit and plans persist. Keep existing UI regression checks against an isolated instance.
3. **Separate native host.** Run that runtime as a headless process; the native client uses the versioned
   protocol over loopback WebSocket with authenticated pairing. Local hooks, ctl and MCP still work.
   Gate: closing/reopening the native client preserves child PIDs, terminal state and plan progress;
   malformed/version-mismatched messages, duplicate commands, crash recovery and slow clients are tested.
4. **Browser client.** Ship same-origin wasm32/WebGPU assets against that same host, retaining the native
   client. Include cell rendering, input, diffs, editor terminals, plans, approvals and explicit control leases.
   Gate: browser/native parity for wide/combining cells, colors, wrapping, cursor, resize, alternate screen,
   scrollback/copy, comments and approval races; two clients, dropped deltas and reconnect restore state.
   Optional microphone support is capability-gated and does not prevent text steering.
5. **Remote single-user deployment.** Ship tailnet/auth-proxy ingress and a headless container profile.
   Gate: unauthenticated upgrades, wrong origins, spoofed proxy headers, cross-user paths and revoked
   sessions fail; authorized reconnect works; pod replacement restores volumes without claiming PTY survival.
   Exercise token refresh, backup restore, termination grace and single-writer fencing before Kubernetes rollout.

Each gate needs regression tests with the implementation phase; this document adds no runtime behavior.
Use an isolated `IMACS_HOME`, `IMACS_SOCKET` and `IMACS_CONFIG` for UI checks, verify the process environment
with `ps eww <pid>`, and never run migration experiments against the user's running IMACS instance.
