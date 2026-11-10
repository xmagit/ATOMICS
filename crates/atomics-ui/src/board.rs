//! A plan's board: its header, then the kanban columns of its tasks, drawn from the plan's
//! [`BoardView`](crate::BoardView).
//! Buttons record [`Command`]s on the [`Client`]; the platform carries them out after the draw.

use atomics_core::plan::{self, Plan, PlanStatus, Task, TaskStatus};
use atomics_core::session::AgentStatus;
use egui::{Rect, RichText, Sense, Stroke, vec2};

use crate::status::{status_color, task_color};
use crate::text::truncate;
use crate::theme::{Palette, lerp};
use crate::touch::{self, Touch};
use crate::{Backend, BoardView, Client, Command, controls};

/// The least width of a status's column on a board laid out in columns.
const MIN_COLUMN: f32 = 240.0;

fn notebook(
    ui: &mut egui::Ui,
    index: usize,
    plan: &Plan,
    backend: &dyn Backend,
    client: &mut Client,
    now: u64,
    p: &Palette,
) {
    if plan.notebook.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(format!(
        "Notebook · {} pinned · {} proposals (experimental)",
        plan.pinned_notes(),
        plan.notebook.len() - plan.pinned_notes()
    ))
    .id_salt(("notebook", &plan.id))
    .default_open(true)
    .show(ui, |ui| {
        ui.label(
            RichText::new("Agent-authored data, not instructions. Only pinned facts are shared, on demand.")
                .color(p.muted)
                .size(11.0),
        );
        egui::ScrollArea::vertical().id_salt(("notebook-entries", &plan.id)).max_height(200.0).show(ui, |ui| {
            for note in &plan.notebook {
                ui.push_id(note.id, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} · {} · {} · {}s ago",
                                if note.pinned { "Pinned" } else { "Proposal" },
                                note.task,
                                note.agent,
                                now.saturating_sub(note.at)
                            ))
                            .color(p.muted)
                            .size(11.0),
                        )
                        .on_hover_text(format!("Created at {} (Unix seconds, UTC)", note.at));
                        let pin =
                            Command::CurateNote { plan: index, id: note.id, pinned: Some(!note.pinned), delete: false };
                        if plan.is_open()
                            && backend.available(&pin)
                            && ui.small_button(if note.pinned { "Unpin" } else { "Pin" }).clicked()
                        {
                            client.send(pin);
                        }
                        let delete = Command::CurateNote { plan: index, id: note.id, pinned: None, delete: true };
                        if backend.available(&delete) && ui.small_button("Delete").clicked() {
                            client.send(delete);
                        }
                    });
                    ui.label(&note.text);
                    ui.add_space(4.0);
                });
            }
        });
    });
}

