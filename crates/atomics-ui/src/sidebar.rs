//! The sidebar's views: **New work**, the BOARDS with the sessions of each and the closed ones to resume, the AGENTS
//! list, and the EXPLORER of the open projects, their worktrees and files. AGENTS and the EXPLORER open folded, and
//! each window remembers which it opened ([`crate::Folds`]). The sidebar around them is [`crate::shell::sidebar`],
//! with the host's TICKETS and USAGE. A menu entry or button whose command the backend cannot carry out is left out.

use std::path::PathBuf;

use atomics_core::plan::PlanStatus;
use egui::{FontId, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::status::{frame_dot, status_color};
use crate::theme::{Palette, lerp};
use crate::{Backend, Client, Command, PlanRow, RecentSession, SessionRow, diff_view};

pub fn section(ui: &mut egui::Ui, p: &Palette, title: &str) {
    ui.horizontal(|ui| {
        // Painted rather than a "▍" glyph, which proportional UI fonts lack.
        let (bar, _) = ui.allocate_exact_size(vec2(3.0, 12.0), Sense::hover());
        ui.painter().rect_filled(bar, 0.0, p.accent);
        ui.label(RichText::new(title).size(10.5).color(p.muted).strong());
    });
    ui.add_space(1.0);
}

/// A session's row; `number` is the N of its Ctrl+N, for the first nine of the tab.
fn session_row(
    ui: &mut egui::Ui,
    p: &Palette,
    s: &SessionRow,
    active: bool,
    selected: bool,
    number: Option<usize>,
) -> egui::Response {
    // A finger's size where the controls have one.
    let height = ui.spacing().interact_size.y.max(38.0);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let painter = ui.painter();
    // What needs the user stands out whether or not it is the active one: a bar in the warning color.
    let needs = s.needs_user();
    if active {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.accent, 0.14));
    } else if needs {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.warning, 0.08));
    }
    crate::controls::feedback(ui, rect, &response, 2.0, p);
    if active || needs {
        let bar = if needs { p.warning } else { p.accent };
        painter.rect_filled(rect.with_max_x(rect.left() + 2.0), 0.0, bar);
    }
    if active && needs {
        painter.rect_filled(rect.with_min_x(rect.right() - 2.0), 0.0, p.accent);
    }
    painter.circle_filled(pos2(rect.left() + 10.0, rect.top() + 13.0), 3.5, frame_dot(ui, s.status, p));
    // What Ctrl+N goes to, whatever the groups' order: under the dot, where it moves nothing else.
    if let Some(number) = number {
        let at = pos2(rect.left() + 10.0, rect.top() + 21.0);
        painter.text(at, egui::Align2::CENTER_TOP, number.to_string(), FontId::monospace(10.5), p.muted);
    }
    let clip = painter.with_clip_rect(rect.shrink2(vec2(2.0, 0.0)));
    // The diff stats keep their corner; a long title is cut before them.
    let stats_left = match s.changes {
        Some((add, del)) => clip
            .text(
                pos2(rect.right() - 6.0, rect.top() + 5.0),
                egui::Align2::RIGHT_TOP,
                format!("+{add} −{del}"),
                FontId::monospace(10.5),
                p.muted,
            )
            .left(),
        None => rect.right(),
    };
    painter.with_clip_rect(rect.shrink2(vec2(2.0, 0.0)).with_max_x(stats_left - 6.0)).text(
        pos2(rect.left() + 20.0, rect.top() + 5.0),
        egui::Align2::LEFT_TOP,
        &s.title,
        p.ui(13.0),
        if active { p.accent } else { p.foreground },
    );
    let mut detail = if s.scratch { format!("{} · scratch", s.agent) } else { s.agent.clone() };
    if s.outside {
        detail.push_str(" · outside");
    }
    let detail =
        clip.text(pos2(rect.left() + 20.0, rect.top() + 21.0), egui::Align2::LEFT_TOP, detail, p.ui(11.0), p.muted);
    // The status in its own color after the agent, and what waits on the user.
    let mut state = format!(" · {}", s.status_label());
    if s.pending_approvals > 0 {
        let n = s.pending_approvals;
        state.push_str(&format!(" · {n} approval{}", if n == 1 { "" } else { "s" }));
    }
    let state_color = if needs { p.warning } else { status_color(s.status, p) };
    let state = clip.text(detail.right_top(), egui::Align2::LEFT_TOP, state, p.ui(11.0), state_color);
    if selected {
        clip.text(state.right_top(), egui::Align2::LEFT_TOP, " · selected", p.ui(11.0), p.accent);
    }
    response
}

/// A group's heading in the AGENTS list: the plan its sessions work for, in the plan's color; a click shows
/// the plan where the backend can.
fn group_heading(ui: &mut egui::Ui, p: &Palette, text: &str, color: egui::Color32, hover: &str) -> egui::Response {
    ui.add_space(4.0);
    let label =
        egui::Label::new(RichText::new(text).font(p.ui(11.0)).color(color).strong()).truncate().sense(Sense::click());
    let response = ui.add(label);
    crate::controls::underline(ui, &response, color);
    response.on_hover_text(hover)
}

