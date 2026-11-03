# Embedded ATOMICS: extracting the web UI (task C1)

Companion to `2026-11-02-embedded-atomics.md` (§4.1 client, §9 stream C, §10.2 slicing). Numbers below were counted on
this branch with `grep` and `wc`, not by a build. Line counts are approximate (test modules are mixed into the files).

## 1. What the UI is today

| Thing | Fact |
| --- | --- |
| Size | `app.rs` + `app/*.rs` + `diff_view.rs` + `terminal*` + `theme*` + `markdown.rs` + `panel_nav.rs` + `highlight.rs`: about 44,000 lines, tests included |
| Shape | `State` (`app.rs:441`) is the host model: 117 fields. `Client` (`app.rs:631`) holds view state: drafts, dialogs, `diff_views`, terminal views, expanded tasks. `Desktop<'a> { runtime: &mut State, client: &mut Client }` derefs to `State`. **The split the design asks for already exists** in the type system: draw code is `impl Desktop<'_>` (23 files) or `impl State` (35 files, mostly runtime logic) |
| Writes | Already a queue. `Desktop::post(FnOnce(&mut State) + Send)` (`app/commands.rs:19`) is used at 50 sites, plus `post_plan` and `post_session`, which address by id. A few views still mutate `runtime` directly (`selected_plan`, `plan_view`, `diff_open`, `panel`: view hints) |
| Reads | Direct: `self.runtime.<field>` |
| egui in the model | `State` carries `ctx: egui::Context`, `wake`, `palette: Arc<Palette>` (egui colors and fonts), and `ClientEvent::Viewport` |

### 1.1 What the draw code reads

`runtime.<field>` occurrences in all files (tests included), top of 70+ distinct fields:

| Domain | Fields (uses) | Where drawn |
| --- | --- | --- |
| Plans and board | `plans` 550, `selected_plan` 155, `plan_view` 27, `integration_checks` 22, `integrating`, `changed_plans` | `sidebar.rs` (tabs), `topbar.rs`, `board.rs` (`plan_panel`, kanban), `plan_view.rs`, `plan_cleanup.rs` |
| Sessions | `sessions` 475, `active` 43, `dismissed`, `events` | `sidebar.rs`, `fleet.rs`, `approvals.rs`, `chief.rs`, `input.rs` |
| Tickets | `tickets` 282, `ticket_config()` 24 | `tickets_panel.rs` (3,093 lines), `board.rs`, `sidebar.rs` |
| Config | `config` 252 | everywhere: fonts, keys, layout |
| Look | `palette` 93 | everywhere; egui `Color32` and `FontId` inside |
| GitHub | `github` 104 | `github.rs` (1,635 lines) |
| Diffs | `diffs` 30, `diff_target()`, `diff_open`, `panel` 71 | `editor.rs` (`diff_panel`), `diff_view.rs`, `github.rs` |
| Everything else | `launch_dir`, `sbx`, `relay`, `connections`, `store`, `repos`, `lane`, `heartbeat` | settings menus, trace lane, sandbox UI |

Files with the most `ui` code and `runtime` reads in production code: `board.rs` (89 reads), `github.rs` (71),
`sidebar.rs` (43), `tickets_panel.rs` (35), `editor.rs` (31), `chief.rs` (26). Files that read **no** `runtime` at
all and are therefore movable as they are: `diff_view.rs`, `panel_nav.rs`, `markdown.rs`, most of `topbar.rs` helpers.

### 1.2 Desktop-only dependencies, and where they are used

| Crate | Used in | Web answer |
| --- | --- | --- |
| `alacritty_terminal` | `terminal.rs`, `terminal/runtime.rs` | A cell-snapshot type in `atomics-core` (C3); the web crate never sees `Term` |
| `syntect`, `two-face` (onig, C) | `highlight.rs` only | Section 6: the host highlights; the browser receives spans |
| `cpal` | `voice.rs` | Stays desktop; the web client has no microphone in release 1 |
| `rfd` | `app.rs` | Native file dialog; the web crate asks the `Backend`, never opens a dialog |
| `notify` | `watcher.rs` | Host side (runtime), not UI |
| `ureq`, `ring` | `github.rs`, `relay.rs`, `mcp_client*`, `credentials.rs` | Host side (services); the UI sees `GithubView`-style snapshots |
| `fontique` + fontconfig | `theme/fonts.rs` | Desktop font lookup behind the `Palette` build; the web crate loads bundled fonts |
| `libc` | process and socket code, 19 files | Host side |
| `png`, `eframe` native, `clap` | `main.rs`, `app/editor.rs` (`save_png`) | Desktop shell |

The UI proper uses none of these except through `State`. That is the extraction's good news: the dependencies hang off the
model and services, not off the drawing.

## 2. The Backend trait

`atomics-ui` (new, `crates/atomics-ui`) holds the trait and the plain data it returns. It depends on `atomics-core`
with `default-features = false` and nothing else, so it builds for wasm32; `tests/dependencies.rs` fails if a
desktop-only crate, `atomics`, or `atomics-core`'s `native` feature enters its tree on the host or the wasm target.