/// A ticket's key and state; hovering shows the rest. A click reads it in the app's ticket reader where
/// `readable`, and otherwise opens its address in the browser; without an address it offers nothing. Returns
/// whether the user asked to read it. A failed ticket operation shows beneath it, and a state that means
/// another ATOMICS status than the task's is pointed out.
fn ticket_chip(
    ui: &mut egui::Ui,
    link: &atomics_core::tickets::TicketLink,
    means: Option<(TaskStatus, TaskStatus)>,
    config: Option<&atomics_core::tickets::TicketConfig>,
    readable: bool,
    now: u64,
    p: &Palette,
) -> bool {
    let mismatch = config.and_then(|config| config.mismatch(link));
    let color = if link.error.is_some() || mismatch.is_some() { p.warning } else { p.info };
    // The configured system's reader cannot read a ticket of another system.
    let read = readable && mismatch.is_none();
    let browser_url = link.url.as_deref().and_then(crate::markdown::web_url);
    let acts = read || browser_url.is_some();
    let mut asked = false;
    label_row(ui, !readable && browser_url.is_none(), |ui| {
        let text = match &link.state {
            Some(state) => format!("{} · {state}", link.label()),
            None => link.label().to_string(),
        };
        let mut response = egui::Frame::new()
            .fill(lerp(p.raised, color, 0.12))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(6, 1))
            .show(ui, |ui| ui.add(egui::Label::new(RichText::new(text).color(color).size(11.0)).selectable(false)))
            .response
            // A key that offers nothing still takes its click, so the card under it does not act on it; it is no
            // keyboard stop.
            .interact(if acts { Sense::click() } else { Sense::CLICK });
        if acts {
            response = touch::reach(ui, response).on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        let response = response.on_hover_ui(|ui| {
            if let Some(title) = &link.title {
                ui.label(RichText::new(title).strong());
            }
            let system = if link.system.is_empty() { "ticket" } else { &link.system };
            ui.label(format!("{system} {}", link.id));
            if let Some(updated) = &link.updated_at {
                ui.label(format!("updated {updated}"));
            }
            // A client that knows only the ticket's key cannot say when its host read it.
            match link.synced_at {
                Some(at) => {
                    ui.label(format!("read {} min ago", now.saturating_sub(at) / 60));
                }
                None if config.is_some() => {
                    ui.label("not read yet");
                }
                None => {}
            }
            if let Some(error) = &link.error {
                ui.label(RichText::new(error).color(p.error));
            }
            let hint = match (read, browser_url) {
                (true, Some(_)) => Some("click to read it here, with Open in browser".to_string()),
                (true, None) => Some("click to read it here".to_string()),
                (false, Some(url)) => Some(format!("click to open {url}")),
                (false, None) => None,
            };
            if let Some(hint) = hint {
                ui.label(RichText::new(hint).color(p.muted));
            }
        });
        if response.clicked() {
            asked = read;
            if let Some(url) = browser_url.filter(|_| !read) {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
        if let Some((ticket, task)) = means.filter(|(ticket, task)| ticket != task) {
            ui.label(RichText::new(format!("ticket says {}", ticket.label())).color(p.warning).size(11.0))
                .on_hover_text(format!("The task is {}; ATOMICS leaves the ticket as it was set", task.label()));
        }
    });
    if let Some(mismatch) = mismatch {
        ui.label(RichText::new(format!("⚠ {mismatch}: not synced")).color(p.warning).size(11.0)).on_hover_text(
            "ATOMICS leaves tickets of another system alone. Link a ticket of the configured system instead.",
        );
    }
    if let Some(error) = &link.error {
        ui.label(RichText::new(format!("⚠ {}", truncate(error, 160))).color(p.error).size(11.0)).on_hover_text(error);
    }
    asked
}

/// `add` in a wrapping row. Where `label`, for a ticket's chip that offers nothing, the row is only as high as what
/// it holds, and the chip no higher than a label: it does not look like what can be pressed on a phone.
fn label_row(ui: &mut egui::Ui, label: bool, add: impl FnOnce(&mut egui::Ui)) {
    let height = ui.spacing().interact_size.y;
    if label {
        ui.spacing_mut().interact_size.y = 0.0;
    }
    ui.horizontal_wrapped(add);
    ui.spacing_mut().interact_size.y = height;
}

fn stage_chip(ui: &mut egui::Ui, stage: Option<&atomics_core::workflow::Stage>) {
    if let Some(stage) = stage {
        if let Some(url) = stage.url.as_deref().and_then(crate::markdown::web_url) {
            ui.hyperlink_to(stage.label(), url);
        } else {
            ui.label(stage.label());
        }
        if let Some(note) = &stage.note {
            ui.label(note);
        }
    }
}

/// The plan header's mode: what the plan does with its ticket. In auto mode the decision made
/// when the plan was created is named, and an undecided plan says so.
pub fn mode_label(plan: &Plan, config: &atomics_core::tickets::TicketConfig) -> String {
    use atomics_core::tickets::TicketMode;
    let mode = match plan.ticket_mode(config.mode) {
        TicketMode::Board => "board",
        TicketMode::Ticket => "ticket",
        TicketMode::Auto => return "auto (deciding…)".into(),
    };
    if config.mode == TicketMode::Auto { format!("{mode} (auto)") } else { mode.into() }
}

/// Board mode: how many of the plan's tasks follow a ticket, e.g. "4 of 6 tasks tracked".
pub fn tracked_label(plan: &Plan, config: &atomics_core::tickets::TicketConfig) -> Option<String> {
    if plan.ticket_mode(config.mode) != atomics_core::tickets::TicketMode::Board || plan.tasks.is_empty() {
        return None;
    }
    let tracked = plan.tasks.iter().filter(|task| task.ticket.is_some()).count();
    Some(format!("{tracked} of {} tasks tracked", plan.tasks.len()))
}

/// A task's result on its card: its completion or last report, its changes, and the `diff` and `resume`
/// buttons. Both need the task's worktree on the host (`available`); a `codex` agent resumes without a
/// saved id. `now` is the host's clock, in seconds. Returns the button clicked.
#[allow(clippy::too_many_arguments)]
pub fn task_result(
    ui: &mut egui::Ui,
    task: &Task,
    status: Option<AgentStatus>,
    expanded: bool,
    read_only: bool,
    available: bool,
    codex: bool,
    now: u64,
    p: &Palette,
) -> Option<&'static str> {
    task_result_for(ui, task, status, expanded, read_only, available, codex, now, p, None, TaskButtons::ALL)
}

/// Which of a task card's `diff` and `resume` buttons the backend can carry out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskButtons {
    pub diff: bool,
    pub resume: bool,
}

impl TaskButtons {
    const ALL: TaskButtons = TaskButtons { diff: true, resume: true };

    /// The buttons of the tasks of the plan at `plan` that `backend` offers.
    pub fn of(backend: &dyn Backend, plan: usize) -> TaskButtons {
        TaskButtons {
            diff: backend.available(&Command::ShowTaskDiff { plan, task: 0 }),
            resume: backend.available(&Command::ResumeTask { plan, task: 0 }),
        }
    }
}