/// A section that opens folded: its title with a chevron and, folded, how many it holds; `extra` draws on its right.
/// A click on it opens or folds it. Returns whether it is open.
pub fn fold(
    ui: &mut egui::Ui,
    p: &Palette,
    title: &str,
    count: Option<usize>,
    open: &mut bool,
    extra: impl FnOnce(&mut egui::Ui),
) -> bool {
    ui.horizontal(|ui| {
        let text = match count.filter(|_| !*open) {
            Some(count) => format!("{title} · {count}"),
            None => title.to_string(),
        };
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width().min(180.0), 22.0), Sense::click());
        crate::controls::feedback(ui, rect.expand2(vec2(2.0, 0.0)), &response, 4.0, p);
        let painter = ui.painter();
        let chevron = Rect::from_center_size(pos2(rect.left() + 5.0, rect.center().y), vec2(9.0, 9.0));
        if *open {
            crate::controls::chevron(painter, chevron, p.muted);
        } else {
            let c = chevron.center();
            let points = vec![pos2(c.x - 2.0, c.y - 4.0), pos2(c.x + 2.0, c.y), pos2(c.x - 2.0, c.y + 4.0)];
            painter.add(egui::Shape::line(points, Stroke::new(1.4, p.muted)));
        }
        painter.text(pos2(rect.left() + 16.0, rect.center().y), egui::Align2::LEFT_CENTER, text, p.ui(10.5), p.muted);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, title));
        crate::flow::record(ui, &format!("fold:{title}"), response.rect);
        if response.clicked() {
            *open = !*open;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), extra);
    });
    *open
}

/// **New work**: back to the start screen, where the chief takes the next thing to do. Marked while it shows.
pub fn new_work(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let shown = client.home.shown;
    let ink = crate::simple::look::Ink::of(&p);
    let text = crate::simple::look::text("New work", 13.5, crate::simple::look::Weight::Medium, ink.primary);
    let content = match crate::icons::text(crate::icons::NEW, 15.0) {
        Some(icon) => egui::Atoms::new((icon.color(ink.secondary), text)),
        None => egui::Atoms::new(text),
    };
    let button = egui::Button::new(content)
        .fill(if shown { lerp(p.sidebar, p.foreground, 0.07) } else { p.raised })
        .stroke(if shown { Stroke::new(1.0, p.accent.gamma_multiply(0.6)) } else { Stroke::NONE })
        .corner_radius(9)
        .min_size(vec2(ui.available_width(), 32.0));
    let response = ui.add(button);
    crate::flow::record(ui, "new-work", response.rect);
    if response.on_hover_text("Start new work with the chief").clicked() {
        crate::home::new_work(backend, client);
    }
}

/// The height of a session's row under its board.
const SESSION_ROW: f32 = 24.0;

/// A session under its board, compact: its status dot, its title, and its state or its Ctrl+N number on the right.
/// A closed one (`record`) is quiet: a hollow dot and "closed", and a click resumes it.
fn session_line(
    ui: &mut egui::Ui,
    p: &Palette,
    title: &str,
    status: Option<(&SessionRow, bool, Option<usize>)>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), SESSION_ROW), Sense::click());
    let painter = ui.painter();
    let ink = crate::simple::look::Ink::of(p);
    let active = status.is_some_and(|(_, active, _)| active);
    if active {
        painter.rect_filled(rect, 6.0, lerp(p.sidebar, p.accent, 0.12));
    }
    crate::controls::feedback(ui, rect, &response, 6.0, p);
    let dot = pos2(rect.left() + 38.0, rect.center().y);
    let right = match status {
        Some((session, _, number)) => {
            painter.circle_filled(dot, 3.0, frame_dot(ui, session.status, p));
            let (word, color) = if session.needs_user() {
                (session.status_label().to_string(), p.warning)
            } else {
                (number.map_or_else(|| session.status_label().to_string(), |n| n.to_string()), ink.tertiary)
            };
            let font = if number.is_some() && !session.needs_user() {
                FontId::monospace(10.5)
            } else {
                crate::flow::font(11.5)
            };
            painter
                .text(pos2(rect.right() - 6.0, rect.center().y), egui::Align2::RIGHT_CENTER, word, font, color)
                .left()
        }
        None => {
            painter.circle_stroke(dot, 3.0, Stroke::new(1.0, p.quiet));
            painter
                .text(
                    pos2(rect.right() - 6.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    "closed",
                    crate::flow::font(11.5),
                    ink.tertiary,
                )
                .left()
        }
    };
    let color = match status {
        Some(_) if active => p.accent,
        Some(_) => ink.secondary,
        None => ink.tertiary,
    };
    let title = crate::flow::truncated(painter, title, crate::flow::font(12.5), color, (right - dot.x - 18.0).max(1.0));
    painter.galley(pos2(dot.x + 9.0, rect.center().y - title.size().y / 2.0), title, color);
    response
}

/// What a live session's menu can do here: select it for the steer bar, rename it, close it.
struct SessionActions {
    steer: bool,
    rename: bool,
    close: bool,
}

impl SessionActions {
    fn of(backend: &dyn Backend) -> Self {
        SessionActions {
            steer: backend.available(&Command::Steer { sessions: Vec::new(), text: String::new(), now: false }),
            rename: backend.available(&Command::Rename(String::new())),
            close: backend.available(&Command::CloseSession(String::new())),
        }
    }