```rust
pub trait Backend {
    fn plan_list(&self) -> Vec<PlanRow>;
    fn plan_row(&self, index: usize) -> Option<PlanRow>;
    fn diff_roots(&self) -> Vec<PathBuf>;
    fn diff(&self, root: &Path) -> Option<DiffSnapshot>;
}
```

Rules that keep every slice small and mechanical:

1. **Reads first, owned or shared data out.** `&self` methods returning owned rows or `Arc`s. A protocol client implements
   them over its last snapshot; the in-process one (`app/backend.rs`, `Local`) over `&State`. No method takes a closure
   over the model, so nothing about the UI depends on a lock being held.
2. **A view type only for what the UI reads**, never `State` fields wholesale. Types that are already browser-safe in
   `atomics-core` (`Plan`, `Task`, `ApprovalRequest`, `ReviewComment`) are returned as they are. Types that were in the
   desktop crate move to `atomics-core` first (`diff_data.rs` this time), with a `pub use` left behind.
3. **Writes are `Command` values** (slice S8, 4.7): the draw code records them, a view command stays with the client
   and every other one goes to `Backend::send`. No draw code posts a closure.
4. **Indices are positions in the last list read**, as today. The protocol slice (H1) addresses by id; `post_plan` already
   converts.
5. No behaviour change in any slice: the slice's tests are the existing ones, run unchanged.

## 3. Slice plan

Each slice leaves the desktop working and builds alone; each is sized to merge `main` into the branch around. Estimates
are agent-days of focused work, review wait excluded. "C1"/"C2" are the design's streams (§9).

| # | Slice | What moves | Est. | Stream |
| --- | --- | --- | --- | --- |
| **S0** | **Done in this change.** `atomics-ui` crate with `Backend`, `PlanRow`, `PlanState`, `DiffSnapshot` and the dependency test; `Local` in-process impl; plan tabs, top bar plan button and `sync_diffs` read through it; `Prepared`, `FileView`, `Span` moved to `atomics-core::diff_data` | ~400 lines | 1.5 | C1 |
| **S1** | **Done** (4.2). Session list: `SessionRow` (id, title, status, kind, chief, branch, root, approval counts, worktree) read by `sidebar.rs` `session_row`, `fleet.rs`, `ungrouped_session`. Largest single read domain (475 uses): do it in three PRs by file | 3 PRs | 2 | C1 |
| **S2** | **Done** (4.4). Board: `plan_panel` (`board.rs`, 89 reads) onto `BoardView` { plan, ready ids, session titles, integration notes, integrating, ticket config, board counts }. Plan header first, kanban columns second | 2 PRs | 2 | C2 |
| **S3** | **Done** (4.4). Approvals and review comments: `approvals.rs` draw path onto `ApprovalRow`; comments on `DiffSnapshot` already | 1 PR | 1 | C2 |
| S4 | Ticket panels: `tickets_panel.rs` (3,093 lines) onto `TicketsView`. Split by panel (tree, find, sign-in) | 3 PRs | 2.5 | C2 |
| **S5** | **Done** (4.3). `Palette` (egui colors and fonts) out of `State`: the web crate owns `Palette`; `fontique` lookup stays a desktop hook that supplies the fonts (`theme::FontSource` already names a font in the config). `config.keys` and `layout` are already core | 1–2 PRs | 1.5 | C1 |
| **S6** | **Done** (4.5). Move the files that read nothing from `runtime`: `diff_view.rs`, `panel_nav.rs`, `markdown.rs`, `topbar` helpers into `atomics-ui` (adds `egui` with default features off). Needs S5 for `Palette` | 1 PR | 1.5 | C1 |
| S7 | Move `Client` and the draw functions: `fn draw_*(ui, &dyn Backend, &mut Client)` per file, `Desktop` becomes a shim holding `&mut State` for the writes not yet converted. Order: `topbar`, `sidebar`, `board`, `fleet`, `chief`, `editor` (diff panel), `github`, `tickets_panel`, settings menus. Moved so far (4.6): `board`, the approval banner, the worktree diff view, `sidebar`, `topbar` and `fleet` | 8–10 PRs | 5 | C1 |
| **S8** | **Done** (4.7). Writes: `Command` enum and `Backend::send`, domain by domain, replacing `post` closures (50 sites) and the direct `runtime.*` view-hint writes | 5 PRs | 3 | H4 / C1 |
| **S9** | **Done** (4.8). `atomics-ui` builds for `wasm32-unknown-unknown` in CI (`cargo check --target`), eframe web entry point `atomics-web` | 1 PR | 1.5 | C5 |

**Sum:** C1 (S0, S1, S5, S6, S7) ≈ 11.5 days, inside the design's 10–15. C2 reads (S2–S4) ≈ 5.5, inside 5–8. S8 and S9 belong
to H4 and C5. The terminal painter (C3) is independent of this list: it needs only the cell type in `atomics-core`.

