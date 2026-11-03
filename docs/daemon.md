# A native host with a browser client

This is a migration design, not an implemented daemon or wire API.
The destination is one native host per user, with native and browser egui clients.
The browser compiles for wasm32 and renders with WebGPU; the host needs no display or GPU.
Closing a window or losing a connection must leave agents, editors, hooks and plans running.
The desktop application remains usable at every phase below.

## Ownership and current source map

Paths in the next two lists are relative to `crates/atomics/src/`.
These are existing files and types; the split describes responsibilities to extract from them.

- `app.rs` and `app/*.rs`: move the runtime parts of `App` and `Session` to the host: agent launch and resume,
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
  Keep the restricted `atomics mcp --agent` report surface distinct from the chief's control tools.
- `setup.rs` and `doctor.rs`: native program discovery, version probing, hook installation and diagnostics.
  `main.rs`: separate native CLI dispatch and host startup from window/browser startup.

The client keeps these responsibilities:

- `app.rs` and `app/*.rs`: the UI portion of `App`, including tabs, dialogs, selection, chief docking, focus,
  steering drafts, board drawing and layout. Runtime actions become explicit commands.
- `terminal.rs`: keyboard/pointer handling, selection and local clipboard use. Painting and `Metrics` are the
  shared painter's (`atomics_ui::terminal`, see Terminal painter), which already draws from a cell copy; the client
  renders a cell cache instead of locking an emulator or holding a PTY sender.
- `diff_view.rs`: rendering, `DiffState`, `ExplorerState`, `DiffAction`, scrolling and comment drafts.
  Dirty scheduling moves to the host; accepted comments are shared host state for MCP and review delivery.
- `theme.rs`: shortcuts and the desktop's font lookup (`SystemFonts`, fontique) remain client code; `Palette`
  and egui styling are in `atomics-ui` (`theme`). The host keeps the resolved `Theme`, free of egui.
- `voice.rs`: native microphone capture through `Recorder` stays on the native client; the browser
  uses its microphone permission flow. Audio upload and transcription are optional negotiated features.

Paths below are relative to `crates/atomics-core/src/`, which is UI-free and builds for wasm32 with `--no-default-features`.
The default `native` feature enables filesystem, process and other native helpers.

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
Start with JSON text messages and the `atomics.v1` WebSocket subprotocol, over WSS for remote use.
The following lowercase names are proposed wire tags, not existing Rust types or commands.
Each envelope carries `kind`, `request_id` when correlated, and a negotiated major/minor version.
Host messages carry a boot `epoch`; IDs and revisions are opaque strings, including sequence numbers,
so browser number precision cannot corrupt cursors. Paths are host paths, never browser filesystem paths.

| Direction | Tags | Payload and meaning |
| --- | --- | --- |
| Client → host | `hello` | Supported versions, capabilities, client ID, prior epoch and last applied event cursor. |
| Host → client | `welcome` | Chosen version, capabilities, limits, epoch, client role, what it may change (`may`) and resume/reset decision. |
| Client → host | `subscribe`, `unsubscribe` | Session screens, worktree diffs/files and plans of interest; visible/focused hints. |
| Host → client | `state_snapshot` | Sessions, projects, plan summaries and pending approvals at a single event cursor. |
| Host → client | `skipped` | An event cursor the client's `diff`/`plan` topics left out, so cursors stay consecutive. |
| Host → client | `session_upsert`, `session_removed` | Stable session ID, agent, title, cwd, worktree, status, chief flag and task link. |
| Client → host | `session_command` | Start, close, steer or open an editor; return `command_result` with delivery state. |
| Client → host | `lease_acquire`, `lease_renew`, `lease_release` | Session ID, takeover or the lease's fencing token. |
| Host → client | `lease_state` | Session ID, holder, and for the holder only its token, next input sequence and time-to-live. |
| Client → host | `terminal_input`, `terminal_resize` | Session ID, control lease, input sequence or rows/columns/cell pixels. |
| Host → client | `screen_snapshot`, `screen_delta` | Session ID, screen generation, base/new revision, cells, cursor and modes. |
| Client → host | `history_get` | Session ID and bounded scrollback range by stable line ID. |
| Host → client | `history_chunk` | Retained lines or an explicit indication that the requested history was evicted. |
| Host → client | `diff_summary`, `diff_file`, `file_content` | Worktree ID, revision, base commit, paths, hunks, counts and highlight spans. |
| Client → host | `diff_get`, `file_get`, `review_command` | Bounded content request, or add/remove/send comments against a diff revision. |
| Host → client | `plan_upsert`, `plan_removed`, `task_report` | Explicit plan/task IDs, revisions, approval state, integration branch, report, and the key of the ticket a plan or task follows (`ticket`, optional). |
| Client → host | `plan_command` | Set, approve, dispatch, update, integrate or archive a specified plan/task. |
| Host → client | `approval_pending`, `approval_resolved` | Approval ID, session, request details, deadline, revision and final disposition. |
| Client → host | `approval_decide` | Approval ID (bound to that one request), decision and optional message. |
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

## Protocol server (in the desktop app)

Stream H1 of `docs/decisions/2026-11-02-embedded-atomics.md`. The desktop can serve `atomics.v1` to clients on
the same machine from a thread inside the running app (`crates/atomics/src/protocol/`). **Off by default**:
without `[remote] enabled = true` there is no thread, no socket and no file.

```toml
[remote]
enabled = true
port = 0                 # 0 picks a free port
allowed_origins = []     # exact Origin values; a native client sends none
```

- **Where it listens.** `127.0.0.1` only. On start the app writes `protocol.json` in its state directory (mode 0600):
  `{url, token, pid, pid_start, started_at}`. The token is new on every start and is never logged; `pid_start` is the
  process start time (clock ticks since boot, from `/proc`, on Linux) and `started_at` is Unix seconds. On exit the app
  removes the file only if it still holds its token, since another app on the same state directory may have replaced
  it. A client reads it with `atomics_core::advert::read`, which accepts it only while that pid is alive and, where
  `pid_start` is known, still the same process; a file a crashed app left (or whose pid was recycled) is removed and
  reported as stale. The headless host removes one an earlier run left behind as it starts, so with `[remote]` off no
  stale advert remains.
- **Upgrade.** Refused with 403/401/400 unless the `Host` header is the loopback address and port, any `Origin` is
  listed, `Authorization: Bearer <token>` matches (constant-time) and the client offers the `atomics.v1` subprotocol.
  At most four authenticated clients at once (503 beyond that).
- **Before authentication.** A connection takes a place in a separate pre-auth pool (4) and claims a client place only
  once its token is accepted, so peers that never authenticate cannot use up the real clients' places; past the pool,
  connections are closed unread. The whole upgrade must finish within 5 s of accept, and every read is cut off at that
  deadline, so a peer that trickles bytes is dropped like one that sends none. A local process that keeps the pool full
  can still delay *new* connections; connected clients are unaffected, and the token is still required to get in.
- **No locks across I/O.** The runtime thread publishes `Arc` projections (and the accepted diff content) and the
  protocol thread only reads them; it never takes the `State` mutex. Every runtime pass rebuilds the projections,
  but a collection's revision moves only when its content does, so a pass with nothing new sends no event.
  `hub.rs` turns published changes into numbered
  events and keeps a replay ring (200 events, 8 MiB, 5 minutes); its mutex guards memory only. Every socket has its own
  thread, so a socket write blocks nothing but that client. That thread waits on its socket and a wake pipe, and
  queueing anything for the client wakes it, so frames are sent as soon as they are queued.
- **Slow readers.** Each client has a bounded queue (256 frames, 4 MiB). On overflow the backlog is dropped and the
  client's thread sends a current `state_snapshot` instead (a resync). A client whose socket stays unwritable for the
  write timeout (5 s) is disconnected. Neither case reaches the runtime thread or other clients.
- **Reconnect.** `hello` carries the prior epoch and cursor. The host replies `welcome` with `replay` (events after the
  cursor follow) or `reset` with a reason (`new_client`, `epoch_changed`, `cursor_too_old`, `cursor_ahead`) and a
  snapshot. A client can also send `resync` at any time. `atomics_core::mirror::Mirror` is the reference client model.