    /// The session's context menu on `row`: what it records waits in `rename` or `close` until the list is drawn.
    fn menu(
        &self,
        row: &egui::Response,
        session: &SessionRow,
        client: &mut Client,
        rename: &mut Option<String>,
        close: &mut Option<String>,
    ) {
        if !self.steer && !self.rename && !self.close {
            return;
        }
        row.context_menu(|ui| {
            let mut selected = client.selected.contains(&session.id);
            if self.steer && ui.checkbox(&mut selected, "Include in “selected”").changed() {
                if selected {
                    client.selected.insert(session.id.clone());
                } else {
                    client.selected.remove(&session.id);
                }
            }
            if !session.chief && self.rename && ui.button("Rename…").clicked() {
                *rename = Some(session.id.clone());
                ui.close();
            }
            if self.close && ui.button("Close").clicked() {
                *close = Some(session.id.clone());
                ui.close();
            }
        });
    }
}

/// BOARDS: every board in the order the Boards dropdown lists them ([`crate::boards::listed`]), each as simple mode
/// shows a plan, with its state and when it last moved, and the sessions that worked for it under it: live ones
/// with their status, closed ones quiet, a click away from resuming. Then the sessions outside any board, the
/// earlier closed sessions and History, folded. A click on a board shows it; its menu archives or reopens it.
pub fn boards(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    // With no open plan there is no board to offer the ticket sign-in, so it goes above the list.
    if crate::signin::no_board_shows(backend) {
        crate::signin::framed(ui, backend, client);
    }
    let plans = crate::boards::in_frame(ui.ctx(), backend);
    let listed = &plans.1;
    let hints = backend.view_hints();
    let sessions: Vec<SessionRow> = backend.session_list().into_iter().filter(|s| !s.chief).collect();
    let visible = backend.visible_sessions();
    let all = backend.session_list();
    let number = |id: &str| visible.iter().position(|&index| all[index].id == id).map(|at| at + 1).filter(|&n| n <= 9);
    let recent = backend.recent_sessions();
    let can_resume = recent.first().is_some_and(|first| backend.available(&Command::Resume(first.record.clone())));
    let actions = SessionActions::of(backend);
    // Where a session shows while the view leaves it out: on its open board; among all sessions once that is done;
    // with the others of no board.
    let on_board = |index: usize, row: &PlanRow| {
        if row.status == PlanStatus::Open { Command::ShowPlan(index) } else { Command::SetView(None) }
    };
    let (mut activate, mut resume, mut rename, mut close) = (None, None, None, None);
    let mut show: Option<(usize, PlanRow)> = None;
    let mut command: Option<Command> = None;
    let mut archive: Option<usize> = None;
    let mut complete: Option<usize> = None;
    let keys = crate::window_keys::board_chords(ui.ctx().os()).map(|chord| ui.ctx().format_shortcut(&chord));
    ui.horizontal(|ui| {
        section(ui, &p, "BOARDS");
    })
    .response
    .on_hover_text(format!("{} and {} go to the next and previous board", keys[0], keys[1]));
    // The older done boards fold away, but for the one on screen and those an agent still works for.
    let live = |row: &PlanRow| sessions.iter().any(|s| s.task.as_ref().is_some_and(|link| link.plan == row.id));
    // The start screen is no board's place.
    let shown = (hints.plan_view && !client.home.shown).then_some(hints.selected_plan);
    let on_screen = |index: usize| shown == Some(index);
    let (done, older) = listed.split_done(|index, row| live(row) || on_screen(index));
    let older: Vec<(usize, PlanRow)> = older.into_iter().cloned().collect();
    let rows: Vec<_> = listed.waiting.iter().chain(&listed.active).chain(done).cloned().collect();
    for (index, row) in &rows {
        let selected = on_screen(*index);
        let (mut detail, color) = crate::simple::plan_detail(backend, *index, row, false, &p);
        if let Some(ago) = crate::home::last_active(row, backend) {
            detail.push_str(&format!(" · {ago}"));
        }
        let response =
            ui.push_id(("board", &row.id), |ui| crate::simple::plan_row(ui, row, (&detail, color), selected, &p)).inner;
        crate::flow::record(ui, &format!("board:{index}"), response.rect);
        let open = row.status == PlanStatus::Open;
        let can_complete = open
            && backend.available(&Command::ClosePlan { plan: *index, status: PlanStatus::Completed, force: false });
        let can_archive = backend.available(&Command::ArchivePlan(*index));
        // Under the pointer, the row's closes in words: Complete and Archive for an open board, Archive for a done one.
        let mut acted = false;
        if ui.rect_contains_pointer(response.rect) {
            let mut right = response.rect.right() - 6.0;
            // Over the line that says how the board stands: its goal stays readable.
            let y = response.rect.top() + 35.0;
            for (word, wanted, completes) in [("Archive", can_archive, false), ("Complete", can_complete, true)] {
                if !wanted {
                    continue;
                }
                let id = ui.scope_id().with(("board-row", word, &row.id));
                let hit = crate::controls::row_action(ui, id, pos2(right, y), word, &p);
                let pill = hit.rect;
                crate::flow::record(ui, &format!("board-{}:{index}", word.to_lowercase()), pill);
                let tip = if completes {
                    "Mark it done; asks first while tasks are not accepted or agents run"
                } else {
                    "Move it to History, where it can be reopened"
                };
                if hit.on_hover_text(tip).clicked() {
                    acted = true;
                    if completes {
                        complete = Some(*index);
                    } else {
                        archive = Some(*index);
                    }
                }
                right = pill.left() - 4.0;
            }
        }
        let response = response.on_hover_text(crate::boards::hover_goal(&row.goal));
        if response.clicked() && !acted {
            show = Some((*index, row.clone()));
        }
        response.context_menu(|ui| {
            use crate::icons;
            if can_complete && ui.button(icons::with(icons::APPROVE, "Complete plan")).clicked() {
                complete = Some(*index);
                ui.close();
            }
            if can_archive && ui.button(icons::with(icons::ARCHIVE, "Archive")).clicked() {
                archive = Some(*index);
                ui.close();
            }
            if !open && backend.available(&Command::ReopenPlan(*index)) && ui.button("Reopen").clicked() {
                command = Some(Command::ReopenPlan(*index));
                ui.close();
            }
        });
        for session in sessions.iter().filter(|s| s.task.as_ref().is_some_and(|link| link.plan == row.id)) {
            let active = hints.active.as_deref() == Some(session.id.as_str());
            let line = ui
                .push_id(("board-session", &session.id), |ui| {
                    session_line(ui, &p, &session.title, Some((session, active, number(&session.id))))
                })
                .inner
                .on_hover_text(session.hover());
            crate::flow::record(ui, &format!("session:{}", session.id), line.rect);
            if line.clicked() {
                activate = Some((session.id.clone(), on_board(*index, row)));
            }
            actions.menu(&line, session, client, &mut rename, &mut close);
        }
        if can_resume {
            for closed in
                recent.iter().filter(|r| r.record.pane.as_ref().is_some_and(|pane| row.sessions.contains(pane)))
            {
                let line = ui
                    .push_id(("board-closed", &closed.record.pane), |ui| {
                        session_line(ui, &p, &closed.record.title, None)
                    })
                    .inner;
                crate::flow::record(
                    ui,
                    &format!("closed:{}", closed.record.pane.as_deref().unwrap_or_default()),
                    line.rect,
                );
                let line = line.on_hover_text(format!("Resume this session\n{}", closed.folder));
                if line.clicked() {
                    resume = Some((closed.record.clone(), on_board(*index, row)));
                }
            }
        }
        ui.add_space(2.0);
    }
    let outside: Vec<&SessionRow> = sessions.iter().filter(|s| s.task.is_none()).collect();
    let checkouts = backend.checkouts();
    if !outside.is_empty() || !checkouts.is_empty() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(RichText::new("UNGROUPED").font(p.ui(10.5)).color(p.muted))
                .on_hover_text("Sessions that work for no board");
        });
        for session in outside {
            let active = hints.active.as_deref() == Some(session.id.as_str());
            let line = ui
                .push_id(("ungrouped-session", &session.id), |ui| {
                    session_line(ui, &p, &session.title, Some((session, active, number(&session.id))))
                })
                .inner
                .on_hover_text(session.hover());
            crate::flow::record(ui, &format!("session:{}", session.id), line.rect);
            if line.clicked() {
                activate = Some((session.id.clone(), Command::ShowUngrouped));
            }
            actions.menu(&line, session, client, &mut rename, &mut close);
        }
        for branch in &checkouts {
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                ui.spinner();
                ui.label(RichText::new(format!("checking out {branch}…")).color(p.muted).size(12.0));
            });
        }
    }
    if rows.is_empty() && sessions.is_empty() && checkouts.is_empty() {
        let text = "No boards yet. Tell the chief what to do: its board shows here.";
        ui.add(egui::Label::new(RichText::new(text).font(crate::flow::font(12.5)).color(p.muted)).wrap());
    }
    // Closed sessions of no board, and the archived boards: folded, out of the way of what runs.
    let boards_sessions: std::collections::HashSet<&String> =
        backend_sessions(&rows).chain(listed.history.iter().flat_map(|(_, row)| row.sessions.iter())).collect();
    let earlier: Vec<_> = if can_resume {
        recent.iter().filter(|r| !r.record.pane.as_ref().is_some_and(|pane| boards_sessions.contains(pane))).collect()
    } else {
        Vec::new()
    };
    if !earlier.is_empty() {
        ui.add_space(6.0);
        let id = ui.make_persistent_id("sidebar-closed-sessions");
        let mut open = ui.data_mut(|d| *d.get_persisted_mut_or_default::<bool>(id));
        let count = earlier.len();
        if fold(ui, &p, "CLOSED SESSIONS", Some(count), &mut open, |_| {}) {
            {
                for closed in earlier {
                    let line = ui
                        .push_id(("closed", &closed.record.title, &closed.folder), |ui| {
                            session_line(ui, &p, &closed.record.title, None)
                        })
                        .inner;
                    crate::flow::record(
                        ui,
                        &format!("closed:{}", closed.record.pane.as_deref().unwrap_or_default()),
                        line.rect,
                    );
                    let hover = closed.record.last_prompt.clone().unwrap_or_else(|| closed.folder.clone());
                    if line.on_hover_text(format!("Resume this session\n{hover}")).clicked() {
                        resume = Some((closed.record.clone(), Command::ShowUngrouped));
                    }
                }
            }
        }
        ui.data_mut(|d| d.insert_persisted(id, open));
    }
    for (title, salt, list) in [("DONE", "done", &older), ("HISTORY", "history", &listed.history)] {
        if list.is_empty() {
            continue;
        }
        ui.add_space(2.0);
        let id = ui.make_persistent_id(("sidebar-finished", salt));
        let mut open = ui.data_mut(|d| *d.get_persisted_mut_or_default::<bool>(id));
        if fold(ui, &p, title, Some(list.len()), &mut open, |_| {}) {
            for (index, row) in list {
                // There may be hundreds, and an archived one's date is read from disk: only those in view are drawn.
                if !crate::simple::rail_row_visible(ui, crate::simple::PLAN_ROW) {
                    continue;
                }
                let selected = on_screen(*index);
                let (mut detail, color) = crate::simple::plan_detail(backend, *index, row, salt == "history", &p);
                if salt == "done"
                    && let Some(ago) = crate::home::last_active(row, backend)
                {
                    detail.push_str(&format!(" · {ago}"));
                }
                let response = ui
                    .push_id((salt, &row.id), |ui| crate::simple::plan_row(ui, row, (&detail, color), selected, &p))
                    .inner;
                crate::flow::record(ui, &format!("{salt}:{index}"), response.rect);
                let hover = crate::boards::hover_goal(&row.goal);
                let response = response.on_hover_text(format!("{hover}\nView it; Reopen brings it back to work"));
                if response.clicked() {
                    show = Some((*index, row.clone()));
                }
                response.context_menu(|ui| {
                    use crate::icons;
                    let archives = salt == "done" && backend.available(&Command::ArchivePlan(*index));
                    if archives && ui.button(icons::with(icons::ARCHIVE, "Archive")).clicked() {
                        archive = Some(*index);
                        ui.close();
                    }
                    if backend.available(&Command::ReopenPlan(*index)) && ui.button("Reopen").clicked() {
                        command = Some(Command::ReopenPlan(*index));
                        ui.close();
                    }
                });
            }
        }
        ui.data_mut(|d| d.insert_persisted(id, open));
    }
    if let Some((index, row)) = show {
        crate::boards::show(&row, index, backend, client);
    }
    if let Some(command) = command {
        client.send(command);
    }
    if let Some(index) = archive {
        crate::boards::archive(index, backend, client);
    }
    if let Some(index) = complete {
        crate::boards::complete(index, backend, client);
    }
    let go = |place: Command, client: &mut Client| {
        if backend.available(&place) {
            client.send(place);
        }
    };
    if let Some((id, place)) = activate {
        if !visible.iter().any(|&index| all[index].id == id) {
            go(place, client);
        }
        client.send(Command::Activate { session: id, open_diff: true });
    }
    if let Some((record, place)) = resume {
        go(place, client);
        client.send(Command::Resume(record));
    }
    if let Some(id) = close {
        client.confirm_close = Some(id);
    }
    if let Some(id) = rename {
        client.send(Command::Rename(id));
    }
}