Risks: S7 is where merge conflicts with `main` concentrate (draw functions change daily); do it file by file, merging
`main` first each time, and announce large UI changes on `main` early. S4 and `github.rs` carry the most
stateful view code; they may need a `ViewState` struct per panel before they move.

## 4. What each slice changed, exactly

### 4.1 S0

- `atomics-core/src/diff_data.rs`: `Span`, `Spans`, `FileHighlights`, `Prepared`, `FileView` (were in `highlight.rs` and
  `diff_worker.rs`, which re-export them, so nothing else changed).
- `atomics-ui`: `Backend`, `PlanRow`, `PlanState` (moved from `plan_view.rs`; `color()` stays in the desktop as the
  `PlanColor` trait because it needs the palette), `DiffSnapshot`.
- `app/backend.rs`: `Local`, the in-process implementation. `sidebar.rs` (`urgent_tabs_label`, `tabs_summary`,
  `urgent_navigation_width`, `view_tabs`, `view_tab`), `topbar.rs` (plan button) and `sync_diffs` read through it.
  `board.rs` is **not** moved yet: it is S2. The plan list is read once per call, and per tab for `view_tab`.

### 4.2 S1

Three commits, one per file group; no existing test changed:

- `atomics-ui`: `SessionRow` and `Backend::session_list` / `session_row`, addressed by position like plans. Beyond the
  list above, a row carries what the draw code reads: `agent`, `project` (the host's `project_of`, which a browser cannot
  compute), `ungrouped` (the host's `ungrouped_session`), `scratch`, `sandbox`, `outside` (no terminal in ATOMICS) and
  `changes` (the root's diff counts). Approval counts are `pending_approvals` and `held_messages`. The worktree is its
  root path: `atomics_core::git` is native-only.
- `app/backend.rs`: `Local` builds the rows; `session_rows_follow_the_model` checks the mapping.
- `sidebar.rs`: `session_row` draws a `SessionRow`; the All, Ungrouped and project counts, the Repos change total and the
  explorer (agent counts, worktree dots and pick) read rows.
- `fleet.rs`: the tab strip and the tile headers read rows.

`ungrouped_session` stays on `State` as the host's rule; the draw code reads it as `SessionRow::ungrouped`. Still on
the model, on purpose: `State::visible_sessions`, `show_ungrouped`, `set_view`, `completed_tasks` and `close_idle`
(runtime logic, also used by `control.rs`; the view filter moves with `Client` in S7); the tile body (approval banner: S3; MCP banner; the terminal and the outside-ATOMICS transcript: C3); the tile
subtitle's slot-queue note (needs the terminal's process id); the close and resume handling (writes: S8); the RECENT
list's claude ids (the store's domain).

### 4.3 S5

One commit; the existing theme tests moved with the code they test, nothing else changed:

- `atomics-ui` depends on `egui` (default features off, `default_fonts` on: a browser needs egui's fonts behind a
  theme's for missing glyphs). `atomics-ui::theme` holds `Palette` (now built by `Palette::new(&ThemeTokens, Fonts)`),
  its style, the theme preview, `lerp` and `parse_hex`, and `FontSource` / `Fonts`. Finding a font is a hook:
  `Palette::apply(ctx, &dyn FontHook)`, and `definitions(ui, mono, load)` puts found faces in front of egui's.
- Desktop `theme.rs`: `Theme`, the host's theme free of egui: resolved tokens, the fonts they name (paths parsed
  against the themes folder, `~` expanded) and the syntax theme checked against the highlighter. It gives
  terminals their colors (`terminal_colors`, OSC 4/10/11/12) and agents `COLORFGBG`, and makes a `Palette`.
  `theme/fonts.rs` is the desktop hook, `SystemFonts`: fontique and font files, off the UI thread, as before.
- `State` holds `theme: Arc<Theme>` instead of `palette: Arc<Palette>`; a proposed theme's preview is a `Theme`
  too. The client makes its palette from the host's theme and makes it again when that `Arc` changes
  (`Desktop::palette()`); `ConfigChanged` applies it to egui as before.
- `atomics_core::theme::xterm_color` holds the xterm 256-color cube and grays, so the host's OSC answers and the
  painter read the same colors; `terminals_answer_the_colors_the_painter_draws` checks both agree for every theme.
- The color-literal test scans `atomics-ui` too.

Checked with screenshots of the same scene before and after, on macOS and Linux (Xvfb), for the built-in claude
theme and for a light theme file with an installed family for UI text, a font file for monospace and a larger
`ui_size`; the shell prints glyphs neither font has.

### 4.4 S2 and S3

Four commits, the board's header before its columns; no existing test changed:

- `atomics-ui`: `BoardView` and `Backend::board(index)`, addressed by plan position. The plan comes whole (`Plan` is
  browser-safe), so the board still works out from it what the plan alone decides: counts, stages, dependencies, the
  ticket mode and the "ticket says" labels. Beside it come the list above (`ready` in plan order; `sessions` as title
  and status by id; `integration_notes` as note and whether its check runs; `integrating` as this plan's task ids;
  `board_counts` as the board's tickets and those without a task), plus `state` (the goal's color) and `checking`
  (an integration check of the plan runs). `ApprovalRow` { session, id, request } and `Backend::session_approval(index)`:
  the first request a session waits on, which is what its banner shows, with `ApprovalRequest` as it is.
- `app/backend.rs`: `Local` builds both; `boards_and_approvals_follow_the_model` checks the mapping.
- `board.rs`: `plan_panel` reads the selected plan's `BoardView` once per frame, the header first, then the kanban
  columns. Its writes still go through `post_plan`, addressed by the index it read.
- `fleet.rs`: the tile's approval banner reads the session's `ApprovalRow`. `approvals.rs` holds no draw code (it is
  the host's approval logic), so the banner is the approvals draw path. Review comments already reach the diff panel
  on `DiffSnapshot` (S0).

Still on the model, on purpose: `task_result` (shared with the completed-task tiles; it reads `config` for a Codex
resume and checks the task's worktree on disk, so it moves with the tiles in S7); the `plan_edit_open` and `panel`
writes and `task_action`, `open_plan_cleanup` and `activate` (S8). The palette is the client's since S5
(`Desktop::palette()`). A shape dump of the board and the banner (open, unapproved, completed and archived plans,
board and ticket mode, two widths; Bash, Azure and pending decisions) was byte-identical before and after.

### 4.5 S6

One commit; the tests moved with the code they test, nothing else changed:

- `atomics-ui` holds `diff_view` (the diff panel: `DiffState`, `ExplorerState`, `DiffAction`, `Header`, `show`),
  `panel_nav` (the keyboard focus and row scrolling that the diff, the Tickets panel and the trace lane share),
  `markdown` (the ticket text reader), `keys` (the chord helpers `shortcut`, `shortcut_label`, `consume_chord`,
  `consume_physical_shortcut`, `is_app_chord` and `is_digit`, which were in the desktop's `theme.rs`; the diff
  header labels its editor shortcut with them) and `topbar` (`measure`, `slot`). `egui` came in with S5.
- The desktop imports the three modules at its crate root (`use atomics_ui::{diff_view, markdown, panel_nav}`), so
  every `crate::diff_view::…` path in it reads as before; `theme.rs` re-exports the chord helpers and
  `app/topbar.rs` the two layout helpers.
- `topbar::record_item` stays in the desktop: it records where items were drawn only in the desktop's own test
  build (`cfg(test)`), which code in another crate cannot see.
- The wasm32 check of `atomics-ui`, the dependency test and the color-literal scan (which reads `atomics-ui/src`)
  pass unchanged.

Checked with screenshots before and after on macOS and Linux (Xvfb): a changed and a new Rust file with a review
comment, and on macOS also an unchanged file's whole-file view. The diff panel is identical pixel for pixel; the
rest of the window differs only where two runs of the same build differ too (the load gauge, terminal text).

### 4.6 S7

The draw code moves into `atomics-ui` file by file. A moved view is a function of `(ui, …, &dyn Backend, &mut
Client)`:

- `atomics_ui::Client` holds the view state the moved code keeps (the palette to draw with, open cards,
  confirmations). Fields leave the desktop's `Client` as the code that uses them moves; the desktop's `Client`
  keeps the rest and holds the moved part as `ui`, and derefs to it, so desktop code and tests that name a
  moved field read as before.
- Writes are `atomics_ui::Command` values recorded by `Client::send` and drained after drawing; host
  commands go through `Backend::send`, while view commands run on the client. A plan's position comes
  from the last list read. `Desktop` draws through the in-process backend. The desktop's `Local` backend resolves
  positions to IDs and posts mutations to the runtime thread (`app/commands.rs`), which looks up those IDs
  again when executing them. View commands update the client's view state. Mutations therefore run when
  the runtime drains its actions, rather than synchronously during drawing (see 4.7).
- Facts the draw code read from the host's disk or native modules come from the host on the view types.

The S7 commits below originally called these writes intents; S8 replaced them with commands (4.7).

Commits, one per file group:

1. **Board** (`board.rs`). `atomics_ui::board::draw` is `plan_panel`, the plan header and the kanban
   columns, with `ticket_chip`, `stage_chip`, `mode_label`, `tracked_label` and `task_result`; its
   clicks, which it already gathered and handled after drawing, become intents. `PlanState::color`
   replaces the desktop's `PlanColor`, `atomics_ui::status` holds `status_color`, `status_dot` and
   `task_color`, and `atomics_ui::text` holds `truncate`. `BoardView` gains what `task_result` read from the
   host: `worktrees` (finished tasks whose worktree is still on disk), `codex` (tasks of a Codex agent, which
   resume without a saved id) and `now`, the host's clock, which the ticket chip and a task's last report
   compare against. The completed-task tiles (fleet, not moved yet) call `task_result` through a desktop
   wrapper that works these out as before. `expanded_tasks` and `confirm_plan_close` move to the client.
   `plan_with_chief_names_the_auto_decision_in_the_board_header` moved with the labels it tests.
2. **Approvals** (`fleet.rs`). `atomics_ui::approvals::banner` is the permission banner of a session's tile:
   it reads the session's `ApprovalRow` itself and records `Intent::Decide`, which runs `approve_request` as
   before. `approval_decisions` moves to the client. The tile around it stays in the desktop until fleet moves.
   The `cfg(test)` record of the banner's buttons is `cfg(feature = "test-support")` in `atomics-ui`; the
   desktop's dev-dependency turns the feature on, as it does `atomics-core`'s.
3. **Diff view** (`editor.rs`). `atomics_ui::diff_panel::draw` is the end of `diff_panel`: a worktree's diff
   under its header. Two reads join the `Backend`: `diff_heading(root)` (the active session's title and
   branch, else the worktree's, and whether an agent there can take a review) and `ui_config()` (the
   config's `keys`, `font` and `layout`, which the header's shortcut, the diff's font and the split read).
   Review comment edits, Send review, Refresh, Close and Edit become intents; the side-by-side toggle stays
   on the client with `diff_views` and `diff_split`, which move. The rest of `diff_panel` stays in the
   desktop until the panels it switches between move: the board (moved), the Tickets panel (S4), the pull
   request diff (`github.rs`), the changed-files column, whose clicks open the editor in editor mode, and
   the editor terminal.
4. **Sidebar** (`sidebar.rs`). `atomics_ui::sidebar::sessions` is the AGENTS list (session rows, their menu,
   worktrees being checked out) with RECENT, and `atomics_ui::sidebar::explorer` the EXPLORER (projects,
   repository workflow, worktrees, files). New reads: `view_hints()` (`ViewHints`: the active session and
   the diff target, as the model holds them), `visible_sessions()` (the current tab's sessions, by position;
   the rule stays the host's, `control.rs` uses it too), `checkouts()`, `recent_sessions()` (the RECENT
   filter, which checks folders on disk, with each folder as the host writes paths), `repos()`,
   `project_of(dir)`, `repo_workflow(project)`, `worktrees(project)` (`WorktreeRow`: `git` is native-only)
   and `display(path)`; `UiConfig` gains the agents' names. Clicks become intents: activate, rename,
   resume, pick a project, start an agent in a folder, new worktree, close a project, show a folder's
   diff, clean up worktrees, open a file. `selected`, `confirm_close` and `explorers` move to the client.
   The sidebar frame (USAGE, the TICKETS section of S4) stays in the desktop and calls both.
5. **Top bar** (`topbar.rs`, the tabs in `sidebar.rs`, the history in `plan_view.rs`). `atomics_ui::tabs` is
   the navigation: the tabs, their overflow menu and the plan history (`view_tabs`, `urgent_tabs_label`,
   `urgent_navigation_width`). `atomics_ui::topbar` gains the brand, the notice line, the diff, `+ New` and
   Plan buttons and the Project menu. `ViewHints` gains the tab on screen (`plan_view`, `ungrouped`,
   `project`, `selected_plan`) and the diff panel's `diff_open` and `panel`; `PanelMode` moves to
   `atomics-ui`. New reads: `plan_date(index)` (the history's date: a file's time in the host's time zone),
   `notices()`, `sandbox_load()` and `project_menu()` (recent projects that still exist, workspaces). The
   buttons are three draw calls, so a click on Show diff reaches the Plan button drawn after it in the same
   frame, as before. The tabs are one: a click on a tab takes effect once the strip is drawn, so in that
   frame the tabs after it still show the old selection, as the tabs before it always did, and the next
   frame shows the new one. `icon` and `workspace_input` move to the client, and `record_item` and the history's
   records to `atomics-ui`, behind `test-support`. The bar's layout (`top_bar`) stays in the desktop with the
   items that have not moved: the chief pill (moved with the chief, 7), the Tickets button and sign-in (S4),
   the GitHub counter, the load gauge and the Settings menu.
6. **Fleet** (`fleet.rs`). `atomics_ui::fleet::tiles` draws the session tiles (header with status, title,
   agent, branch and status, sandbox badge and close; the approval banner; the MCP banner; the plan task's
   last report; the terminal, or the last exchange of a session outside ATOMICS; the exited overlay) and
   the completed-task tiles, and `fleet::tab_strip` the session tabs over them. The terminal is drawn
   through a hook, `atomics_ui::terminal_surface::TerminalSurface` (`show(ui, index, session, rect, id,
   metrics, palette)`): the desktop's `Terminals` draws it with its `TerminalView`s, a browser will draw the
   screen the host streams. `terminal::Metrics::new` makes the cell metrics from the palette and the
   config's font size. Two reads join the `Backend`: `tile(index)` (`TileView`: the task's last report,
   the sandbox's usage and the sampling error, the slot-queue note, the MCP banner's `ConnectButton`s, which
   moved to `atomics-ui`, whether the process exited and can be resumed, and an outside session's folder,
   last prompt and message) and `completed_tasks()` (`CompletedTile`: plan and task positions, the task,
   and the `read_only`, `worktree`, `codex` and `now` that `task_result` needs, worked out as the board
   works them out). Clicks become commands as before, plus `ResumeSession(id)` for the exited overlay's
   Resume, which runs `State::resume_exited` on the runtime thread: the record, the scratch folder kept and
   the close and resume, which the tile did at the click before. `focus_terminal` and `status_glyph` move
   to `atomics-ui`; `connect_buttons` is `State`'s. Host rules stay in the desktop: `visible_sessions`,
   `completed_tasks`, `close_idle` and `queue_note`. `RemoteBackend` reads a tile from the mirror (exited,
   folder) and has no completed tiles yet.

   Checked with `scripts/linux-qa.sh` screenshots at 1600×1000, two before and two after: the project's
   shell, which shows as outside ATOMICS there (no sandbox in the container), two live shells and an
   exited one on the tab strip. Before and after are identical pixel for pixel except the load gauge and
   the temporary folder's random name in the outside transcript, which differ between two runs of the
   same build too. The completed-task tiles and the exited overlay were not on screen: the tiles' tests
   ran unchanged, and the overlay, which no test draws, moved line for line.
7. **Chief** (`chief.rs`, and the steer bar in `app.rs`). `atomics_ui::chief` draws the chief's pill, its
   window and the steer bar. A new read, `Backend::chief()`, returns `ChiefView`: the chief's session and
   status, whether its window shows, docked or floating, the width the user gave it, and the last heartbeat
   with its age and local time. The host works those two out, because a browser has neither its clock nor
   its time zone (`store::now` is native-only). `pill` reads it and the selected plan's row. The platform
   passes what the microphone does as `VoiceState`; `None` (no microphone) leaves voice out of the tooltip.
   The window is three frames and a body. `dock_width` is the width the docked chief asks for (the user's,
   else a third of what the sidebar leaves). `dock` is the left panel, `window` the floating one, and
   `content` the header (minimize, dock or float) with the chief's terminal. `divider`, drawn after the
   central panel, resizes the dock.
   `ChiefView::session` is the chief's position in `session_list`, and `content` draws its terminal
   through the fleet's `TerminalSurface` (6) when the chief runs inside ATOMICS; the desktop passes its
   `Terminals`. `content` returns the strip under the terminal that the platform asked for. The desktop
   draws its trace lane (host-only: the chief's ticket calls) there after `content` returns, since the
   lane writes the model, which the backend borrows while `content` runs.
   `steer_bar` and `send_steer` read the targets from session rows, which gain `can_receive`. A message
   with no target records `Command::Toast` and keeps the text. Three view commands are new: `HideChief`
   (minimize, which wrote `chief_open`), `ResizeChief` (the divider, which wrote the store's
   `chief_width`) and `Toast`. The desktop carries them out at once, as before. `steer_text`,
   `steer_target` and `focus_steer` move to the client, and `SteerTarget` with them.
   `Desktop::send_steer` stays, test-only, as the steer bar's Send for the tests that call it, and
   `lane_height` moves to `State`. The divider's test moved with it and reads the commands it records.
   Still in the desktop: the host's chief logic (`wake_chief`, `start_chief`, `toggle_chief`,
   `set_chief_dock`, delivery) and the frames around the steer bar and the dock in `paint` (the window's
   split, S7s).
   Checked with Linux QA screenshots before and after (`scripts/linux-qa.sh screenshot`, 1600x1000) in four
   scenes: no chief, docked, floating, and started then hidden. The image has no agent CLI, so a shell
   session first installs a stand-in `claude` that runs `sh`. Against the commit before the fleet (6),
   the build with both slices draws the chief's dock, its floating window, the top bar up to the load
   gauge and the steer bar pixel for pixel the same. The only differences are those two runs of one build
   have too: the load gauge, and the container's host name and random temporary folders in the shells'
   text, including where that text shows through the floating window's anti-aliased left edge.

### 4.7 S8

Eight commits, one per domain, then the mapping. The tests ran unchanged but one, which called the removed
`Desktop::open_file` and now sends `Command::OpenFile`.

- `atomics_ui::Command` is S7's `Intent`, recorded with `Client::send`. `Backend::send(&self, Command)` carries out a
  host's command. `Command::is_view` names the UI's own commands, the view hints and the dialogs: the platform
  carries those out itself and they never reach `send`.
- The desktop has one entry, `Desktop::send` (`app/commands.rs`, which replaces `app/intents.rs`). A view command
  writes the native model's view hints as before (`active`, `diff_root`, `diff_open`, `panel`, the tab,
  `plan_edit_open`), where ctl reads them, or opens a dialog on the window's client. Any other goes to
  `Local::send`, which runs it with the post and `State` method the desktop's code used (`Local::run`): a
  position becomes an id when the command is sent, and the id a position again when the action runs. The
  approval (`Decide`) and the steer bar (`Steer`, one operation per target) still go through
  `State::mutate_desktop`, the journaled path H4 added. `post`, `post_plan` and `post_session` are `State`'s now;
  two tests still post a closure to set up a race.
- The domains, and what each maps onto in atomics.v1 (`Command::remote`):

| Domain | Commands | atomics.v1 |
| --- | --- | --- |
| Approvals | `Decide` | `approval_decide` |
| Plans | `ApprovePlan` (with the plan's digest), `StartTask`, `DispatchReady`, `SetTaskStatus`, `IntegrateTask`, `IntegrateAccepted` (one `integrate` per accepted task), `ArchivePlan`, `ArchiveFinished`, `ClosePlan` as archived | `plan_command`: `approve`, `dispatch`, `update`, `integrate`, `archive` |
| | `EditBrief`, `ResumeTask`, `AcknowledgeRefs`, `SyncTickets`, `ReopenPlan`, `ClosePlan` as completed, `CleanUpPlan`, `ReloadPlanCleanup`, `RemovePlanBranches` | none yet |
| Review | `EditComments` (a `remove` for each comment gone, then an `add` for each new one, at the diff's revision), `SendReview` | `review_command` |
| | `RefreshDiff`, `OpenFile`, `LeaveFile`, `Edit` | none (a remote client reads files with `diff_get` and `file_get`) |
| Sessions | `Steer` (one per session), `VoiceToSession` (a steer), `StartAgent` (`start` in the folder), `CloseSession` | `session_command` |
| | `Resume`, `ResumeSession`, `RenameSession`, `CloseIdle`, `SeeSession`, `DismissMcp` | none yet |
| Projects and worktrees | `OpenProject`, `CloseProject`, `OpenWorkspace`, `SaveWorkspace`, `OpenCleanup`, `CleanupFetch`, `CleanupPrune`, `CleanupReload`, `RemoveWorktrees` | none yet |
| Chief and settings | `ToggleChief`, `SetChiefDock`, `SaveChiefPreferences`, `VoiceToChief`, `SelectTheme`, `CheckSandbox`, `RefreshConnections`, `ImportConnections`, `Connect`, `Disconnect`, `AddConnection`, `DecideProposal` | none yet |
| View | `Activate`, `OpenTaskSession`, `ShowTaskDiff`, `ShowRoot`, `OpenDiff`, `CloseDiff`, `ToggleDiff`, `SetPanel`, `SetView`, `ShowUngrouped`, `ShowPlan`, `ViewPlanHistory`, `TogglePlan`, `EditingPlan`, `ClosePlanTab`; the dialogs `NewSession`, `NewWorktree`, `Rename`, `PickProject`; since S7's chief, `HideChief`, `ResizeChief`, `Toast` | none: the client's own |

- `Command::remote(backend, op)` is the mapping the remote `Backend` (C5) uses. It answers `Remote::Send` with the
  mutations in order (empty when what the command names is gone), `Remote::Client` or `Remote::Unsupported`. It
  reads positions from the client's snapshot (`plan_row`, `session_row`, `board`) and two reads this slice adds:
  `Backend::plan_digest`, the approval scope that approving names, and `DiffSnapshot::revision`, which a new
  comment is anchored to. `op` gives each mutation its operation ID. `commands_map_onto_atomics_v1` checks the
  mapping over `Local` and sends the mapped approval through `State::mutate_desktop`.
- The desktop's own plan, review and session commands still run the `State` methods its code called, not
  `plan_command`, `review_command` or `session_command` through the journal: those differ in what the user sees
  (the board's toasts, the user rather than the chief as a ticket's author, no digest check). Only the approval
  and the steer were journaled before this slice, and they still are.
- Where work happens moved in a few places; what it does did not. The board's `TaskAction` is `ResumeTask`,
  `AcknowledgeRefs` and `ShowTaskDiff`. The approval banner marks a request as being decided on the click (it was
  right after the draw, in the same frame). The removals of the two cleanup dialogs moved into
  `State::remove_plan_branches` and `State::remove_worktrees`, which read the dialog's rows, channel and plan when
  the action runs, on the next runtime tick, instead of at the click. `StartAgent` fills `pending_open` from an
  action, which the runtime tick drains before it spawns what is pending. Shift+arrows over a pull request's diff
  step the view before they open the panel.
- The desktop's shortcuts, tiles and editor bar send view commands instead of writing `runtime.diff_open`,
  `runtime.panel` and `runtime.active`. The writes left are `Desktop::send`'s, `view_plan_history` (which carries
  out `ViewPlanHistory`) and the per-frame reset of `plan_edit_open`. Code that has not moved yet switches
  panels through `State` methods (the Tickets panel, GitHub, the trace lane); those become commands as it moves.
- A trap for later slices: `Desktop` derefs to `State`, which has methods named like the removed wrappers
  (`close`, `open_file`, `send_review`, `open_project`, `set_chief_dock`, `save_chief_preferences`,
  `task_action`). A call left behind would compile and run at once instead of being posted; making the wrapper
  private does not help, since method lookup then goes on to `State`'s. Every call site sends a command now.

### 4.8 S9

- `crates/atomics-web` (C5) runs the moved draw functions in a browser: `board::draw`, `approvals::banner` and
  `diff_panel::draw` over `RemoteBackend`, a `Backend` on `atomics_core::mirror` fed by `atomics.v1`. Its writes are
  `Command::remote`'s mutations, sent only where `welcome`'s `may` grants them; the browser offers `Decide` and
  `ApprovePlan` in this release and says so for the rest. A view command stays in the frame (`views.rs`), as
  `Desktop::send` keeps it in the desktop.
- What `RemoteBackend` cannot know reads as empty or default: the host's config (`ui_config` is the defaults),
  projects, worktrees, recent sessions, the sandbox and the ticket config. Positions are the mirror's order (by
  id). A board's `Plan` is built from `PlanInfo`, so briefs and acceptance are not on the cards; `plan_digest` is
  the host's `PlanInfo.digest`, never recomputed in the browser.
- The diff panel reads each worktree's `DiffSummary` and fills in hunks as `diff_get` brings them; no spans travel,
  so the browser draws diffs without syntax colours (section 6).
- CI checks `atomics-ui` and `atomics-web` for `wasm32-unknown-unknown` and lints `atomics-web` there, since its
  browser glue (`src/web.rs`) does not build natively. Section 5's test covers both crates.

## 5. The dependency test

`crates/atomics-ui/tests/dependencies.rs` runs `cargo tree --locked` for `atomics-ui` and `atomics-web`, for the host target and for
`wasm32-unknown-unknown` (metadata only, so no wasm toolchain is needed) and fails if the tree contains `atomics`,
`syntect`, `onig`, `two-face`, `alacritty_terminal`, `cpal`, `rfd`, `notify`, `ureq`, `fontique`, `ring`, `tempfile`,
`dirs` or `libc`, or if `atomics-core` is present with its `native` feature. One exception, on the host target
only: egui's locks (`parking_lot_core`) and the Noise channel's randomness (`getrandom`) use `libc` on Unix, so
the host tree may reach `libc` through them and nothing else; the wasm32 tree may not contain it at all. A second test checks the list against
`atomics`'s own tree so a rename of a crate cannot make the first test vacuous. Add a crate to the list when the web
crate's reviewers find another one the browser cannot use.

## 6. The highlighter for wasm

**Decision: the web client does not highlight.** The host already colors lines (`Highlighter::file` and `lines` in
`highlight.rs`) and `Prepared` / `FileView` carry the spans (byte ranges and RGBA, `daemon.md:263-268`); the protocol sends
those. The web crate therefore needs no grammar set, and syntect stays on the host with onig, where
`native_highlighting_uses_only_onig` keeps checking it. That also keeps the grammar set out of the wasm bundle (its size is not yet measured).

**Fallback if a host-less path ever needs highlighting in the browser** (no such path is planned): syntect with
`regex-fancy` (`default-fancy`, via two-face `syntect-fancy`). It is pure Rust, uses the same grammars and themes, so its
spans are identical to the host's, and it needs no new dependency family. Pure-Rust alternatives (`synoptic`,
tree-sitter) change the grammars and so the colors, and tree-sitter is C. This choice is a recommendation until it is measured.

**Measurements** (2026-11-02, `scripts/highlight-bench/measure.sh`, release, `opt-level = "z"`, 3 shared CPUs, five
sample files: `app.rs` 1,851 lines, `config.rs` 1,301, `README.md` 749, two small; times are the mean of five runs after a
warm-up):

| Engine | Startup (grammars + theme) | ms per 1,000 lines | Native binary | wasm32 |
| --- | --- | --- | --- | --- |
| onig (desktop today) | 11.6 ms | 36.3 | 2.09 MB | does not build (C) |
| fancy-regex | 11.0 ms | 168.3 (4.6× slower) | 3.14 MB | 2.31 MB raw, 1.34 MB gzip |

Per file, fancy / onig: `app.rs` 270 / 75 ms, `config.rs` 150 / 39 ms, `README.md` 241 / 27 ms (Markdown is 9× slower).
The wasm time was **not** measured (no browser here); wasm usually runs slower than native, so expect more than the native
fancy column. The wasm size is for the highlighter alone, without egui.

**What this settles:** fancy-regex is a working pure-Rust engine with identical grammars, but it costs about 1.3 MB gzip
and several times the CPU, in a browser on a phone. That confirms the decision above: the web client receives spans from the
host and carries no highlighter. Keep `syntect` with `regex-fancy` only as a documented fallback, to be measured in a
browser if a host-less path is ever planned.