- **Bad input.** Malformed JSON, unknown kinds, a wrong major version, an oversize frame (over 256 KiB, refused before
  it is buffered) and binary frames get a typed `error` frame; oversize frames and a ninth error end the connection.
  An error `code` or lease `reason` a peer does not know (a newer minor version's) decodes as `unknown`.
  A first message other than `hello` ends it, and so does a `hello` the host cannot attach (the error carries the
  reason's own code). Idle clients are pinged every 25 s and dropped after 60 s of silence.
- **Large boards.** A `state_snapshot` that does not fit in a frame is sent in parts, the same way a large
  `screen_snapshot` is: each part has `part {index, count}`, the same `cursor` and `revisions`, and a slice of the
  collections (sessions, then plans, approvals, diffs). `Mirror` collects the parts and replaces the board only when it
  holds all of them; a client must not show a partial board. The parts are queued together, so live events follow the
  last one. Only a single session, plan, approval or diff summary over the frame limit still fails (`oversize`).
- **Large diffs.** `diff_get` takes an optional `offset` (diff lines to skip, counted across the file's hunks).
  `diff_file` holds as many lines as fit; when lines remain it has `truncated: true` and `next_offset`, which the client
  sends back as `offset`. A hunk cut at a page boundary continues on the next page under the same header, its start
  lines those of its first line, so a client appends the pages' hunks in order. Pass the `revision` on every page: a
  change in between answers `stale_revision`. An `offset` past the end is `not_found`, and a single line over the
  frame limit is refused (`oversize`).
- **Topics.** `diff` and `plan` topics narrow the board's live updates, each kind on its own: a client that has
  subscribed to no `diff` topic gets every worktree's `diff_summary`, and once it has subscribed to any it gets only
  those worktrees'. Likewise `plan` topics for `plan_upsert`, `plan_removed` and `task_report`. Sessions and approvals
  are never narrowed, and snapshots always carry everything. An event left out is sent as `skipped {cursor}` so cursors
  stay consecutive (`Mirror` only advances), and the client's copy of that diff or plan goes stale. Widening the set
  (adding a topic to a narrowed kind, or removing its last one) means the client missed events, so the host answers
  with a fresh `state_snapshot`. A client that subscribes to nothing gets everything, as before topics existed.
- **Not yet.** `file_get` (answers `not_found`). A removed diff is sent as a `diff_summary` with no files and no base
  (a tombstone). An approval that leaves the board says how it ended in `approval_resolved` (`allowed`,
  `allowed_always`, `denied`, `expired`), or `withdrawn` when it was not answered through ATOMICS (the hook gave up,
  the agent moved on, someone answered in the terminal).

### Terminal screens

Stream H2. A client that negotiated the `terminals` capability subscribes to `{"topic": "screen", "session", "focused"}`
and gets a `screen_snapshot`, then `screen_delta`s. A `subscribe` is checked whole first (the capability, every
screen's terminal, the subscription limit of 256 topics of every kind together), so a refused one subscribes to nothing. The types and the reference client (`Screen`, `ScreenMirror`) are in
`atomics_core::screen`; the host side is `protocol/screens.rs` and `terminal/screen.rs`.

- **Who copies.** The runtime thread only publishes which terminals exist (`Terminals`), and only while the server runs;
  it does so before the board, so a client that sees a session can subscribe to its screen at once. A server thread of
  its own (`atomics-protocol-screens`) copies a screen only while a client subscribes to it, only when the emulator
  reports a change, and at most every 100 ms (focused) or 200 ms (visible). The copy is taken under
  the emulator's fair lock from the live screen, never the desktop's scrolled view; comparing and encoding happen after
  the lock is released. The board keeps flowing while an emulator is locked, and nothing takes the `State` lock.
- **Cells.** A frame lists the styles it uses once; each line is runs of cells sharing a style. A run is plain text (one
  column per `char`) or a single cell spanning `w` columns: a wide character, or a character with its combining marks.
  Colours are palette slots (0-255, 256 foreground, 257 background, 258 cursor; 259-268 are the dim and bright variants
  the client derives, as the desktop does) or `"#rrggbb"`. Snapshots carry the palette (the theme with the program's
  OSC colour changes), title, cursor shape and blink, and the modes a client needs to encode input: application cursor
  and keypad, bracketed paste, mouse modes and encodings, focus reporting, alternate scroll and kitty keyboard flags.
  Soft wraps are kept per line; trailing blanks and blank rows are left out. A copy keeps at most 1 KiB of the title
  (cut at a character boundary) and 16 combining marks per cell, whatever the program writes.
- **Revisions.** The first copy of a generation is a snapshot at revision 1. Each later change is one delta naming its
  generation, base and revision: `scroll` (lines that moved into history) first, then the changed lines, cursor, modes,
  title or palette. A resize, an alternate-screen switch, a replaced terminal, or output that cannot be followed exactly
  starts a new generation with a snapshot. Changed rows come from comparing against the previous copy, not from
  alacritty's damage, which is relative to the desktop's scrolled view and has a single reader.
- **Line IDs.** The top screen line is `first_line`; history lines have the IDs below it and `history` says how many are
  retained. Scrolling is followed exactly by finding the old top line's storage in history (alacritty rotates rows
  rather than copying them), checked against the newest history line's content when storage may have been reused. When
  that cannot be proven (more output than the history holds between two copies, a reset) the host starts a new
  generation rather than guess. `history_get {session, generation, before, count <= 1000}` is answered by a
  `history_chunk` of the retained lines (the newest, if one frame cannot hold them all), with `evicted` set when the
  request reached past the oldest retained line. Another generation is `stale_revision`; a screen not subscribed to is
  `not_found`.
- **Slow and lossy clients.** Each client has a slot per screen, bounded at 32 deltas and 2 MiB of deltas across its
  screens. A slot that overflows, or a delta that does not follow what the client was sent, is replaced by the current
  snapshot, which is encoded once per revision and shared by every client. Board events are always sent before screen
  frames. A client whose mirror reports a gap sends `resync {"screens": [session]}` and gets a snapshot. A socket that
  stays unwritable for the write timeout still disconnects the client; the emulator never waits for one.
- **Large screens.** A snapshot over the frame limit is split by rows into self-contained parts (`part {index, count}`),
  applied when the last part arrives; a delta over the limit becomes a snapshot. A screen whose single line still does
  not fit is not sent: the client gets one `oversize` error naming the session, its other screens keep streaming, and
  the screen's snapshot is sent at its first change that fits. Snapshots are encoded outside the client's queue lock,
  so the threads that queue for it never wait for an encoding.
- **Remote off.** No `Terminals`, no screens thread, nothing copied: the runtime thread skips publishing. The only
  change on the desktop's path is a counter the listener bumps on a title change and a theme change bumps.
- **Not yet.** Editor terminals and OSC 8 hyperlinks. Input and leases are H3, below.

Desktop terminal frame times (2026-11-02, `terminal::frame_bench`, ignored by default: 600 frames of
`TerminalView::show` plus tessellation for a 164x50 pane while a shell writes coloured lines as fast as it can; test
profile, shared 18-core machine). The base is the code before this change, with the same bench added.

| Build | Copies | mean | p50 | p95 | p99 |
| --- | --- | --- | --- | --- | --- |
| Base, no remote | none | 1.89 ms | 1.66 ms | 2.82 ms | 6.99 ms |
| This change, remote off | none | 1.70 ms | 1.52 ms | 2.35 ms | 4.31 ms |
| This change, one client, focused | every 100 ms | 1.70 ms | 1.59 ms | 2.37 ms | 3.50 ms |
| This change, stress | every 10 ms | 1.63 ms | 1.49 ms | 2.65 ms | 3.37 ms |

The differences are within run-to-run noise. A copy held the emulator lock for 1.3-1.5 ms on average in this unoptimised
build, waiting for the busy PTY thread included.

The M1 demo is a native client: `cargo run -p atomics --example loopback_client` prints the board and the first diff
file of a running app (`--follow` keeps printing); `--screen SESSION` prints that session's terminal instead, and
`--screen SESSION --type 'ls\r'` takes its lease, types, lets go and prints the screen until it settles.
The decoder has a fuzz target: `cd crates/atomics-core && cargo +nightly fuzz run envelope_decode`; a seeded mutation
test in `wire/tests.rs` runs in the normal suite.

### Terminal input and leases

Stream H3. A client that negotiated `input` (with `terminals`) may type into a screen it subscribes to, under that
terminal's controller lease. The types and the reference client (`Typist`) are in `atomics_core::lease`; the host
side is `protocol/lease.rs` (one `Lease` per terminal, kept with the terminal's runtime state) and
`protocol/session.rs`.

- **One holder, on request.** Subscribing never takes control. `lease_acquire {session}` takes a free lease; while
  another client holds it, it fails with `lease_held` unless `takeover` is set, an explicit user action that ends
  the other client's lease. Typing needs the screen: `not_found` until the client subscribes, and unsubscribing ends
  the lease. A view-only client (`Role::Viewer`) never gets one. Loopback clients hold the bearer token, so they are
  controllers.
- **Everyone sees who types.** Every client that negotiated `input` and follows the screen gets a `lease_state`
  whenever the lease changes hands or the holder's size comes or goes: `holder` (its `client_id`), `resized`, and
  `reason` (`granted`, `taken_over`, `released`, `expired`, `disconnected` or `desktop`). Only the holder is told the
  fencing token (`lease`), the `next` input number and `ttl_ms`. Each lease request is answered with a `lease_state`.
  When a followed terminal goes away (its session ended), its followers get a `lease_state` with no holder and
  `reason: disconnected`. `input` is offered only together with `terminals`.
- **Fencing.** Each grant takes a fencing token from one counter for the whole process, so tokens only go up and
  never come back. A lease belongs to the connection that took it. It ends when it is released or taken over, after
  30 s without input, a resize or a `lease_renew`, when its connection ends, or when someone types on the desktop.
  Input, a resize, a renew or a release naming an ended lease gets `fenced` and is not applied. `fenced`,
  `lease_held` and `slow_client` never count towards disconnecting a client: one that races another, or types faster
  than a program reads, meets them as a matter of course. Nor does `not_found` for something that went away meanwhile:
  the terminal of a screen the client follows or subscribes to (the host remembers the last 64 that closed), or a
  diff file it was told of, once the diff has moved on from the revision `diff_get` names or its worktree's diff
  went away. A session that never had a terminal, or a file missing from the revision named, counts.
- **Order, and no replay.** `seq` starts at 1 in each lease and must be the next one; a repeat or a gap gets
  `out_of_order` and is not applied. The client's own thread writes admitted input to the PTY under the lease's lock,
  so input admitted under an old lease can never land after a takeover. Nothing is replayed across a disconnect: the
  replay ring holds no input, the lease ends with the connection, and a new connection cannot use the old token.
  `Typist::disconnected` forgets the lease, so a client sends nothing it typed before or during the outage.
- **Input.** `data` is UTF-8 text the client encoded from the screen's published modes, at most 64 KiB per frame;
  more is refused as `oversize`. `Typist` splits longer pastes at character boundaries so that each piece is at most
  64 KiB once JSON-escaped: a control character takes six bytes in a frame, so 64 KiB of raw text could outgrow the
  256 KiB frame limit. 8-bit meta bytes cannot be sent. Remote input does not move the desktop's scrolled view.
- **A program that does not read.** Admitted input the PTY has not taken yet is bounded at 1 MiB per terminal
  (counted as the PTY writes it). Above that, input gets `slow_client` and is not applied; its number is not used, so
  the client sends the same input again once the program reads (its echo shows). A long paste is sent at once, so
  the pieces behind the refused one meet `out_of_order`; after a `slow_client` those do not count either.
  `Typist::refused` rewinds to the refused number and hands back the text from that piece on, to type again before
  anything newer. `Typist` keeps what it sent until it knows it was applied: a `lease_state` with `next` for the
  same lease says so, and `Typist` sends a `lease_renew` after every MiB of input to get one.
- **Resizing only when asked.** Taking the lease or typing never resizes. `terminal_resize` with a `size` ("fit to
  this screen") resizes for the holder; without one it returns to the host's own size. The holder's size lasts as
  long as its lease: however the lease ends, the terminal returns to the desktop's size (the pane's last size, or
  the size it started with on the headless host). A desktop resize made in the meantime waits until then. Resizes
  are debounced: at most one per 100 ms, and the latest one waiting is applied when that time has passed.
- **The desktop** is not a lease holder and types as it always has. Typing into a terminal pane (`TerminalView`)
  takes a remote client's lease back (`reason: desktop`), and with it any size the client set. Steering, ctl and MCP
  writes leave the lease alone.
- **Echo at once.** A keystroke is written as soon as its frame is read, and both ends set `TCP_NODELAY`. Input wakes
  the screens thread, which copies the next change at once. For 200 ms after that, it copies changes at most 5 ms
  apart and looks for them every 250 µs. Queueing the echo wakes the typing client's thread (see Protocol server), so
  it leaves at once rather than at the next 25 ms read timeout. Other viewers get the same frames; their screens
  keep the usual cadence once the window has passed.
- **Remote off.** No thread and no socket. Every terminal has a `Lease` that is never held: the desktop's resize goes
  through its uncontended lock, and its typing checks it with a single atomic load.
- **Tests.** `protocol/lease.rs` covers every rule in isolation, with a fake clock. `protocol/input_tests.rs` checks
  them over real loopback sockets with emulators that echo what they are sent: order, fencing, takeover seen by
  everyone, expiry and renewal, no replay after a reconnect, resizing only when asked and restored when the lease
  ends, debounced bursts, the desktop taking the lease back, a terminal that closes, a program that stops reading,
  capability, subscription and role, and an echo that does not wait for a 5 s cadence. `app::host_tests` types into a real shell through the host while the `State` lock is
  held. `terminal::tests` drives `TerminalView` to show that desktop typing still reaches the PTY and takes the lease
  back. The headless gate (`tests/headless_host.rs`) types into a shell that ctl started. `protocol/link.rs` races
  wakes against the wake pipe's drain and checks that a later wake still ends a wait at once.
- **Who holds it.** `holder` is a paired device's name, the one it was paired under (so a client can say
  "typing from iPhone"); a loopback client's is the `client_id` it chose. The desktop shows it too: while a
  remote client holds a terminal's lease, its pane carries "Typing from <device>" (with "· sized for it" while the
  client's size is in force) and **Take back**, which ends the lease as typing in the pane does
  (`TerminalView::holder_pill`). The pill floats over the cells, so the pane keeps its size.
- **Encoding.** Keys, typed text, a paste and mouse reports are encoded by `atomics_ui::terminal::encode` from the
  screen's published modes, on the desktop and in the browser alike; only the browser turns Ctrl with `2`-`8` and
  some punctuation into control bytes (the module's notes say why). A program's clipboard escape (OSC 52) never
  reaches a browser's clipboard: there is no opt-in yet.

Keystroke echo on loopback (2026-11-02, `protocol::echo_bench`, ignored by default; release build, shared 18-core
machine). A real PTY runs `cat` in cooked mode, so the kernel echoes each key. A client on a loopback socket with
`TCP_NODELAY` holds the lease and types one key per frame. Each sample is timed from just before the frame is sent
until the client's `ScreenMirror` shows the echo. The server runs its production settings (100 ms focused cadence,
25 ms read tick).

| Path | p50 | p90 | p95 | p99 | max |
| --- | --- | --- | --- | --- | --- |
| Remote, typing (300 keys, 60 ms apart) | 0.55 ms | 0.61 ms | 0.67 ms | 0.72 ms | 0.78 ms |
| Remote, cold (100 keys, 400 ms apart) | 0.60 ms | 0.70 ms | 0.72 ms | 1.14 ms | 14.0 ms |
| The host's PTY alone (write, then read the emulator) | 0.07 ms | 0.10 ms | 0.13 ms | 0.38 ms | 2.97 ms |
| Desktop, in-app, frame cadence included (`2026-11-02-vm-display.md`) | 4.7 ms | | 12.3 ms | | |

The protocol adds about 0.5 ms to the PTY's own echo: under the desktop's 4.7 ms p50, and far under its 12.3 ms p95.
The single 14 ms cold sample came in the same run as the PTY's own 2.97 ms worst case, on a machine other agents
were loading.
The remote figures leave out the client's own paint; the desktop's include the app's frame cadence. A first version
that polled every 1 ms on both the screens thread and the client's thread measured 2.67 ms p50. The wake pipe and the
250 µs echo poll removed that. Debug builds measure about 1.1 ms.

### Mutations

Stream H4. A client that negotiated `mutations` sends `approval_decide`, `plan_command`, `review_command` and
`session_command`; each is answered by `command_result` echoing its `request_id`. The types and the reference
client's tracker (`Pending`) are in `atomics_core::ops`; the host side is `protocol/ops.rs` (the hand-off),
`protocol/journal.rs` and `app/mutations.rs`.

- **Who may.** `welcome` lists what the client may change in `may`: `type` (with `input`), and `approve`, `session`,
  `plan` and `review` (with `mutations`). It comes from the client's `Role`: a controller gets all of them, a viewer
  none. Loopback clients hold the bearer token, so they are controllers; a rendezvous client's role is its paired
  device's access in the allowlist, "view only" (viewer) or "may type and approve" (controller; see Rendezvous,
  "Pairing and the encrypted channel"). A mutation without the capability or the
  grant is refused `unauthorised` in a `command_result`, never forwarded, and counts towards disconnecting.
- **One at a time, on the runtime thread.** The protocol thread never runs a mutation: it hands it over a channel
  that wakes the runtime thread, which carries mutations out under the `State` lock in turn with the desktop's
  actions and ctl's commands. The desktop's own approve and steer go through the same code (`State::mutate`,
  the journal and `State::decide`), so a client and the desktop race exactly as two clients do.
- **Operations.** Every mutation carries an operation ID (`op`) the client chose. The journal records it, keyed by
  the client's `client_id` and `op` with a fingerprint of the command, before any side effect, and its outcome
  after. The same `op` again returns the known outcome (from another connection too, as after a reconnect); while
  it still runs the repeat is answered `accepted` and hears the end. The same `op` with a different command is
  `conflict`. Outcomes are `accepted` (journaled, under way: a delivery to confirm, a worktree, a merge), then
  `done` with a result, `refused` with a code, or `uncertain`.
- **Crashes.** While the server runs the journal is `protocol-ops.jsonl` in the state directory (mode 0600): one
  line when an operation begins and one when it ends. An operation that began without ending when the host stopped
  is `uncertain` from the next start on, and is never run again by itself; a codex delivery whose receipt never came
  is `uncertain` too. Each line is synced to disk (`sync_data`) before the work starts and before its outcome is
  answered, so neither the host process dying nor the machine losing power makes it run an operation again.
  Outcomes are kept 30 minutes from when the operation began (at most 4096; past that new work is
  refused `too_many` rather than an unexpired outcome forgotten). Clients are told `op_ttl_secs` (15 minutes) and
  resend an unanswered operation only while it is younger (`Pending::resend`): an older one needs the user's fresh
  intent. With `[remote]` off the journal is in memory and there is no file.
  An incomplete final line is truncated on open. If opening or writing the journal fails,
  new client mutations are refused until the host restarts (no record may follow one that failed); a failed
  reopen after compaction disables further writes. The desktop says why: in Remote devices, with a warning dot
  on the Remote indicator and the reason in its hover, and as `journal_error` in `atomics ctl '{"cmd":"remote"}'`.
  Desktop actions use fresh operation IDs, bypass the client capacity limit, and still run
  without journaling when the journal is unwritable. Outcome records are appended under the
  State lock, then synced after releasing it, before client replies are sent. The beginning
  remains synced before side effects. Compaction also syncs its replacement and directory.

- **Ownership.** Only one host per ATOMICS home owns remote access: it holds an exclusive advisory lock on
  `remote.lock` for its lifetime. A second desktop keeps running without remote access and
  reports that another ATOMICS owns it. It does not open the journal, device keys or advertisement.
  The command journal opens only after the protocol server has started successfully.
- **Approvals.** An approval's ID is minted for that one request, and `approval_decide` names it. The first valid
  decision answers it and is recorded; a later one for the same ID, from any client or the desktop, is refused
  `already_resolved` with how it ended, and every client gets `approval_resolved` with the disposition. A decision
  for an answered request never answers the session's next one; one for a request queued behind another is
  `rejected`; one for an ID absent from both pending requests and recent resolutions is `not_found`. Expiry and
  hook timeouts are as before.
- **Plans.** `plan_command` names the plan and an `action`: `set`, `approve`, `dispatch`, `update`, `integrate`,
  `archive`, as ctl's `plan_*` do. `approve`, and `set` of an existing plan, carry the plan's `digest` from the board
  (`PlanInfo.digest`, the approval scope the remote conversation's `/approve` uses, published for open plans): a plan
  whose scope changed since is `stale_revision`, so the user approves or replaces exactly what they read.
- **Reviews.** `review_command` adds, removes or sends comments on a worktree's diff. The comments waiting to be sent
  are in its `diff_summary` (`comments`), shared by every client. `add` names the diff revision it was written
  against and a file in that diff; a diff that moved on is `stale_revision`, and the client anchors the comment
  again. `remove` names the comment's fields; `send` delivers them all to the worktree's agent as ctl's does.
- **Sessions.** `session_command` steers (`steer`: the steer bar's delivery, with its `sent`, `held` and
  `queued_until_next_hook` results), starts a configured agent (`start`, in a folder, a new worktree or a scratch
  folder) or closes one (`close`). IDs are matched exactly; ctl's title and prefix matching is not used.
- **Bounds.** At most 64 operations under way per connection (`max_pending_ops`; more are `too_many`), 64 KiB of
  text in a field and 256 tasks in a `set`.
- **Tests.** `ops.rs` and `wire/tests.rs` cover the frames and bounds, `journal.rs` retries, conflicts, a crash in the
  middle and expiry, `protocol/mutation_tests.rs` the gates (view-only and capability refusals), routing and bounds
  over loopback sockets. `app::host_tests` runs the real host: two clients racing one approval get exactly one winner
  and both see how it ended; the desktop racing a client likewise; a decision for an answered request does not answer
  the next; a retried steer is delivered once; an operation an earlier host died during is `uncertain`; and a plan
  approval with an outdated digest is refused.
- **Who sent it.** The journal keys a paired device's operations by a digest of its key and its client's ID
  (`device:<digest>:<client_id>`), so two devices never share one, and records the device's name (`by`). A loopback
  client's are `client:<client_id>`, the desktop's `desktop`.
- **Not yet.** Opening an editor (its terminal is not streamed yet).

### Terminal painter

Stream C3. `atomics_ui::terminal::paint` draws an `atomics_core::screen::Screen` with egui, and the desktop paints its
terminals through it too, so a browser and the desktop draw the same cells the same way. It lives next to the
`Palette` (S5) in `atomics-ui`, which builds for wasm32; `Palette::terminal_theme` gives it the colours drawn around
and over the cells, and `Palette::dim` is the painter's own fade.

- **Desktop path.** `TerminalView::paint` copies the cells it displays (`terminal/screen.rs` `view`: the screen at
  the view's scroll offset, the theme's palette with the program's OSC colours, the cursor hidden when scrolled out
  of view) under the emulator lock, releases it, then paints. The old painter held the lock while it laid out text.
  The cells are copied again only when the terminal's revision, the scroll offset, the history length or the view's
  rows changed; the palette is refreshed every frame. The alacritty selection becomes a `Selection` in screen rows.
- **The desktop's look, kept exactly.** One monospace font (bold and italic not drawn), every underline style as one
  thin line in the text colour, no strikeout, the theme's cursor colour (not OSC 12), and a solid cursor only when
  focused. Two old quirks are kept on purpose: combining marks over an ASCII letter are not drawn (`e` + U+0301 shows
  `e`), and a wide character split from its spacer by an erase is still drawn two columns wide with nothing on the
  stray spacer (the desktop copy keeps alacritty's flags; the wire form normalises them).
- **Parity checks.** `terminal/legacy_paint.rs` keeps the old painter for tests only. `terminal::paint_tests` paints
  23 scenes (colours, attributes, TUI and alternate screen, wide and combining text, split wide characters, OSC
  colours, wrapping, five cursor shapes, selections simple, reversed, block and by word, scrollback with and without
  a selection) both ways at 1x and 2x, focused and not, in two themes, and requires equal shapes and equal
  CPU-rasterised pixels; `ATOMICS_TERMINAL_SCREENSHOTS=<dir>` saves them. Real-app screenshots of four tiles (colours,
  an alternate-screen TUI, wide and combining text, history) compared the base binary with this one: identical on
  macOS (2880x1800) and Linux QA (Xvfb, 1440x900, with an XTEST drag selection and wheel scrollback) except for the
  topbar's live load figure and one braille glyph that also differs between two runs of the base binary on macOS.

Frame times (2026-11-02, `terminal::frame_bench`: 600 frames of a 163x50 pane, mean / p50 in ms; `busy` is a shell
writing coloured lines flat out, `tui` a still full screen of styled cells and fills). Each painter runs alone in
alternating blocks, as in the app; the bench also has them take turns frame by frame, where the old painter's long
lock hold makes the PTY thread's backlog land on the next frame.

| Build | busy, old | busy, shared | tui, old | tui, shared |
| --- | --- | --- | --- | --- |
| macOS, test profile | 1.16-1.21 / 1.15-1.19 | 1.04-1.05 / 1.02-1.03 | 2.28-2.31 / 2.10-2.27 | 1.95-1.97 / 1.87-1.91 |
| macOS, release | 0.54-0.64 / 0.48-0.62 | 0.54-0.64 / 0.49-0.63 | 0.76-0.78 / 0.71-0.74 | 0.70-0.73 / 0.66-0.67 |
| macOS, release, taking turns | 0.61 / 0.53 | 0.60 / 0.51 | 0.68 / 0.67 | 0.66 / 0.65 |
| Linux QA container, test profile | 2.92-3.01 / 2.58-2.83 | 2.29-2.95 / 2.20-2.71 | 2.80-2.88 / 2.62-2.71 | 2.63-3.22 / 2.40-2.99 |

The shared machine's load moves whole blocks (the release busy blocks swap places between rounds), and the Linux
container (2 CPUs) is noisy in both directions. The changes to `attrs` also cut the remote screen copy (h2) from
about 1.05 ms to 0.4 ms in the macOS test profile.

## Headless host (`atomics host`)

Phase F of `docs/decisions/2026-11-02-embedded-atomics.md`, brought forward so the host can run in the NixOS VM.
`atomics host [-w WORKSPACE] [PROJECT]...` runs `host::Host` (the runtime thread, the local socket and, with
`[remote] enabled = true`, the protocol thread above) with no window, egui, display or GPU. Plain `atomics` is the
desktop exactly as before.

- **Service construction without egui.** `State::build(options, window)` builds the runtime model and its services;
  the runtime asks a window for repaints, focus and its size only through `host::Window`, an optional
  `host::Surface`. The desktop's `State::new` attaches its `egui::Context` as that surface and applies the theme
  to it; the headless host attaches none, so every repaint is a no-op and `ctl list` reports `window: null`.
  `Host::start`, `ipc::serve`, the governor and the usage pollers take the `Window`, and runtime methods no longer
  take an `egui::Context`. The palette is still loaded on the host for diff highlighting (moving it is slice S5).
- **No client attached.** The runtime thread answers and drops client events nobody consumes (a screenshot
  reply gets an error), so they never accumulate. `screenshot`, `resize` and `show_settings` fail with "no window";
  `restart` is refused, since a service manager restarts the host. Notices the desktop shows as toasts go to stderr.
- **One owner per home.** `instance.lock` in `ATOMICS_HOME`, by `flock`: the host holds it exclusively and records
  its pid, and every desktop holds it shared. A host refuses a home a desktop or another host is using, and a desktop
  refuses a home a host is using; desktops still start side by side (the second runs without the socket, as before).
  The kernel drops the lock with the process; the file is close-on-exec, so agents never inherit it. A desktop whose
  home cannot be locked at all (no `flock`) starts as it always has; a host refuses.
- **Service manager friendly.** Foreground, logs on stderr (start, socket, config, the protocol address but never its
  token, notices, stop). SIGTERM, SIGINT and SIGHUP (unless ignored) reach a self-pipe from a signal handler, not a
  blocked mask, so agents inherit default signal handling. On the first one it removes the socket, stops the protocol
  server (removing `protocol.json`) and the runtime thread, shuts down as the desktop does on quit (terminals, tickets,
  sandboxes, the clean-exit record) and exits 0. A second signal takes the default action. It exits 1 rather than run
  half-reachable: when the local socket cannot be served (nothing could reach the host), or when `[remote]` is enabled
  and the protocol server cannot start. `Host::protocol_error` carries that start error out (the desktop still logs it
  and carries on), the log names the configured port, and any stale `protocol.json` is removed.
- **Tests.** `crates/atomics/tests/headless_host.rs` runs the binary with an isolated home and no `DISPLAY`: a shell
  starts through ctl, a permission hook waits and gets the `allow` that ctl gives, a plan persists, a loopback
  `atomics.v1` client sees the session and the plan, a second host and the desktop are refused, SIGTERM exits 0 with
  the shell, socket and `protocol.json` gone and the token in no log, and the home can be hosted again. A second test
  checks that with remote off there is no `protocol.json` and that `atomics mcp` still reaches the host; a third occupies the
  `[remote]` port first and expects exit 1, an accurate log line, no advert or socket left, and a healthy host on that
  port once it is free. `app::host_tests` checks the refusals and the drained client
  events in process, and `instance::tests` the lock both ways.
- **Not yet.** The desktop cannot attach to a running host as a client (phase 3). Terminals keep the size they
  started with unless a lease holder asks for its own (H3), and are seen and typed into through ctl, MCP and
  `atomics.v1` (H2, H3). The container profile stays in Phase F.

## Rendezvous

Stream H5 of `docs/decisions/2026-11-02-embedded-atomics.md`, sections 5-8. The host side is built
(`crates/atomics/src/protocol/rendezvous/`): registration, passes and the tunnel (H5a), and the end-to-end Noise
channel, pairing and the device allowlist (H5b), whose browser half is `atomics_core::noise`.
The rendezvous lets a browser reach a host that cannot accept inbound connections. The host dials out to a
broker; a browser dials the same broker; the broker pairs the sockets and relays opaque frames. The protocol
above (`atomics.v1`) runs unchanged inside an end-to-end encrypted channel, so the broker is never a protocol peer.
Nothing here names a product: the broker, the profile operations and the pass format belong to the deployment.

### The host side (H5a)

```toml
[remote]
enabled = true                 # the rendezvous serves its channels through the protocol server

[remote.rendezvous]
enabled = true
url = "wss://rendezvous.example/atomics/tunnel"   # ws:// only to a loopback host
name = ""                      # how the service lists this host; empty = the machine's name
```

The ticket profile maps `rendezvous_register`, `rendezvous_challenge` and `rendezvous_pass` (and `whoami`), and
`[tickets.rendezvous]` names the service and its refusal codes (`docs/tickets.md`, "Operations"). Off unless both
`enabled`; with `[remote.rendezvous] enabled` but the protocol server off, nothing starts and the status says why.

- **Device keys.** An Ed25519 signing key and an X25519 Noise static key, made on first use and kept in the OS
  credential store as one item (`atomics-rendezvous-v1`) per ATOMICS home, named by a digest of the home's path: two
  homes on one machine are two hosts. Only public keys leave the host, as base64url without padding.
- **Registration and passes.** On start, `whoami` must be the profile's personal-agent kind with an owner; then
  `rendezvous_register` (a fresh `{nonce}` as idempotency key) gives the host id. To dial, `rendezvous_challenge`
  gives a challenge, the host signs `atomics-tunnel-pass-v1\n<host id>\n<challenge>`, and `rendezvous_pass` gives a
  single-use pass. Each dial and each renewal buys a new pass. The calls run on a thread of their own
  (`atomics-rendezvous-issuer`), on the issuer's own MCP connection with the profile's saved sign-in.
- **Dialing.** Blocking `tungstenite` on the `atomics-rendezvous` thread, TLS by rustls with the web PKI's roots,
  subprotocol `atomics.rendezvous.v1`, `Authorization: Bearer <pass>` and no `Origin`. The pass is never in a URL,
  a log, a status line or `atomics ctl`.
- **Frames.** Every binary message is `[kind u8][channel u32 BE][body]` (`atomics_core::tunnel`). Data (0) on a
  channel of 1 or more carries one Noise message (below); control (1) is an operation byte: `joined` (1), `left` (2,
  then the close code), `close` (3, host to service, 1000 or 4000-4999 only), `renew` (4, the new pass as ASCII, on
  channel 0) and `expiry` (5, the admission's end in Unix ms, on channel 0). Unknown kinds and operations from the
  service are ignored, data for a channel that never joined is dropped, and malformed frames are skipped.
- **Channels.** Each `joined` gets a thread of its own (`channel.rs`): a Noise handshake, then one `atomics.v1`
  session inside the encrypted channel, served by the protocol server's session code with its queues and limits
  (`protocol::Sessions`), with the role of the paired device (below). At most four at once; one more is closed with
  `quota` (4429), and a channel the host closes gives its place back at once, before the service's `left`. A
  channel ends with 1000, `refused` (4000) for a protocol error or a message that fails to decrypt, `idle` (4001),
  `not_paired` (4003), `no_pairing` (4004), `pairing_busy` (4005) or `slow` (4408); one the host cannot start gets
  `unavailable` (4002).
  Each channel may queue 1 MiB
  toward the service (a session waits up to 5 s for room, then ends as slow), and a channel whose session stops
  reading 1 MiB of what arrives is closed as slow. Stopping the rendezvous closes the tunnel and waits up to 2 s
  for the channels' threads, so their sessions' last-seen times are written before the host goes.
- **Renewal.** After each `expiry` the host renews at half the remaining admission, between 15 s and 150 s, so a
  skewed clock still renews in time. The renewal's MCP calls never stall the frames; a failed one is retried after
  15 s (a minute when throttled), and a revoked answer stops the host.
- **Reconnect.** Jittered exponential backoff, 1 s to 60 s, reset once a tunnel has stayed up 30 s. `unauthorised`
  (4401), `host_offline` (4404), `slow` (4408), any other code, a dropped socket, a refused upgrade, or 75 s with
  nothing from the service (it pings every 25 s): dial again with a new pass. `quota` (4429): wait 5 minutes.
  `revoked` (4403) and `replaced` (4409): stop until restart. Profile, sign-in and owner problems are retried
  every 30 s; an `unknown` host id a minute after registering registers again.
- **Status.** A "Remote" indicator in the top bar while `[remote.rendezvous]` is enabled (online in the success
  colour, waiting or stopped with a dot, connecting muted; the hover says why), and `atomics ctl '{"cmd":"remote"}'`:
  the protocol server's URL (and `journal_error` while the operation journal cannot be written, above) and the
  rendezvous's `phase`, `service`, `host`, `registered`, `host_id`, `online`, `channels` and `note`. The headless
  host logs whether it started.
- **Tests.** `protocol::rendezvous::tests` runs the host against an in-process service that admits only unused
  passes from a fake issuer, with browsers speaking `atomics_core::noise` on its channels: registration, the
  upgrade's headers, pairing (controller and view-only, a wrong psk and the link given back, a device racing the
  link and told it is busy, a used link and no pairing open), sessions of paired devices, an unknown device refused inside the channel, plaintext and
  forged channels, replays (a replayed handshake marks nothing connected and writes nothing), revoking, renewal at
  half-life, every close code, a dropped socket and a silent service, refused upgrades, the channel limit, issuer
  refusals, and a stop. The profile issuer is tested against an in-process MCP server (whoami, signatures, refusal
  codes, no secret in messages); `atomics_core::tunnel` tests the framing; `atomics_core::noise` checks both
  protocols against the standard Noise vectors and its own framing against vectors from an independent
  implementation; `rendezvous::devices` tests the pairing stages, expiry, the allowlist file and revocation.

### Pairing and the encrypted channel (H5b)

```toml
[remote.rendezvous]
pair_url = "https://rendezvous.example/atomics/pair#{pairing}"   # the service's pairing page; empty = no pairing
```

- **Pairing.** Clicking the top bar's "Remote" opens Remote devices. "Pair a device" (once the host is registered
  and `pair_url` is set) shows a QR code and the link `pair_url` with `{pairing}` replaced by
  `<v>.<host_id>.<host_noise_pubkey>.<psk>`: `v` is 1, `host_id` the service's id, and the key and a fresh psk 43
  characters of unpadded base64url. `pair_url` must be `https://` (or `http://` to this machine) and end in
  `#{pairing}`, so the psk travels only in the fragment, which browsers never send. The link works once, for five
  minutes, and only one is open. A device answers it with `Noise_IKpsk2_25519_ChaChaPoly_SHA256`; one at a time
  (another gets `pairing_busy`, 4005, and a browser asks again while its link may be valid). A link that was
  never offered, was used, expired or was withdrawn gets `no_pairing` (4004): asking again cannot help. An older
  host says `no_pairing` for both; an older browser reads `pairing_busy` as a dropped channel and dials again,
  until the link is used or expires and the host says `no_pairing`. The handshake proves nothing
  about the psk, so the link goes back as soon as the device's first message fails to decrypt or the device leaves,
  and at the latest after 30 s. Its first message (`PairRequest`, its name) proves the psk; then the desktop asks
  "Pair “name”?" beside the six-digit code from the handshake hash, which the device shows too. "Codes match: may type and approve" or "Codes match: view only" pairs
  it; "Codes differ: refuse" does not. Either answer, the device leaving, or expiry uses the link up. The device gets
  `PairAnswer` (`paired`, `may`); a paired device's channel goes on as its session, a refused one is closed with
  `not_paired`.
- **Sessions.** A paired device dials `Noise_IK_25519_ChaChaPoly_SHA256` with its stored static key. The host
  answers and waits up to 10 s for the device's first transport message (its `hello`). Only once that decrypts does
  it look the key up in the allowlist, mark the device connected and write its last-seen time, so a replayed
  handshake changes nothing. A key that is not there gets the host's refusal sealed into the channel, an
  `atomics.v1` `error` with code `not_paired` (`wire::not_paired`), and then the close `not_paired` (4003). A
  browser forgets its key on that sealed frame only: the close code alone comes through the broker
  unauthenticated. Its access is its role: view only is a viewer (`may` empty), may type and approve a
  controller. Its name is its lease holder label and its name in the operation journal.
- **Frames.** The first data body is `[kind u8: 1 pair, 2 session][Noise message 1]`, exactly 97 bytes; the
  host's answer is message 2 (48 bytes); before it, the host reads that one bounded frame and parses nothing. Then
  each body is one Noise transport message (at most 65535 bytes) whose plaintext is `[flags u8][part of a frame]`:
  flag 1 says the `atomics.v1` frame goes on in the next message (so a 256 KiB frame takes five), flag 2 that the
  sender rekeys after this message and the reader after reading it. Each direction rekeys every 1 GiB, and the host
  every hour too. Noise's counter nonces make a replayed, reordered, cut or forged message fail, which ends the
  channel with `refused`. The prologue is `atomics-noise-v1`, `atomics.v1`, the protocol major and the host id, one
  per line.
- **The allowlist** is `devices.json` in the ATOMICS home (0600, written atomically): each device's public key,
  name, access, and when it paired and was last seen. Remote devices lists them, with "connected" while a session
  is open; "Revoke…" asks once, removes the device and ends its sessions at once with the sealed refusal and
  `not_paired`. A host the service revoked (4403) stops, and withdraws an open pairing link. `atomics ctl '{"cmd":"remote"}'` adds `paired`:
  the pairing's stage and the devices (no keys, links or secrets).
- **The browser's half** is `atomics_core::noise` (wasm32-safe, `getrandom`'s `wasm_js` backend): `Pairing::read`,
  `Initiator::pair` or `Initiator::connect`, `finish`, then `Channel::seal` and `Channel::open`. The device's static
  key is the browser's to keep; the web client (below) keeps it sealed in IndexedDB.

**Roles.** Host (this program, off by default), broker (untrusted relay of bytes), client (browser).

**Frames on the broker socket.** Binary WebSocket messages with a small plaintext header `{kind, channel}`:
`data` carries one Noise transport message; `ctrl` carries broker hints. The broker assigns each browser
connection a `channel` and tells the host `joined(channel)` and `left(channel)`. Those hints are untrusted:
a forged `joined` produces a failed handshake and nothing else. One Noise session per channel; a channel
is closed on any authentication failure.

**Host identity and admission.**
1. The host keeps an Ed25519 signing key and an X25519 Noise static key in the OS credential store, never in plaintext.
2. *Register* (a profile operation over the user's existing authenticated connection) sends both public keys, a
   display name and the protocol major. The deployment binds the `host_id` to the human the connection acts for,
   and refuses a connection with no human owner.
3. *Pass*: to dial, the host asks for a pass with its `host_id` and a signature over a fresh server nonce. A pass is
   single use and lives about five minutes. It goes in the upgrade's `Authorization` header, never the URL.
4. A browser obtains a single-use connect ticket (about 30 seconds, bound to user, host and session) from the
   deployment's web tier and presents it in `Sec-WebSocket-Protocol`. The broker never reads a session cookie;
   it verifies the exact `Origin` and re-checks the session behind each channel about every minute.
5. Revoking the connection, deleting the host or rotating its device key stops new passes; the broker closes the
   host's sockets within a pass lifetime.

**Pairing.** The host shows a QR code and a link whose *fragment* is `<v>.<host_id>.<host_noise_pubkey>.<psk>`;
fragments are never sent to a server. The `psk` is single use, expires in five minutes, and only one pairing is
open at a time. Both screens then show a six-digit code derived from the handshake hash; the user confirms they
match on the host before the device is added to the allowlist. A device may be added view-only.

**Handshake.** First pairing: `Noise_IKpsk2_25519_ChaChaPoly_SHA256`, the browser as initiator. Known devices:
`Noise_IK_25519_ChaChaPoly_SHA256`, with the browser key looked up in the host allowlist. The prologue binds
`atomics.v1`, the protocol major and `host_id`. Before authentication the host parses only a bounded Noise message,
never JSON. Rekey every 1 GiB or hour. After the handshake (and, when pairing, the host's `PairAnswer`) the first
`atomics.v1` frame is `hello` (see Client protocol), and `welcome` decides replay or reset from the epoch and cursor
as in Reconnect.

**Limits (broker enforces bytes and time; the host enforces meaning).**
Frame at most 256 KiB (`wire::MAX_FRAME_BYTES`), a per-channel send buffer of 1 MiB closed after 10 s full,
sustained 2 MiB/s per host with an 8 MiB burst, ping every 25 s with a 60 s idle close, and per-user quotas on hosts
and channels. The broker cannot read frames, so it cannot supersede screen deltas: the host applies the queue
policy from Terminal streaming before encrypting, and a closed slow channel reconnects to a fresh snapshot.
Close codes are typed: `unauthorised`, `revoked`, `slow`, `replaced`, `quota`; the host's own, on a channel, are
`refused`, `idle`, `unavailable`, `not_paired`, `no_pairing` and `pairing_busy`.

**Reconnect.** Jittered backoff. A dropped broker socket detaches channels without touching sessions. The host
redials with a fresh pass; each browser reconnects, handshakes again and sends `hello` with its prior epoch and
cursor. Handshake state is never resumed across sockets.

**What the broker sees.** The user, host ID and display name, online state and times, source addresses, channel
numbers, frame sizes and timing, and public keys. It cannot read terminal contents, diffs, paths, plans, approvals,
or which device holds a lease, and cannot forge, replay or reorder frames; it can drop, delay or cut them.

**Out of scope here.** Serving the client code (its integrity is the deployment's trust boundary), a fallback
pairing without an authenticated connection, a browser key that script on the client's origin cannot use (the web
client seals it at rest, below), and routing between broker replicas.

### The browser client and its embedding (C5)

`crates/atomics-web` is the egui UI in a browser: eframe's web runner with glow (WebGL2), atomics-ui's draw
functions over a remote `Backend` built on `atomics_core::mirror`, and the Noise channel above. Everything but
the browser glue (`src/web.rs`) builds natively, and the tests run it against an in-process host.

**The bundle.** `scripts/build-web.sh` runs trunk (`crates/atomics-web/Trunk.toml`, profile `web`) into
`crates/atomics-web/dist`: `index.html`, `boot.js`, the content-hashed `atomics-web-<hash>.js`, `_bg.wasm` and
`style-<hash>.css`, `snippets/` (the device store, and the terminal's input overlay and key bar) and
`manifest.json` (`atomics_version`, `protocol_major`, `protocol_minor_max`, `wasm_sha256`, `built_from`, and
every file's SHA-256). It then packs
`target/web/atomics-web-<version>.tar.gz` with a `.sha256` beside it; the `Web bundle` workflow publishes both
on tags. Every URL in the bundle is relative: serve the folder as it is, one per protocol major. There is no
inline script or style, so the client's origin can send `Content-Security-Policy: default-src 'none';
script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; connect-src 'self' wss://<broker host>;
frame-ancestors <parent origin>` (`connect-src 'self'` is for the wasm's fetch), `index.html` with
`Cache-Control: no-store` and the hashed files immutable.

**The parent origin.** The hosting product serves `config.js` beside `index.html` (it is not part of the bundle):

```js
window.ATOMICS_PARENT_ORIGIN = 'https://app.example';
```

That exact origin, as `MessageEvent.origin` writes it (`https`, or `http` to loopback; lower case; no default port,
path or trailing slash), is the only one the frame listens to, and only from the window that framed it. A message
from any other origin or window is dropped before its data is read. Without the setting, or opened outside a
frame, the client says to open it from its hosting page and does nothing else.

**The iframe.** The terminal's Copy writes the clipboard and the key bar's Paste reads it through the
asynchronous clipboard API, which a cross-origin frame may use only where its `<iframe>` allows it:
`allow="clipboard-write; clipboard-read"`. Without `clipboard-write`, Copy falls back to the browser's copy
command on a scratch field, which some browsers refuse in a frame; without `clipboard-read`, Paste says the
clipboard could not be read, and pasting with the keyboard still works.

**Frame to parent.** `postMessage({type, v: 1}, <parent origin>)`, where `type` is one of:

| `type` | When |
| --- | --- |
| `atomics.ready` | Once, as soon as the frame listens: send the boot |
| `atomics.booted` | The boot was taken |
| `atomics.ticket` | The frame needs a fresh connect ticket, to reconnect. Asked again every 15 s until one comes |
| `atomics.paired` | The host accepted this device: the codes matched |

**Parent to frame.** Both messages carry `v: 1`; any other version or `type` is ignored.

`atomics.embed.boot`, once per page load (to boot again, load the frame again):

| Field | Required | Rule |
| --- | --- | --- |
| `host_id` | yes | The rendezvous service's id for the host: 1-64 of `A-Z a-z 0-9 -` |
| `ticket` | yes | The single-use connect ticket: 1-4096 RFC 7230 token characters |
| `broker` | yes | The broker's absolute URL: `wss://`, or `ws://` to loopback; no user part or fragment |
| `display_name` | no | The signed-in person, as the page names them |
| `theme` | no | The page's theme. A built-in ATOMICS theme of that name is used, else the first built-in of `scheme` |
| `scheme` | no | `dark` (the default) or `light` |
| `locale` | no | The page's locale |
| `view` | no | Where to open: `board` (the default), `agents`, `approvals`, `diffs`, `terminal` (an agent's) or `chief`; on a wide screen `diffs` opens the right panel on the diff and `chief` shows the chief; `plans`, or an unknown name, opens the board |
| `ticket_key` | no | A ticket's key (`docs/tickets.md`; its id where it has no key): opens the plan that follows it, else one with a task for it (open plans first, keys compared without regard to ASCII case), or the board when none does. Wins over `view`. A text like the others; empty is ignored. (`ticket` is the connect ticket.) |
| `mode` | no | Absent, or `pair` on a pairing page |
| `pair` | with `mode: pair` | `{v: 1, host_noise_pubkey, psk, age_ms}`: the pairing link's fields, each key 43 characters of unpadded base64url, and optionally `age_ms`, how many milliseconds before this boot the page got the link (opened, scanned or pasted). The link's five minutes run from then; without it, or with anything but a number of at least 0, from the boot |

A boot with a malformed required field, an unknown `mode` or a malformed `pair` is ignored. Texts are trimmed,
stripped of control characters and cut at 200 characters.

`atomics.embed.ticket`, answering `atomics.ticket`: `{type, v: 1, host_id, ticket}`, for the booted host. A ticket
the frame did not ask for is dropped.

**Dialing.** `new WebSocket(broker, ['atomics.rendezvous.v1', 'atomics.ticket.<ticket>'])`, binary messages, each a
bare tunnel body; the broker must answer `atomics.rendezvous.v1` or the frame closes the socket. Each ticket is
used once. Then the Noise channel (H5b): on a pairing boot the frame makes a fresh device key, pairs with
`Noise_IKpsk2`, names itself from its user agent ("Safari on iPhone"), shows the six-digit code until the host
answers, and on `paired` keeps the key and posts `atomics.paired`; the channel goes on as the session. When the
boot names a host this browser already holds another host key for, the frame first asks "Pair this browser
again?": pairing replaces that key once the host accepts the device, and "Keep the current pairing" connects with
the key it holds. The same host key needs no question, since only that host can answer the link. Otherwise it
loads the key it holds for `host_id` and dials `Noise_IK`; with none it asks to pair from the host's Remote devices.

**The device key** is kept per host in the client origin's IndexedDB (`web/device-store.js`): the host's key and
the device key sealed with AES-GCM under a WebCrypto key made non-extractable, with `navigator.storage.persist()`
asked for. A copy of the database is ciphertext, but script on the client's origin can use the sealing key, which
is why the client gets an origin of its own (design §4.2, §6.2).

**Reconnecting.** A drop waits 1 s, doubling to 30 s with jitter (60 s after `quota`), and starts over once a
channel stayed up 30 s; each try asks the parent for a fresh ticket, then says `hello` with the epoch and cursor
the board holds, so the host replays what was missed or sends a snapshot. A gap in the events asks for one
`resync`. Operations still unanswered are sent again with the same IDs while younger than `op_ttl_secs`; older
ones say to check the board. At most 64 wait (`max_pending_ops`); past that the browser refuses new ones until
some are answered. A transport message that fails to decrypt ends the channel, and the frame dials again. While
pairing, `pairing_busy` (another device is pairing) is tried again with the same waits, saying so, until the link's
five minutes are up, counted from when the page got it (`pair.age_ms`). It stops for good on `no_pairing` at once
("This pairing link was used or has expired. Show a new code on the computer."), on `pairing_busy` once the link's
time is up, on the host's sealed refusal (and forgets the key), a bare `not_paired` close (it keeps the key),
`revoked`, a refused or failed pairing, a drop while the codes are being compared, or a handshake answer that does
not authenticate (the host's key is not the one this browser paired with).

**What it offers.** What the desktop offers wherever `atomics.v1` can carry it, asked for in `hello` with `board`,
`plans`, `approvals`, `diffs`, `terminals`, `input` and `mutations`. A command is offered when
`Command::remote` has mutations for it and `welcome`'s `may` grants them all, so a viewer, or a browser that is not
connected, sees no action it cannot take. The shared views ask `Backend::available` and draw no button, menu entry
or tab for the others:

- **Offered:** answering approvals; approving a plan, Dispatch, a card's start, accept, send back, unblock and reopen,
  integrate and Integrate; a tile's close and the sidebar's Close, asking where the desktop asks; review comments and
  Send review; the steer bar and "Include in selected" (wide only: a phone's terminal is right there); typing
  into terminals.
- **Close plan…** offers Archive alone: `atomics.v1` has no way to complete a plan. An open plan's tab closes the same
  way; a finished plan's tab has no close, since archiving one needs a check the protocol lacks, and its board's
  Archive stays. A closed plan's board stays read-only. A plan that closes while it shows gives way to the first open
  plan, as on the desktop.
- **`+ New`** (wide only) opens a small picker of what the host's board names: the agents its sessions and open plans use,
  and a scratch folder or a folder its sessions and open plans use. The host's configured agents and its
  worktrees are not sent, so a host with no session and no open plan offers none, and starting in a new worktree
  stays on the computer.
- **The changed-files column** scrolls to a file's hunks, which the page has already.
- **Hidden, for lack of an action or of what it reads:** reopening a plan, cleaning one up, completing one, editing a
  brief, syncing tickets, renaming and resuming a session, the editor, the whole file of a file with no changes, the
  explorer; and the voice, since a browser has no microphone here. The mirror does not say whether a session is
  sandboxed, outside ATOMICS or in a scratch folder, so closing an exited session outside a worktree and a plan asks, in
  case it deletes its folder. It has no agent kinds either: a steer to all agents leaves out those named `shell`, where
  it would run as a command.

A command that fails says why: refused, unconfirmed or too large, on the phone's notice line and in a note under the
wide layout's top bar. A steer goes back into the bar, an approval back to its banner, and a task's `integrating…` ends with
the outcome; a comment shows once the host has it. A diff on screen is fetched file by file with `diff_get`, four
files at a time, up to 16 pages each; the browser draws it without syntax colours.

**Layouts (C4).** A wide window draws the desktop's own window, `atomics_ui::shell::frame`, in the boot's theme
as the desktop draws it (monospace UI text where the theme says so; the default theme does): the top bar with the
plan tabs and the chief's pill, the sidebar where the window has room for it, the tiled terminals with their
headers and approval banners, the steer bar, the board or a worktree's diff in the right panel, and the chief docked
(at first) or floating. The items only a host has (Settings, sandboxes, credentials, the ticket sign-in, GitHub, the
load gauge, USAGE, Project) are hooks the browser leaves empty; the connection's state takes the place of the
desktop's Remote item, and the ⋯ layout menu that of Settings. What the desktop keeps in its
host's model about its window (the tab, the active session, the right panel, the chief's window) the browser keeps
per page (`atomics-web/src/desk.rs`), following the desktop's rules for the view commands; the tabs are the plans'
and Ungrouped, and a page opens as the desktop does, on the first open plan's with its board beside the agents, and
keeps its plan by id. Finished plans are reached through History; with no open plan, every agent shows and no board.
A click into a terminal, or picking an agent, sends what is typed there, as on the desktop: the page's one input
moves over it, focused, and the terminal it left gets its lease back. A chief's permission request is answered over
the chief's terminal, and connection trouble (why the page stopped, or that the board may be out of date) stays in
a line under the frame.

On a phone (narrower than 600 CSS pixels, or 900 with a coarse pointer; the ⋯ menu overrides it, remembered in the
client origin's `localStorage`), one pane at a time from a bottom bar, each a view of the desktop's: Agents (the
sidebar's AGENTS list; a tap opens the agent's tile), Board (the current plan's board under one row to pick plans
from: the open ones in a strip that scrolls sideways, the finished ones behind a scrolling History), Diffs (the
worktrees with changes, then the diff panel), Terminal (the active agent's fleet tile at the pane's size: its header,
its approval banner, its terminal) and Chief (the chief's window, with the request it waits on). The approvals are behind the top bar's ◆ badge, selected while they show. All of it
sits inside `visualViewport` and the safe-area insets (the sides too, for a phone held sideways). The page's own
controls take the theme's colours as CSS custom properties.

The phone draws the theme's colours zoomed by 1.25 (through the theme's UI size, so code and terminals keep their
CSS size), with proportional UI text and monospace only for code, terminals and IDs; every control is at least
44 CSS pixels square and the content keeps a 16 px gutter from the sides. The shared views fit it through
`Client::touch` (`atomics_ui::touch`), which the desktop never sets: the board stacks the statuses that have tasks
with full-width cards under its header and buttons; a tile fills the pane to its sides with a finger-sized header;
the diff header stacks the title, then the branch and counts, then its buttons in a row that wraps with Close last,
and the diff shows one side, numbers each line once and wraps long lines under their indent, measured by the
glyphs they are drawn with, so wide characters never run past the screen (**wrap** turns that off); approval
banners span the screen.

**Terminals (C4).** A terminal on screen is subscribed (focused); a terminal no frame shows is unsubscribed, and
the history kept for it freed. Its cells are the desktop's grid, drawn by the shared painter: the font fits the
width until the user pinches, on a phone never below 13 px, a wider grid pans, a grid taller than the pane keeps
the cursor in view, and the screen's own background fills the pane below a shorter one. Looking never resizes the
PTY. **Fit to this screen** sends `terminal_resize` for the pane at the current font, shown on the
desktop, and lasts as long as the lease (**Computer's size** gives it back early).

- **Input** goes through a transparent `<textarea>` over the terminal (`web/terminal.js`; design §4.4): committed
  text, composition as preedit (drawn over the cursor), named keys (Return and Backspace too) and Ctrl+character
  from `keydown` (Ctrl+V and Cmd stay the browser's), a soft keyboard's Return and Backspace from `beforeinput`
  where its `keydown` names no key, and pastes. The field has autocorrect, autocapitalisation, autocomplete and
  spellcheck off. A touch on it is handled by the client: a tap focuses it from the touch's own handler (which
  opens the keyboard), and the browser's loupe, callout and page zoom never start. It takes the pointer only where
  egui would give the terminal the pointer: a floating window over the terminal is cut out of it with a `clip-path`,
  so the browser sends that part's pointer to the canvas, and all of it goes while a menu or popup is open, so a click
  away closes the popup as it does anywhere else. A click on a text box the page draws, the steer bar or a comment
  box, takes the browser's focus with it: what is typed goes there, and nothing reaches the terminal.
- **The key bar** (two rows: Esc, Tab, sticky Ctrl and Alt, the arrows, PgUp/PgDn, `|` `~` `/` `-`, Paste) shows
  while the field has the focus on a coarse pointer, sits on `visualViewport`'s bottom edge, clear of the
  safe area's sides, and takes the bottom bar's place. Its keys are at least 44 CSS pixels each way (a screen
  narrower than 375 scrolls a row) and never take the focus; Paste reads the clipboard on the touch's end.
- **Touch and mouse.** A tap clicks the cell (press and release) where the program asked for mouse reports and this
  client types; a one-finger drag scrolls this client's own history (`history_get` as it reaches older lines; new
  output does not move a view scrolled back) and pans; a long press (450 ms) selects the word under the finger
  (letters, digits and `_-./~:@%+`; else the one cell), out to wherever the finger then moves, with a handle at
  each end to drag; a **Copy** chip, above the selection or below it where there is no room in the terminal, copies
  just that range on its touch's end, leaving the focus where it was (see the iframe above). The page's own text selection is off. A pinch sets the font size (5-28
  points). A touch whose lift never reached the client is cancelled when the next touch starts. A mouse clicks
  and drag-selects, and its wheel scrolls; Ctrl+wheel zooms. In the alternate screen, or with SGR mouse reports, a
  scroll sends arrows or wheel reports instead, as the desktop does, only while this client types.
- **Typing needs the lease.** Only the tap that focuses the field asks for a free lease; text typed meanwhile waits
  for the grant (16 KiB at most, this connection only), and a lease state sent before the host read the request
  leaves it standing. The input moving to another terminal, or to none, releases the lease (a grant still on its way
  goes back when it comes), so another browser never sees it held by someone who has moved on. Typing never asks for the lease: text, keys and pastes count only while the field has the
  focus and this client holds the lease or is asking for it, and anything else is dropped, never kept. Another
  client typing makes the field read-only, with **Take over**. The lease is renewed every 10 s while the field has
  the focus. Losing it (the desktop types or takes it back, another client takes over), or being refused it, takes
  the focus away, and what was in flight then is dropped: keys of that frame, a word the keyboard commits as the
  field loses the focus, a Paste whose clipboard read finishes late. The caret goes with the focus, so text a
  keyboard commits later cannot focus the field again. Typing again is a new tap. A paste over
  1 MiB, or more than may wait, is not typed, with a note naming the limit that refused it (16 KiB while waiting
  for the grant, else 1 MiB). Input refused as `slow_client` is typed
  again from the screen's next change on, at most 1 MiB per change. Lease requests, input and resizes carry
  request IDs, so a refusal names what it refuses; the client keeps at most 1024, forgetting input once a lease
  state shows it applied. A view-only device's field is always read-only.
- **Offline.** The screen stays on view, marked "Disconnected"; the field is read-only and nothing typed is kept:
  the lease and anything waiting end with the channel, and the screen is subscribed again after `welcome`.

**Tests.** `cargo test -p atomics-web`: the boot's origin, window and field rules; the channel, pairing, the
board, commands and grants, reconnecting and every stop against an in-process host built from `atomics_core`
(`src/fake_host.rs`, whose terminals echo under leases); the terminals' leases, typing, history, no replay and
`slow_client`; that nothing typed after a take back types or asks for the lease; the gestures; the overlay's
events and what is cut out of it; the page's plan and tabs against the desktop's rules; the phone layout at 360, 390
and 430 points; key encoding per terminal mode (`atomics_ui::terminal::encode`);
the views drawn headless, each command as a controller, a viewer and offline, what it sends and what the host does with
it, its refusal and what it leaves on screen; the manifest; and that the crate names no hosting product.

`tests/browser.rs` runs the real bundle in headless Chrome against that host, behind a fake broker. On a desktop
it pairs, reconnects and reloads, types into a terminal through DevTools, sees the echo, and loses and regains the
lease. As an iPhone (390 × 844 points at 3×, a touch screen, Safari's user agent, framed full screen) it is used
by touch alone: it pairs, shows the board, the AGENTS list and a two-file diff, approves the plan and a tool request
with a tap on their buttons (checking the `plan_command` approve and the `approval_decide` the host receives),
focuses a terminal with a tap, types a line, Return, Backspace and the key bar's ↑ (checking the bytes, and that the
key is 44 CSS pixels each way), and once the desktop takes the terminal back checks that the field lost the focus
and typing sends nothing; then it long-presses a word, drags the selection's end handle over the next word and
checks that Copy copies exactly those two words. It saves every screen as a PNG in `target/phone-shots/` for a
person to review. A click on a floating chief types there, and a click on a ⋯ menu entry over a terminal acts and
takes no lease; a host of 26 plans, 25 of them archived, opens with the open plan's Approve plan on screen in a wide
window and on a phone, where History reaches the others (`target/many-plans-shots/`). In a 1600 × 1000 window it
checks the desktop's frame: the board, the docked chief's terminal and the agent's tile, a click into either terminal
typing there and releasing the other's lease, and Approve plan; it saves `target/wide-shots/browser.png`
and, beside the desktop's QA screenshot of the same board (`scripts/wide-desktop-shot.sh`, which writes
`target/wide-shots/desktop.png`), `target/wide-shots/side-by-side.png`. The actions are clicked and tapped where the
fake board puts them, saving `target/actions-shots/`: while a terminal has the focus, a click on the steer bar or a
comment box moves the keyboard there, what is typed reaches the host as a steer or a comment, Send review sends it,
and nothing reaches the terminal; a refused steer says why and comes back; start and Dispatch; a tile's close with its
confirmation; the picker of `+ New`; Close plan… with Archive alone; "Include in selected" and a steer to the
selected; unblock, integrate, accept and Integrate; the changed-files column; and on a phone 360 CSS pixels wide, by
touch, integrate, accept, Integrate, Close plan…, a comment and Send review, a tile's close and the archived plan
from History. All of them need the bundle and Chrome, so they are ignored by default:

```sh
scripts/build-web.sh
cargo test -p atomics-web --test browser -- --ignored
# the phone alone, printing where its screenshots are:
cargo test -p atomics-web --test browser -- --ignored --nocapture the_bundle_works_on_a_phone_by_touch
# the desktop's QA screenshot first, then the wide window beside it:
atomics queue --class qa -- scripts/wide-desktop-shot.sh
cargo test -p atomics-web --test browser -- --ignored --nocapture the_bundle_draws_the_desktops_frame_in_a_wide_window
```

`ATOMICS_WEB_DIST` names another bundle folder and `CHROME` another browser.
`crates/atomics-ui/tests/dependencies.rs` keeps desktop-only crates out of `atomics-ui` and `atomics-web` on both
targets, and CI checks both for `wasm32-unknown-unknown`.

## Container state and credentials

Run one non-root host per user/pod with a stable UID and no display server, GPU or Docker socket.
Persist a private user home and repositories/worktrees on user-scoped volumes, at stable absolute paths
so git worktree metadata and recorded cwd values remain valid across pod replacement.
Set `ATOMICS_HOME=/data/atomics`, `ATOMICS_CONFIG=/data/config/atomics/config.toml` and
`ATOMICS_SOCKET=/run/atomics/atomics.sock` explicitly. The socket and PID/lease files are ephemeral.
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
2. **Runtime without painting — complete.** Extract host ownership behind an in-process command/event boundary;
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
Use an isolated `ATOMICS_HOME`, `ATOMICS_SOCKET` and `ATOMICS_CONFIG` for UI checks, verify the process environment
with `ps eww <pid>` on macOS or `ps eww -p <pid>` on Linux, and never run migration experiments against the user's running ATOMICS instance.

## Phase 2 complete: in-process native host

`host::Host` owns the native runtime model and services it on the
`atomics-runtime` thread. Hook/control requests, terminal events, worktree discovery,
filesystem notifications, diff/file results, integrations, heartbeat and cleanup
results wake its condition variable directly. Approvals, delivery verification,
waiters, fallback status checks, configuration and diff scheduling use host deadlines.
None of those operations needs an egui pass or a repaint callback to make progress.

The native desktop owns its terminal views, selection, diff navigation and scroll
positions, comment drafts, explorer state, dialogs, steering drafts, focus,
layout inputs, screenshot saving and microphone lifecycle. Desktop runtime actions
are queued in-process; session and plan IDs are resolved again when executed.
Native view hints remain in the shared model for ctl compatibility and scheduling.
The desktop still borrows that model under a mutex while rendering; it never pumps
runtime work or waits for a command while holding the guard.

`terminal::runtime::Terminal` owns the child, PTY sender and alacritty emulator.
Its listener has no egui dependency: it answers color/size queries locally,
records title/exit/output changes and signals the host. Clipboard escape requests
are consumed by the native client. `TerminalView` owns painting, geometry,
keyboard/pointer encoding, scrolling and selection. Host notifications retain the
100/200/500 ms output cadence, using expiring wall-clock visibility hints instead
of egui frame counters; input echo can still flush immediately.

`diff_worker` owns preparation, coalescing, budgets and cache reuse. It publishes
results to a host-signalling channel. `DiffData` holds completed revisions,
accepted comments, file subscriptions and refresh scheduling; `DiffState` holds
client navigation, collapsed sections, scroll position and the draft editor.
Highlight spans use byte offsets and plain RGBA values. Cleanup jobs remain owned
and polled by the host after their client dialogs close.

Fourteen production-thread gates in `app::host_tests` run without drawing a frame
or calling a runtime tick: hook reply, approval expiry, held external messages,
status-change wait, wait timeout, heartbeat, ctl plan persistence, real Git
integration persistence, agent exit with saved task status, delivery timeout,
filesystem changes publishing fresh diffs, held PTY input receiving its receipt,
cleanup previews completing, and desktop-command shell launch/input delivery.
A listener test also verifies that exit wakes a host without any client.
Direct UI harnesses service the same action queue after a simulated frame.
Race regressions also deliberately leave actions and client events unconsumed
across frames: approval decisions carry stable request IDs and pending controls
are disabled; a blocked fake native picker lets the real host keep servicing
hooks and waits; late file-open events preserve the host's resolved target.
Terminal regressions cover selection invalidation after output/resize and editor
replacement at unchanged panel geometry.

Queued native QA with isolated `ATOMICS_HOME`, `ATOMICS_SOCKET` and `ATOMICS_CONFIG`,
verified on the GUI process with `ps eww`, passed shell input/output, fresh diff,
ctl plan persistence, heartbeat, fleet/board/chief screenshots and clean shutdown.
The screenshot harness requests paints while obscured so the existing CLI
screenshot delay can advance. Targeted application and terminal tests pass.
Final validation passed `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and all 314 tests through
`atomics queue --class test -- cargo test --workspace`. The separate narrow top bar
follow-up also passed 480/550-point paint and click tests and isolated queued
native screenshots at both widths.

Plan follow-ups passed read-only completed/archived board, tab close/confirmation,
History View/Reopen, and activity text/color tests. Urgent-plan overflow tests
cover 480, 900, 1200 and 1400 points; 900 and wider keep both urgent plans visible,
while the Tabs button surfaces urgent counts if folding becomes unavoidable.
Isolated queued native QA at 1400 points verified a running plan, a needs-you
plan, an idle plan, two completed plans and archived History. Screenshots and
`ps eww` isolation evidence are in
`/private/tmp/atomics-plan-ux-qa-06344607-v4/`. History View stayed read-only and
History Reopen persisted the open status. The QA process then exited cleanly.

### Remaining for phase 3

- Done: the host starts independently of the native window and creation context, and the egui
  notification and style/viewport adapters are out of service construction (`atomics host`, above).
- Replace the shared native model borrow and emulator reads with immutable,
  versioned client projections and terminal screen snapshots/deltas. Keep the
  native input/resize channel behind the protocol adapter, with control leases.
- Implement the versioned loopback WebSocket protocol and authenticated pairing,
  subscriptions, bounded queues, reconnect/reset handling and slow-client tests.
- Done for the protocol (H4, "Mutations" above): operation acceptance/outcome journaling, duplicate-command and
  uncertain crash recovery, with hook, ctl and MCP formats unchanged.
- Change shutdown ownership: closing/reopening the desktop must preserve child
  PIDs, emulator state and plan progress. Today exiting the entire application
  still joins the host and shuts down its terminals, as before.

The separate daemon, browser rendering and remote deployment are not shipped by
phase 2. Their gates remain the phase 3–5 gates above.