fn backend_sessions(rows: &[(usize, PlanRow)]) -> impl Iterator<Item = &String> {
    rows.iter().flat_map(|(_, row)| row.sessions.iter())
}

/// The AGENTS section: the sessions the current tab shows, worktrees being checked out, and the RECENT
/// sessions to resume. A click activates a session; its menu selects it for steering, renames or closes it. A
/// phone's Agents pane shows it whole; the full window's sidebar folds it ([`agents_fold`]).
pub fn sessions(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    // With no open plan there is no board to offer the ticket sign-in, so it goes above the agents.
    if crate::signin::no_board_shows(backend) {
        crate::signin::framed(ui, backend, client);
    }
    section(ui, &p, "AGENTS");
    agent_rows(ui, backend, client);
    let recent = backend.recent_sessions();
    if let Some(first) = recent.first()
        && backend.available(&Command::Resume(first.record.clone()))
    {
        ui.add_space(10.0);
        section(ui, &p, "RECENT · click to resume");
        for RecentSession { record, folder } in recent {
            let hover = record.last_prompt.clone().unwrap_or(folder);
            let response =
                ui.selectable_label(false, RichText::new(&record.title).color(p.muted).size(12.0)).on_hover_text(hover);
            if response.clicked() {
                client.send(Command::Resume(record));
            }
        }
    }
}