/// [`task_result`] with its buttons sized for `touch`, and only those in `buttons`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn task_result_for(
    ui: &mut egui::Ui,
    task: &Task,
    status: Option<AgentStatus>,
    expanded: bool,
    read_only: bool,
    available: bool,
    codex: bool,
    now: u64,
    p: &Palette,
    touch: Option<Touch>,
    buttons: TaskButtons,
) -> Option<&'static str> {
    let result = |ui: &mut egui::Ui| {
        let mut action = None;
        if task.finished_at.is_some() {
            ui.label(
                RichText::new(task.completion_line().unwrap_or_else(|| "Agent completed".into()))
                    .color(p.success)
                    .size(12.0),
            );
        } else if matches!(status, Some(AgentStatus::Idle | AgentStatus::Done))
            && let Some(line) = task.reported_line(now)
        {
            ui.label(RichText::new(line).color(p.muted).size(12.0));
        }
        if let Some(pr) = &task.pull_request {
            ui.hyperlink_to(format!("PR #{} · {} → {}", pr.number, pr.head, pr.base), &pr.url);
        }
        if let Some(report) = &task.report {
            let summary = if expanded {
                report.summary.clone()
            } else {
                truncate(&report.summary.lines().take(3).collect::<Vec<_>>().join("\n"), 360)
            };
            // A shortened report keeps all of it a hover away, and an open card shows it whole.
            let shortened = summary != report.summary;
            let (summary, color) = if task.finished_at.is_some() {
                (summary, p.foreground)
            } else {
                let color = match report.status.as_str() {
                    "done" => p.success,
                    "blocked" | "question" => p.warning,
                    _ => p.info,
                };
                (format!("{}: {summary}", report.status), color)
            };
            let response = ui.label(RichText::new(summary).color(color).size(12.0));
            if shortened {
                response.on_hover_text(format!("{}\n\nClick the card's title to keep it open.", report.summary));
            }
        }
        if let Some(report) = &task.report
            && !report.checks.is_empty()
        {
            egui::CollapsingHeader::new(format!("Checks ({})", report.checks.len()))
                .id_salt(("report-checks", &task.id))
                .default_open(false)
                .show(ui, |ui| {
                    for check in &report.checks {
                        ui.label(RichText::new(&check.check).strong().size(12.0));
                        ui.label(RichText::new(&check.command).monospace().size(11.0));
                        let exit = check.exit.map_or_else(|| "no exit code".into(), |exit| format!("exit {exit}"));
                        ui.label(
                            RichText::new(exit)
                                .color(if check.exit == Some(0) { p.success } else { p.warning })
                                .size(11.0),
                        );
                        ui.label(RichText::new(&check.output_tail).monospace().size(11.0));
                    }
                });
        }
        if task.finished_at.is_some() {
            ui.label(RichText::new(format!("+{} −{}", task.additions, task.deletions)).color(p.muted).size(12.0));
            ui.horizontal(|ui| {
                if buttons.diff && ui.add_enabled(available, touch::sized(egui::Button::new("Diff"), touch)).clicked() {
                    action = Some("diff");
                }
                if !read_only
                    && buttons.resume
                    && ui
                        .add_enabled(
                            status.is_none() && available && (task.resume_id.is_some() || codex),
                            touch::sized(egui::Button::new("Resume"), touch),
                        )
                        .clicked()
                {
                    action = Some("resume");
                }
            });
        }
        action
    };
    when_drawn(ui, egui::Layout::top_down(egui::Align::Min), None, result)
}

/// `text` after a dot in `color`: the font's ● where it has one, else a painted dot, as for a phone's proportional
/// text, which has none.
fn dotted(ui: &mut egui::Ui, text: String, color: egui::Color32, size: f32) {
    let family = ui.style().text_styles.get(&egui::TextStyle::Body).map(|font| font.family.clone()).unwrap_or_default();
    let font = egui::FontId::new(size, family);
    if ui.fonts_mut(|fonts| fonts.has_glyph(&font, '●')) {
        ui.label(RichText::new(format!("● {text}")).color(color).size(size));
        return;
    }
    // A line of text high, as the label is, the dot in the middle of its first row.
    let (dot, gap) = (size * 0.6, size * 0.35);
    let row = ui.fonts_mut(|fonts| fonts.row_height(&font));
    let galley = ui.painter().layout(text, font, color, (ui.available_width() - dot - gap).max(dot));
    let (rect, _) = ui.allocate_exact_size(vec2(dot + gap + galley.size().x, galley.size().y), Sense::hover());
    ui.painter().circle_filled(egui::pos2(rect.left() + dot / 2.0, rect.top() + row / 2.0), size * 0.3, color);
    ui.painter().galley(egui::pos2(rect.left() + dot + gap, rect.top()), galley, color);
}

/// `add` in a child of `layout`, its rows `row` high if given, that takes room only once something is drawn
/// in it, so that a card keeps no gap for a part it has nothing of.
fn when_drawn<R>(ui: &mut egui::Ui, layout: egui::Layout, row: Option<f32>, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let mut rect = ui.available_rect_before_wrap();
    if let Some(row) = row {
        rect.max.y = rect.min.y + row;
    }
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    let inner = add(&mut child);
    let used = child.min_rect();
    if used.width() > 0.0 && used.height() > 0.0 {
        ui.advance_cursor_after_rect(used);
    }
    inner
}