/// AGENTS as the full window's sidebar has it: folded until it is opened, as this window last left it. The closed
/// sessions are under their boards in BOARDS.
pub fn agents_fold(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let count = backend.visible_sessions().len();
    let mut open = client.folds.agents;
    let open = fold(ui, &p, "AGENTS", Some(count), &mut open, |_| {});
    client.folds.agents = open;
    if open {
        agent_rows(ui, backend, client);
    }
}

/// The agents of the current tab, grouped under the plan each works for, with what is being checked out.
fn agent_rows(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let mut clicked: Option<String> = None;
    let mut close: Option<String> = None;
    let mut rename: Option<String> = None;
    let sessions = backend.session_list();
    let visible = backend.visible_sessions();
    let active_id = backend.view_hints().active;
    let can_rename = backend.available(&Command::Rename(String::new()));
    let can_close = backend.available(&Command::CloseSession(String::new()));
    // What "selected" is for: the steer bar sends to them.
    let can_steer = backend.available(&Command::Steer { sessions: Vec::new(), text: String::new(), now: false });
    // The sessions grouped by the plan they work for, in the plans' order, then those outside any plan; one
    // group alone needs no heading.
    let plans = backend.plan_list();
    let group_of =
        |session: &SessionRow| session.task.as_ref().and_then(|link| plans.iter().position(|row| row.id == link.plan));
    let mut groups: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
    for &index in &visible {
        let group = group_of(&sessions[index]);
        match groups.iter_mut().find(|(g, _)| *g == group) {
            Some((_, members)) => members.push(index),
            None => groups.push((group, vec![index])),
        }
    }
    groups.sort_by_key(|(group, _)| group.unwrap_or(usize::MAX));
    let headed = groups.len() > 1 || groups.iter().any(|(group, _)| group.is_some());
    let mut order = Vec::new();
    for (group, members) in &groups {
        order.push((Some(*group), members[0]));
        order.extend(members.iter().skip(1).map(|&index| (None, index)));
    }
    for (heading, index) in order {
        let session = &sessions[index];
        if headed && let Some(group) = heading {
            let (text, color, hover, show) = match group.map(|g| (g, &plans[g])) {
                Some((g, row)) => {
                    let show = if row.status == atomics_core::plan::PlanStatus::Open {
                        Command::ShowPlan(g)
                    } else {
                        Command::ViewPlanHistory(g)
                    };
                    (row.tab_name.clone(), row.state.color(&p), format!("The plan: {}", row.goal), Some(show))
                }
                None => ("Outside plans".to_string(), p.muted, "Sessions no plan's task uses".to_string(), None),
            };
            let response = group_heading(ui, &p, &text, color, &hover);
            if let Some(show) = show.filter(|show| backend.available(show))
                && crate::touch::reach(ui, response).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
            {
                client.send(show);
            }
        }
        let active = active_id.as_deref() == Some(session.id.as_str());
        let number = visible.iter().position(|&shown| shown == index).map(|at| at + 1).filter(|&n| n <= 9);
        let row = session_row(ui, &p, session, active, client.selected.contains(&session.id), number)
            .on_hover_text(session.hover());
        if row.clicked() {
            clicked = Some(session.id.clone());
        }
        if !can_steer && !can_rename && !can_close {
            continue;
        }
        row.context_menu(|ui| {
            let mut selected = client.selected.contains(&session.id);
            if can_steer && ui.checkbox(&mut selected, "Include in “selected”").changed() {
                if selected {
                    client.selected.insert(session.id.clone());
                } else {
                    client.selected.remove(&session.id);
                }
            }
            if !session.chief && can_rename && ui.button("Rename…").clicked() {
                rename = Some(session.id.clone());
                ui.close();
            }
            if can_close && ui.button("Close").clicked() {
                close = Some(session.id.clone());
                ui.close();
            }
        });
    }
    let checkouts = backend.checkouts();
    for branch in &checkouts {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(format!("checking out {branch}…")).color(p.muted).size(12.0));
        });
    }
    if sessions.is_empty() && checkouts.is_empty() {
        let text = if backend.available(&Command::NewSession) {
            "No agents yet. Start one with + New,\nor install hooks to see agents\nrunning in other terminals."
        } else {
            "No agents yet."
        };
        ui.label(RichText::new(text).color(p.muted).size(12.0));
    }
    if let Some(id) = clicked {
        client.send(Command::Activate { session: id, open_diff: true });
    }
    if let Some(id) = close {
        client.confirm_close = Some(id);
    }
    if let Some(id) = rename {
        client.send(Command::Rename(id));
    }
}

/// The EXPLORER: the open projects, the focused project's repository workflow and worktrees, and its files
/// with the changes of the worktree under review laid over them.
pub fn explorer(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let palette = client.palette.clone();
    let p: &Palette = &palette;
    let can_pick = backend.available(&Command::PickProject);
    ui.horizontal(|ui| {
        section(ui, p, "EXPLORER");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if can_pick && ui.button("+").on_hover_text("Open project…").clicked() {
                client.send(Command::PickProject);
            }
        });
    });
    explorer_body(ui, backend, client);
}

/// The EXPLORER as the full window's sidebar has it: folded until it is opened, as this window last left it.
pub fn explorer_fold(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let can_pick = backend.available(&Command::PickProject);
    let mut open = client.folds.explorer;
    let mut pick = false;
    let open = fold(ui, &p, "EXPLORER", None, &mut open, |ui| {
        pick = can_pick && ui.small_button("+").on_hover_text("Open project…").clicked();
    });
    client.folds.explorer = open;
    if pick {
        client.send(Command::PickProject);
    }
    if open {
        explorer_body(ui, backend, client);
    }
}

/// The open projects, the focused one's repository workflow and worktrees, and its files.
fn explorer_body(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let palette = client.palette.clone();
    let p: &Palette = &palette;
    let target = backend.view_hints().diff_target;
    let repos = backend.repos();
    let agent_names = backend.ui_config().agents;
    let focus = target.as_deref().and_then(|t| backend.project_of(t)).or_else(|| repos.first().cloned());
    let can_pick = backend.available(&Command::PickProject);
    let can_start = backend.available(&Command::StartAgent { agent: String::new(), folder: PathBuf::new() });
    let can_add_worktree = backend.available(&Command::NewWorktree(PathBuf::new()));
    let can_close = backend.available(&Command::CloseProject(PathBuf::new()));
    let can_clean_up = backend.available(&Command::OpenCleanup { repo: PathBuf::new(), preselect: None });
    let can_open = backend.available(&Command::OpenFile { root: PathBuf::new(), path: String::new() });
    let agent_names = if can_start { agent_names } else { Vec::new() };
    let sessions = backend.session_list();
    let management = backend.management();
    let identity_error = management.as_ref().is_some_and(|info| info.tools["identity_error"].is_string());
    let identities = management.map(|info| info.tools["identities"].clone()).unwrap_or_default();
    let mut switch_to: Option<PathBuf> = None;
    let mut close: Option<PathBuf> = None;
    ui.horizontal_wrapped(|ui| {
        for root in repos {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let agents = sessions.iter().filter(|s| s.project.as_ref() == Some(&root)).count();
            let label = if agents > 0 { format!("{name} ·{agents}") } else { name };
            let identity = identities
                .as_array()
                .into_iter()
                .flatten()
                .find(|row| row["project"].as_str().is_some_and(|path| std::path::Path::new(path) == root));
            let label = identity
                .and_then(|row| {
                    if identity_error {
                        Some("identity unavailable")
                    } else {
                        row["identity"]["email"].as_str().or_else(|| row["label"].as_str())
                    }
                })
                .map_or(label.clone(), |identity| format!("{label} · {identity}"));
            let selected = focus.as_ref() == Some(&root);
            let color = if selected { p.accent } else { p.foreground };
            let response = ui.selectable_label(selected, RichText::new(label).color(color).size(12.0));
            if response.clicked() && !selected {
                switch_to = Some(root.clone());
            }
            if let Some(account) = identity.and_then(|row| row["github"].as_str()) {
                ui.label(RichText::new(format!("GitHub: {account}")).color(p.muted).size(11.0));
            }
            let detail = identity.and_then(|row| row["label"].as_str()).unwrap_or_default();
            let response = response.on_hover_text(format!("{}\n{detail}", backend.display(&root)));
            if !agent_names.is_empty() || can_add_worktree || can_close || identity.is_some() {
                response.context_menu(|ui| {
                    if let Some(row) = identity
                        && ui.button("Change identity…").clicked()
                    {
                        if let Ok(identity) = serde_json::from_value(row["override"].clone()) {
                            client.management.show_identity(root.to_string_lossy().into_owned(), identity);
                        }
                        ui.close();
                    }
                    for agent in &agent_names {
                        if ui.button(format!("Start {agent} in the main checkout")).clicked() {
                            client.send(Command::StartAgent { agent: agent.clone(), folder: root.clone() });
                            ui.close();
                        }
                    }
                    if can_add_worktree && ui.button("New worktree…").clicked() {
                        client.send(Command::NewWorktree(root.clone()));
                        ui.close();
                    }
                    if can_close && ui.button("Close project").clicked() {
                        close = Some(root.clone());
                        ui.close();
                    }
                });
            }
        }
    });
    if let Some(root) = switch_to {
        client.send(Command::ShowRoot(root));
    }
    if let Some(root) = close {
        client.send(Command::CloseProject(root));
        return;
    }
    let Some(project) = focus else {
        if can_pick {
            ui.add_space(8.0);
            ui.label(RichText::new("Open a project with + or Project › Open project…").color(p.muted).size(12.0));
        }
        return;
    };

    if let Some(workflow) = backend.repo_workflow(&project) {
        egui::CollapsingHeader::new("Repository workflow").id_salt(("repo-workflow", &project)).show(ui, |ui| {
            for (file, text) in &workflow.instructions {
                ui.label(RichText::new(file).small()).on_hover_text(text);
            }
            if !workflow.config.table.is_empty() {
                ui.small(&workflow.config.path).on_hover_text(workflow.config_text());
            }
            for warning in &workflow.warnings {
                ui.label(warning);
            }
            if workflow.instructions.is_empty() {
                ui.small("No ATOMICS.md or .atomics/*.md");
            }
        });
    }

    // Worktrees of the focused project, folded by default (big repos have dozens).
    let worktrees = backend.worktrees(&project);
    let explorer = client.explorers.entry(project.clone()).or_default();
    let arrow = if explorer.show_worktrees { "▾" } else { "▸" };
    let mut open_cleanup = false;
    ui.horizontal(|ui| {
        let header = ui.add(
            egui::Label::new(
                RichText::new(format!("{arrow} worktrees ({})", worktrees.len())).color(p.muted).size(12.0),
            )
            .sense(Sense::click()),
        );
        if header.clicked() {
            explorer.show_worktrees = !explorer.show_worktrees;
        }
        if can_clean_up
            && ui.button("Clean up…").on_hover_text("Find merged or abandoned worktrees and remove them").clicked()
        {
            open_cleanup = true;
        }
    });
    if open_cleanup {
        client.send(Command::OpenCleanup { repo: project.clone(), preselect: None });
        return;
    }
    let explorer = client.explorers.entry(project.clone()).or_default();
    if explorer.show_worktrees {
        let mut pick: Option<PathBuf> = None;
        let mut remove_one: Option<PathBuf> = None;
        egui::ScrollArea::vertical().id_salt(("worktrees", &project)).max_height(190.0).show(ui, |ui| {
            let main_selected = target.as_ref() == Some(&project);
            if ui.selectable_label(main_selected, RichText::new("main checkout").size(12.0)).clicked() {
                pick = Some(project.clone());
            }
            for wt in &worktrees {
                let name = wt.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let agent = sessions.iter().find(|s| s.root == wt.path);
                let selected = target.as_ref() == Some(&wt.path);
                let response = ui
                    .horizontal(|ui| {
                        if let Some(agent) = agent {
                            ui.label(RichText::new("●").color(status_color(agent.status, p)).size(10.5));
                        }
                        let response = ui.selectable_label(selected, RichText::new(&name).size(12.0));
                        if let Some(branch) = &wt.branch {
                            ui.label(RichText::new(branch).color(p.muted).size(10.5));
                        }
                        response
                    })
                    .inner;
                if response.clicked() {
                    pick = Some(wt.path.clone());
                }
                if !agent_names.is_empty() || can_clean_up {
                    response.context_menu(|ui| {
                        for agent in &agent_names {
                            if ui.button(format!("Start {agent} here")).clicked() {
                                client.send(Command::StartAgent { agent: agent.clone(), folder: wt.path.clone() });
                                ui.close();
                            }
                        }
                        if can_clean_up && ui.button("Remove worktree…").clicked() {
                            remove_one = Some(wt.path.clone());
                            ui.close();
                        }
                    });
                }
            }
        });
        if let Some(path) = remove_one {
            client.send(Command::OpenCleanup { repo: project.clone(), preselect: Some(path) });
            return;
        }
        if let Some(path) = pick {
            if backend.paged_folders() {
                client.send(Command::ShowRoot(path));
                return;
            }
            match sessions.iter().find(|s| s.root == path).map(|s| s.id.clone()) {
                Some(id) => client.send(Command::Activate { session: id, open_diff: true }),
                None => client.send(Command::ShowRoot(path)),
            }
        }
    }

    let review = target.filter(|t| backend.project_of(t).as_ref() == Some(&project)).unwrap_or_else(|| project.clone());
    if review != project {
        let name = review.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.label(RichText::new(format!("changes from {name}")).color(p.muted).size(11.0));
    }
    if backend.paged_folders() {
        folder_panel(ui, backend, client, &project, &review, can_open);
        return;
    }
    let main = backend.diff(&project).and_then(|d| d.prepared);
    let changes = backend.diff(&review).and_then(|d| d.prepared);
    let viewing = client.diff_views.get(&review).and_then(|d| d.viewing.clone());
    let (main, changes) = match (main, changes) {
        (Some(main), Some(changes)) => (main, changes),
        (Some(main), None) => (main.clone(), main),
        (None, Some(changes)) => (changes.clone(), changes),
        (None, None) => {
            ui.label(RichText::new("Reading files…").color(p.muted).size(12.0));
            return;
        }
    };
    let explorer = client.explorers.entry(project).or_default();
    if let Some(path) = diff_view::files_panel(ui, explorer, &main, &changes, viewing.as_deref(), p)
        && can_open
    {
        client.send(Command::OpenFile { root: review, path });
    }
}