/// The board of the plan at `index` of [`Backend::plan_list`]: its header, then the kanban columns, drawn
/// from the plan's [`BoardView`](crate::BoardView), read once per frame. A phone's board is one surface that
/// scrolls, its header going up with the cards under it.
pub fn draw(ui: &mut egui::Ui, rect: Rect, index: usize, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let touch = client.touch;
    ui.painter().rect_filled(rect, 0.0, p.background);
    let Some(board) = backend.board(index) else { return };
    let margin = touch.map_or(vec2(12.0, 8.0), |touch| vec2(touch.gutter, 8.0));
    let mut child =
        ui.new_child(egui::UiBuilder::new().id_salt(("plan-panel", &board.plan.id)).max_rect(rect.shrink2(margin)));
    if touch::stacks(touch) {
        egui::ScrollArea::vertical()
            .id_salt(("plan-board", board.plan.id.clone()))
            .auto_shrink([false, false])
            .show(&mut child, |ui| contents(ui, index, board, backend, client));
    } else {
        contents(&mut child, index, board, backend, client);
    }
}

/// The board's header, then its columns, or on a phone its statuses one under the other.
fn contents(ui: &mut egui::Ui, index: usize, mut board: BoardView, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let touch = client.touch;
    let now = board.now;
    let plan = &board.plan;
    let read_only = plan.status != PlanStatus::Open;
    let mut reopen = false;
    let mut archive = false;
    let mut approve = false;
    let mut dispatch_all = false;
    let mut integrate_accepted = false;
    let mut close_status = None;
    let mut clean_plan = false;
    let ready = board.ready.len();
    let total = plan.tasks.len();
    // The actions the backend can carry out; the others are not drawn.
    let can = |command: Command| backend.available(&command);
    let can_archive = can(Command::ArchivePlan(index));
    let can_clean_up = can(Command::CleanUpPlan(index));
    let can_reopen = can(Command::ReopenPlan(index));
    // The confirmation offers the ways the backend has.
    let can_close = can(Command::ClosePlan { plan: index, status: PlanStatus::Completed })
        || can(Command::ClosePlan { plan: index, status: PlanStatus::Archived });
    let can_approve = can(Command::ApprovePlan(index));
    let can_dispatch = can(Command::DispatchReady(index));
    let can_integrate_accepted = can(Command::IntegrateAccepted(index));
    let can_sync = can(Command::SyncTickets(index));
    let can_edit = can(Command::EditBrief { plan: index, task: String::new(), brief: String::new() });
    let can_start = can(Command::StartTask { plan: index, task: String::new() });
    let can_move = can(Command::SetTaskStatus { plan: index, task: String::new(), status: TaskStatus::Done });
    let can_integrate = can(Command::IntegrateTask { plan: index, task: String::new() });
    let can_open = can(Command::OpenTaskSession(String::new()));
    let can_diff = can(Command::ShowTaskDiff { plan: index, task: 0 });
    // The card of the session on screen is marked.
    let active = backend.view_hints().active;
    let can_acknowledge = can(Command::AcknowledgeRefs { plan: index, task: 0 });
    let can_read_ticket = can(Command::OpenTicket(Default::default()));
    let buttons = TaskButtons::of(backend, index);
    if board.checking {
        ui.label(RichText::new("Checking integration freshness…").color(p.muted));
    }
    // The plan's buttons: those that end or reopen it, and approving or dispatching it. The desktop puts them
    // right to left at the end of the header's row; a phone puts them in a row of their own, the main one first.
    let mut end_buttons = |ui: &mut egui::Ui| {
        if read_only {
            if plan.status == PlanStatus::Completed {
                archive = can_archive && touch::button(ui, touch, "Archive").clicked();
                clean_plan = can_clean_up && touch::button(ui, touch, "Clean up plan…").clicked();
            }
            reopen = can_reopen && touch::button(ui, touch, "Reopen plan").clicked();
        } else if can_close && touch::button(ui, touch, "Close plan…").clicked() {
            close_status = Some(PlanStatus::Completed);
        }
    };
    let mut main_buttons = |ui: &mut egui::Ui| {
        if read_only || total == 0 {
            return;
        }
        if !plan.approved {
            if !can_approve {
                return;
            }
            approve = controls::primary(ui, "Approve plan", touch, &p)
                .on_hover_text("Let the orchestrator dispatch these tasks")
                .clicked();
        } else {
            dispatch_all = can_dispatch
                && ui
                    .add_enabled(ready > 0, touch::sized(egui::Button::new(format!("Dispatch ready ({ready})")), touch))
                    .on_hover_text("Start every task whose dependencies are done")
                    .clicked();
            ui.label(RichText::new("Approved").color(p.success).size(12.0));
        }
    };
    ui.horizontal(|ui| {
        crate::sidebar::section(ui, &p, "PLAN");
        if total > 0 {
            let done = plan.count(TaskStatus::Done);
            ui.label(RichText::new(format!("{done}/{total} done")).color(p.muted).size(12.0));
        }
        if read_only {
            let status =
                if plan.status == PlanStatus::Completed { "Completed · read-only" } else { "Archived · read-only" };
            ui.label(RichText::new(status).color(p.muted).size(12.0));
        }
        if !touch::stacks(touch) {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                end_buttons(ui);
                main_buttons(ui);
            });
        }
    });
    if !plan.goal.is_empty() {
        ui.label(RichText::new(&plan.goal).color(board.state.color(&p)).size(13.0));
    }
    if let Some(request) = &plan.publish {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.heading("Publish pull request");
            ui.label(format!("{} · GitHub account: {}", request.repository, request.account));
            ui.label(format!("{} → {} · {}", request.branch, request.base, request.head_sha));
            ui.strong(&request.title);
            ui.label(&request.body);
            ui.monospace(&request.commits);
            if let Some(url) = &request.url {
                ui.hyperlink(url);
            } else {
                if let Some(error) = &request.error {
                    ui.colored_label(p.error, error);
                }
                ui.horizontal(|ui| {
                    for (label, approve) in [("Publish", true), ("Dismiss", false)] {
                        let command = Command::PublishDecision { plan: index, request: request.id.clone(), approve };
                        if ui.add_enabled(!read_only && backend.available(&command), egui::Button::new(label)).clicked()
                        {
                            client.send(command);
                        }
                    }
                });
            }
        });
    }
    let integration = plan.integration_branch();
    if !plan.tasks.is_empty() {
        ui.label(RichText::new(format!("integration branch: {integration}")).color(p.muted).size(11.0))
            .on_hover_text("Integrated tasks are merged into it, and tasks with dependencies branch from it");
    }
    if touch::stacks(touch) {
        ui.horizontal_wrapped(|ui| {
            main_buttons(ui);
            end_buttons(ui);
        });
    }
    let pending = plan.pending_integrations().len();
    if !read_only && pending > 0 {
        let row = |ui: &mut egui::Ui| {
            let tasks = if pending == 1 { "task" } else { "tasks" };
            ui.label(RichText::new(format!("{pending} accepted {tasks} not integrated yet")).color(p.warning));
            integrate_accepted =
                can_integrate_accepted && ui.add(touch::sized(egui::Button::new("Integrate"), touch)).clicked();
        };
        // On a phone the button goes under the words rather than off the screen.
        if touch::stacks(touch) {
            ui.horizontal_wrapped(row);
        } else {
            ui.horizontal(row);
        }
    }
    // Needing a sign-in shows where the tickets do, with the button that signs in; with no open plan the
    // sidebar shows it instead.
    if !crate::signin::no_board_shows(backend) {
        crate::signin::framed(ui, backend, client);
    }
    let ticket_config = &board.ticket_config;
    stage_chip(ui, plan.stages.last());
    let mut sync_tickets = false;
    let mut read_ticket = None;
    if let Some(link) = &plan.ticket {
        label_row(ui, !can_read_ticket && link.url.is_none(), |ui| {
            let system = ticket_config.as_ref().map_or(link.system.as_str(), |c| c.system.as_str());
            let what = ticket_config.as_ref().map_or("ticket".to_string(), |c| mode_label(plan, c));
            // A client that knows only the ticket's key has no system to name.
            ui.label(RichText::new(format!("{system} {what}").trim_start()).color(p.muted).size(11.0));
            let means = ticket_config
                .as_ref()
                .filter(|c| {
                    plan.ticket_mode(c.mode) == atomics_core::tickets::TicketMode::Ticket
                        && !plan.ticket_progress_expected(None)
                })
                .and_then(|c| c.states.status_of(link.state.as_deref()?))
                .map(|ticket| (ticket, atomics_core::tickets::plan_status(plan)));
            if ui.vertical(|ui| ticket_chip(ui, link, means, ticket_config.as_deref(), can_read_ticket, now, &p)).inner
            {
                read_ticket = Some(link.clone());
            }
            if let Some(title) = &link.title {
                ui.label(RichText::new(title).color(p.foreground).size(11.0));
            }
            if let Some(tracked) = ticket_config.as_ref().and_then(|c| tracked_label(plan, c)) {
                ui.label(RichText::new(tracked).color(p.muted).size(11.0));
            }
            if let Some((tickets, loose)) = board.board_counts {
                ui.label(
                    RichText::new(format!("{tickets} tickets · {loose} without a task"))
                        .color(if loose > 0 { p.warning } else { p.muted })
                        .size(11.0),
                );
            }
            if ticket_config.is_some() && can_sync {
                sync_tickets = touch::button(ui, touch, "Sync").on_hover_text("Read the tickets again now").clicked();
            }
        });
    }
    if plan.status == PlanStatus::Completed {
        ui.label(
            RichText::new("Completed · preview cleanup to check what is merged and safe to remove.").color(p.muted),
        );
        if clean_plan {
            client.send(Command::CleanUpPlan(index));
        }
    }
    for (task, dep) in plan.unknown_dependencies() {
        ui.label(RichText::new(format!("{task} depends on unknown task {dep}")).color(p.error).size(11.0));
    }
    notebook(ui, index, plan, backend, client, now, &p);
    ui.add_space(6.0);
    if plan.tasks.is_empty() {
        ui.label(
            RichText::new(
                "No plan yet. Ask an orchestrator (a Claude session with the ATOMICS MCP server) to plan a goal: \
                 its tasks appear here for you to review, edit and approve before any agent starts.",
            )
            .color(p.muted),
        );
    }

    let mut actions: Vec<(String, &str)> = Vec::new();
    let mut editing = false;
    let mut open_session: Option<String> = None;
    let editable = !read_only && !plan.approved && can_edit;
    let ready_ids = &board.ready;
    let approved = plan.approved;
    let sessions = &board.sessions;
    let expanded = &client.expanded_tasks;
    let integrating = &board.integrating;
    let plan_id = plan.id.clone();
    let ticket_states = ticket_config.as_ref().map(|c| c.states.clone());
    let latest_stages: std::collections::HashMap<_, _> = plan
        .tasks
        .iter()
        .filter_map(|task| plan.latest_stage(task).map(|stage| (task.id.clone(), stage.clone())))
        .collect();
    let integration_notes = &board.integration_notes;
    let (worktrees, codex) = (&board.worktrees, &board.codex);
    let expected_tasks: std::collections::HashSet<_> = plan
        .tasks
        .iter()
        .filter(|task| plan.ticket_progress_expected(Some(&task.id)))
        .map(|task| task.id.clone())
        .collect();
    // The cards edit briefs in place; an edit is recorded below.
    let mut edited_tasks = std::mem::take(&mut board.plan.tasks);
    let tasks = &mut edited_tasks;
    // One task's card.
    let mut card = |ui: &mut egui::Ui, task: &mut Task| {
        let color = task_color(task.status, &p);
        let open = expanded.contains(&(plan_id.clone(), task.id.clone()));
        let live = task.session.as_ref().filter(|s| sessions.contains_key(*s)).cloned();
        let on_screen = live.is_some() && live == active;
        // A click on the card, outside its buttons and its title, shows the task's session and its diff, or a
        // finished task's diff; the title opens and closes the card.
        let focus = match &live {
            Some(session) if can_open => Some((task.id.clone(), "session", Some(session.clone()))),
            None if can_diff && buttons.diff && worktrees.contains(&task.id) => Some((task.id.clone(), "diff", None)),
            _ => None,
        };
        let stroke = if on_screen {
            Stroke::new(1.5, p.accent)
        } else {
            Stroke::new(1.0, if open { color.gamma_multiply(0.6) } else { p.border })
        };
        let sense = if focus.is_some() { Sense::click() } else { Sense::hover() };
        let scope = ui.scope_builder(egui::UiBuilder::new().sense(sense), |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            egui::Frame::new()
                .fill(if on_screen { lerp(p.raised, p.accent, 0.07) } else { p.raised })
                .stroke(stroke)
                .corner_radius(crate::theme::RADIUS)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    // Columns justify their children; card text needs ordinary word spacing.
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        ui.set_width(ui.available_width());
                        let title = ui.add(
                            egui::Label::new(RichText::new(&task.title).color(p.foreground).strong())
                                .sense(Sense::click()),
                        );
                        if touch::reach(ui, title).clicked() {
                            actions.push((task.id.clone(), "toggle"));
                        }
                        let mut meta =
                            format!("{} · {}", task.id, if task.agent.is_empty() { "claude" } else { &task.agent });
                        if let Some(model) =
                            task.args.windows(2).find(|w| w[0] == "-m" || w[0] == "--model").map(|w| w[1].clone())
                        {
                            meta.push_str(&format!(" {model}"));
                        }
                        if let Some(isolation) = task.isolation {
                            meta.push_str(&format!(" · {}", isolation.name()));
                        }
                        ui.label(RichText::new(meta).color(p.muted).size(11.0));
                        if let Some(reason) = &task.reviewer_reason {
                            ui.label(RichText::new(reason).color(p.muted).size(11.0));
                        }
                        stage_chip(ui, latest_stages.get(&task.id));
                        if let Some(link) = &task.ticket {
                            let means = ticket_states
                                .as_ref()
                                .filter(|_| !expected_tasks.contains(&task.id))
                                .and_then(|s| s.status_of(link.state.as_deref()?))
                                .map(|ticket| (ticket, task.status));
                            if ticket_chip(ui, link, means, ticket_config.as_deref(), can_read_ticket, now, &p) {
                                read_ticket = Some(link.clone());
                            }
                        }
                        if let Some(branch) = &task.branch {
                            let branch = RichText::new(branch).color(p.muted).size(11.0);
                            // A phone shows names like this one in monospace, its other text in the UI's font.
                            let branch = if touch::stacks(touch) { branch.monospace() } else { branch };
                            ui.add(egui::Label::new(branch).truncate());
                        }
                        if let Some((note, checking)) = integration_notes.get(&task.id) {
                            let color = if *checking { p.muted } else { p.warning };
                            ui.label(RichText::new(note).color(color).size(11.0));
                        }
                        if !task.depends_on.is_empty() {
                            ui.label(
                                RichText::new(format!("after {}", task.depends_on.join(", ")))
                                    .color(p.muted)
                                    .size(11.0),
                            );
                        }
                        if let Some((title, agent_status)) = task.session.as_ref().and_then(|s| sessions.get(s)) {
                            let color = status_color(*agent_status, &p);
                            dotted(ui, format!("{title} · {}", agent_status.label()), color, 11.0);
                        }
                        let status = task.session.as_ref().and_then(|s| sessions.get(s)).map(|(_, s)| *s);
                        if let Some(action) = task_result_for(
                            ui,
                            task,
                            status,
                            open,
                            read_only,
                            worktrees.contains(&task.id),
                            codex.contains(&task.id),
                            now,
                            &p,
                            touch,
                            buttons,
                        ) {
                            actions.push((task.id.clone(), action));
                        }
                        if let Some(alert) = &task.ref_alert {
                            egui::Frame::new()
                                .fill(lerp(p.panel, p.error, 0.15))
                                .stroke(Stroke::new(1.0, p.error))
                                .inner_margin(6)
                                .show(ui, |ui| {
                                    ui.label(RichText::new(alert).color(p.error).strong().size(12.0));
                                    if can_acknowledge && touch::button(ui, touch, "Acknowledge ref alert").clicked() {
                                        actions.push((task.id.clone(), "acknowledge_refs"));
                                    }
                                });
                        } else if open {
                            // An open card shows every note, oldest first.
                            for note in &task.notes {
                                ui.label(RichText::new(note).color(p.muted).italics().size(11.0));
                            }
                        } else if let Some(note) = task.notes.last() {
                            let response = ui.label(RichText::new(note).color(p.muted).italics().size(11.0));
                            let earlier = task.notes.len() - 1;
                            if earlier > 0 {
                                let plural = if earlier == 1 { "" } else { "s" };
                                let more = format!("{earlier} earlier note{plural} · click the title");
                                let hint = ui.label(RichText::new(more).color(p.quiet).size(10.5));
                                let all = task.notes.join("\n\n");
                                response.on_hover_text(&all);
                                hint.on_hover_text(all);
                            }
                        }
                        if open {
                            ui.add_space(4.0);
                            if editable && task.status == TaskStatus::Planned {
                                let edit = egui::TextEdit::multiline(&mut task.brief)
                                    .desired_rows(4)
                                    .desired_width(f32::INFINITY);
                                let response = ui.add(edit);
                                editing |= response.has_focus();
                                if response.changed() {
                                    actions.push((task.id.clone(), "edited"));
                                }
                            } else {
                                ui.label(RichText::new(&task.brief).color(p.foreground).size(12.0));
                            }
                            for check in &task.acceptance {
                                controls::marked_line(
                                    ui,
                                    crate::status::Mark::Check,
                                    p.muted,
                                    RichText::new(check).color(p.muted).size(11.0),
                                );
                            }
                        }
                        let buttons = |ui: &mut egui::Ui| {
                            if can_open
                                && task.session.as_ref().is_some_and(|s| sessions.contains_key(s))
                                && touch::button(ui, touch, "Open")
                                    .on_hover_text("Show this task's agent and its diff")
                                    .clicked()
                            {
                                open_session = task.session.clone();
                            }
                            if read_only {
                                return;
                            }
                            match task.status {
                                TaskStatus::Planned if approved && ready_ids.contains(&task.id) && can_start => {
                                    if touch::button(ui, touch, "Start")
                                        .on_hover_text("Start this task's agent")
                                        .clicked()
                                    {
                                        actions.push((task.id.clone(), "start"));
                                    }
                                }
                                TaskStatus::Review if can_move => {
                                    if touch::button(ui, touch, "Accept").on_hover_text("Mark this task done").clicked()
                                    {
                                        actions.push((task.id.clone(), "done"));
                                    }
                                    if touch::button(ui, touch, "Send back")
                                        .on_hover_text("Back to running; steer it in the steer bar")
                                        .clicked()
                                    {
                                        actions.push((task.id.clone(), "running"));
                                    }
                                }
                                TaskStatus::Blocked if can_move => {
                                    if touch::button(ui, touch, "Unblock").on_hover_text("Back to running").clicked() {
                                        actions.push((task.id.clone(), "running"));
                                    }
                                }
                                TaskStatus::Done
                                    if can_move
                                        && task.session.as_ref().is_some_and(|s| sessions.contains_key(s))
                                        && touch::button(ui, touch, "Reopen")
                                            .on_hover_text("Back to review")
                                            .clicked() =>
                                {
                                    actions.push((task.id.clone(), "review"));
                                }
                                _ => {}
                            }
                            if can_integrate
                                && task.id != plan::REVIEW_TASK
                                && task.branch.is_some()
                                && matches!(task.status, TaskStatus::Review | TaskStatus::Done)
                            {
                                let busy = integrating.contains(&task.id);
                                let label = if busy { "Integrating…" } else { "Integrate" };
                                if ui
                                    .add_enabled(!busy, touch::sized(egui::Button::new(label), touch))
                                    .on_hover_text(format!("Commit its changes and merge it into {integration}"))
                                    .clicked()
                                {
                                    actions.push((task.id.clone(), "integrate"));
                                }
                            }
                        };
                        // The row is as tall as its buttons, and takes no room without any.
                        let row = egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true);
                        let height = touch.map_or(ui.spacing().interact_size.y, |touch| touch.target);
                        when_drawn(ui, row, Some(height), buttons);
                    });
                });
        });
        if let Some((id, action, session)) = focus {
            let response = scope.response.on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.has_focus() {
                let ring = Stroke::new(2.0, ui.visuals().selection.stroke.color);
                ui.painter().rect_stroke(response.rect.expand(1.0), 4.0, ring, egui::StrokeKind::Outside);
            }
            if response.clicked() {
                match session {
                    Some(session) => open_session = Some(session),
                    None => actions.push((id, action)),
                }
            }
        }
    };
    // A status's heading, over its tasks.
    let heading = |ui: &mut egui::Ui, status: TaskStatus, count: usize| {
        ui.label(
            RichText::new(format!("{} · {count}", status.label().to_uppercase()))
                .color(task_color(status, &p))
                .strong()
                .size(10.5),
        );
    };
    // A panel narrower than all the columns scrolls them sideways; only a panel too narrow for one whole
    // column stacks the statuses as a phone does, so that no card is cut at the panel's edge.
    let columns_fit = ui.available_width() >= MIN_COLUMN;
    if !tasks.is_empty() && !touch::stacks(touch) && columns_fit {
        egui::ScrollArea::horizontal().id_salt(("plan-board", &plan_id)).auto_shrink([false, false]).show(ui, |ui| {
            let columns = TaskStatus::ALL.len();
            let spacing = ui.spacing().item_spacing.x * (columns - 1) as f32;
            ui.set_width(ui.available_width().max(MIN_COLUMN * columns as f32 + spacing));
            ui.columns(TaskStatus::ALL.len(), |columns| {
                for (column, status) in columns.iter_mut().zip(TaskStatus::ALL) {
                    let count = tasks.iter().filter(|t| t.status == status).count();
                    heading(column, status, count);
                    column.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("plan-column", &plan_id, status.label()))
                        .auto_shrink([false, false])
                        .show(column, |ui| {
                            for task in tasks.iter_mut().filter(|t| t.status == status) {
                                card(ui, task);
                                ui.add_space(6.0);
                            }
                        });
                }
            });
        });
    } else if !tasks.is_empty() {
        // A phone, or a panel too narrow for one column, stacks the statuses that have tasks, one under the
        // other, each card the panel's width. A phone's whole board scrolls already; a panel's statuses scroll here.
        let mut statuses = |ui: &mut egui::Ui| {
            for status in TaskStatus::ALL {
                let count = tasks.iter().filter(|t| t.status == status).count();
                if count == 0 {
                    continue;
                }
                ui.add_space(4.0);
                heading(ui, status, count);
                ui.add_space(2.0);
                for task in tasks.iter_mut().filter(|t| t.status == status) {
                    card(ui, task);
                    ui.add_space(8.0);
                }
            }
        };
        if touch::stacks(touch) {
            statuses(ui);
        } else {
            egui::ScrollArea::vertical()
                .id_salt(("plan-board", &plan_id))
                .auto_shrink([false, false])
                .show(ui, |ui| statuses(ui));
        }
    }

    if editing {
        client.send(Command::EditingPlan);
    }
    if let Some(link) = read_ticket {
        client.send(Command::OpenTicket(link));
    }
    for (id, action) in actions {
        match action {
            "toggle" => {
                let key = (plan_id.clone(), id);
                if !client.expanded_tasks.remove(&key) {
                    client.expanded_tasks.insert(key);
                }
            }
            "edited" => {
                if let Some(edited) = edited_tasks.iter().find(|t| t.id == id) {
                    client.send(Command::EditBrief { plan: index, brief: edited.brief.clone(), task: id });
                }
            }
            "start" => client.send(Command::StartTask { plan: index, task: id }),
            "integrate" => client.send(Command::IntegrateTask { plan: index, task: id }),
            "diff" | "resume" | "acknowledge_refs" => {
                if let Some(command) =
                    edited_tasks.iter().position(|t| t.id == id).and_then(|task| Command::task(index, task, action))
                {
                    client.send(command);
                }
            }
            status => {
                if let Some(status) = TaskStatus::parse(status) {
                    client.send(Command::SetTaskStatus { plan: index, task: id, status });
                }
            }
        }
    }
    if approve {
        client.send(Command::ApprovePlan(index));
    }
    if integrate_accepted {
        client.send(Command::IntegrateAccepted(index));
    }
    if sync_tickets {
        client.send(Command::SyncTickets(index));
    }
    if dispatch_all {
        client.send(Command::DispatchReady(index));
    }
    if let Some(session) = open_session {
        client.send(Command::OpenTaskSession(session));
    }
    if reopen {
        client.send(Command::ReopenPlan(index));
    }
    if archive {
        client.send(Command::ArchivePlan(index));
    }
    if let Some(status) = close_status {
        client.confirm_plan_close = Some((plan_id, status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::tickets::{TicketConfig, TicketLink, TicketMode};

    #[test]
    fn plan_with_chief_names_the_auto_decision_in_the_board_header() {
        let auto = TicketConfig { system: "demo".into(), mode: TicketMode::Auto, ..Default::default() };
        let mut plan = Plan { ticket_mode: Some(TicketMode::Board), ..Default::default() };
        assert_eq!(mode_label(&plan, &auto), "board (auto)");
        assert_eq!(tracked_label(&plan, &auto), None);
        plan.tasks = vec![
            Task { ticket: Some(TicketLink { id: "1".into(), ..Default::default() }), ..Default::default() },
            Task::default(),
        ];
        assert_eq!(tracked_label(&plan, &auto).as_deref(), Some("1 of 2 tasks tracked"));
        plan.ticket_mode = Some(TicketMode::Ticket);
        assert_eq!(mode_label(&plan, &auto), "ticket (auto)");
        assert_eq!(tracked_label(&plan, &auto), None);
        plan.ticket_mode = None;
        assert_eq!(mode_label(&plan, &auto), "auto (deciding…)");
        let forced = TicketConfig { mode: TicketMode::Board, ..auto };
        assert_eq!(mode_label(&Plan::default(), &forced), "board");
    }
}