/// A folder feed uses the same explorer in every remote client. Only folders on screen are followed;
/// opening a child releases the previous listing, and another page is fetched only on request.
fn folder_panel(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    project: &std::path::Path,
    root: &std::path::Path,
    can_open: bool,
) {
    let p = client.palette.clone();
    let touch = client.touch;
    let explorer = client.explorers.entry(project.to_path_buf()).or_default();
    ui.horizontal_wrapped(|ui| {
        if !explorer.folder.is_empty() && crate::touch::button(ui, touch, "↑ Parent folder").clicked() {
            explorer.folder = explorer.folder.rsplit_once('/').map_or(String::new(), |(parent, _)| parent.into());
        }
        ui.label(if explorer.folder.is_empty() { "/" } else { &explorer.folder });
    });
    ui.add(
        crate::controls::single_line(&mut explorer.filter)
            .hint_text("Filter files")
            .desired_width(ui.available_width()),
    );
    let path = explorer.folder.clone();
    let snapshot = backend.folder(root, &path);
    let changed = backend.diff(root).and_then(|diff| diff.prepared);
    let mut open = None;
    egui::ScrollArea::vertical().id_salt(("folder-files", root, &path)).auto_shrink([false, false]).show(ui, |ui| {
        for entry in &snapshot.entries {
            let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
            if !explorer.filter.is_empty() && !name.to_lowercase().contains(&explorer.filter.to_lowercase()) {
                continue;
            }
            let changed = changed.as_ref().is_some_and(|diff| diff.diff.files.iter().any(|f| f.path() == entry.path));
            let label = if entry.directory { format!("▸ {name}/") } else { name.into() };
            let color = if changed { p.accent } else { p.foreground };
            if ui
                .add(crate::touch::sized(
                    egui::Button::selectable(false, RichText::new(label).color(color)).truncate(),
                    touch,
                ))
                .clicked()
            {
                if entry.directory {
                    explorer.folder = entry.path.clone();
                } else if can_open {
                    open = Some(entry.path.clone());
                }
            }
        }
        if snapshot.loading {
            ui.label("Reading files…");
        }
        if let Some(error) = &snapshot.error {
            ui.label(RichText::new(error).color(p.warning));
        }
        if !snapshot.first && crate::touch::button(ui, touch, "First page").clicked() {
            backend.first_folder(root, &path);
        }
        if snapshot.more && !snapshot.loading && crate::touch::button(ui, touch, "Next files").clicked() {
            backend.more_folder(root, &path);
        }
        if snapshot.entries.is_empty() && !snapshot.loading && snapshot.error.is_none() {
            ui.label("No files in this folder");
        }
    });
    if let Some(path) = open {
        client.send(Command::OpenFile { root: root.to_path_buf(), path });
    }
}
