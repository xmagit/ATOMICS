//! The sidebar's views: the AGENTS list with the RECENT sessions to resume, and the EXPLORER of the open
//! projects, their worktrees and files. The sidebar around them is [`crate::shell::sidebar`], with the
//! host's USAGE and TICKETS. A menu entry or button whose command the backend cannot carry out is left out.

use std::path::PathBuf;

use egui::{FontId, RichText, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::status::{status_color, status_dot};
use crate::theme::{Palette, lerp};
use crate::{Backend, Client, Command, RecentSession, SessionRow, diff_view};

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
    let needs = s.status.needs_attention() || s.pending_approvals > 0;
    if active {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.accent, 0.14));
    } else if needs {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.warning, 0.08));
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, p.raised);
    }
    if active || needs {
        let bar = if needs { p.warning } else { p.accent };
        painter.rect_filled(rect.with_max_x(rect.left() + 2.0), 0.0, bar);
    }
    if active && needs {
        painter.rect_filled(rect.with_min_x(rect.right() - 2.0), 0.0, p.accent);
    }
    if response.has_focus() {
        let ring = Stroke::new(2.0, ui.visuals().selection.stroke.color);
        painter.rect_stroke(rect.shrink(1.0), 2.0, ring, StrokeKind::Inside);
    }
    let now = ui.input(|i| i.time);
    painter.circle_filled(pos2(rect.left() + 10.0, rect.top() + 13.0), 3.5, status_dot(s.status, p, now));
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
    let mut state = format!(" · {}", s.status.label());
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
    ui.add(label).on_hover_text(hover)
}

/// The AGENTS section: the sessions the current tab shows, worktrees being checked out, and the RECENT
/// sessions to resume. A click activates a session; its menu selects it for steering, renames or closes it.
pub fn sessions(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    // With no open plan there is no board to offer the ticket sign-in, so it goes above the agents.
    if crate::signin::no_board_shows(backend) {
        crate::signin::framed(ui, backend, client);
    }
    section(ui, &p, "AGENTS");
    let mut clicked: Option<String> = None;
    let mut close: Option<String> = None;
    let mut rename: Option<String> = None;
    let sessions = backend.session_list();
    let visible = backend.visible_sessions();
    let active_id = backend.view_hints().active;
    let can_rename = backend.available(&Command::Rename(String::new()));
    let can_close = backend.available(&Command::CloseSession(String::new()));
    // What "selected" is for: the steer bar sends to them.
    let can_steer = backend.available(&Command::Steer { sessions: Vec::new(), text: String::new() });
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
        let row = session_row(ui, &p, session, active, client.selected.contains(&session.id), number);
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

/// The EXPLORER: the open projects, the focused project's repository workflow and worktrees, and its files
/// with the changes of the worktree under review laid over them.
pub fn explorer(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let palette = client.palette.clone();
    let p: &Palette = &palette;
    let target = backend.view_hints().diff_target;
    let repos = backend.repos();
    let agent_names = backend.ui_config().agents;
    let focus = target.as_deref().and_then(|t| backend.project_of(t)).or_else(|| repos.first().cloned());
    let can_pick = backend.available(&Command::PickProject);
    ui.horizontal(|ui| {
        section(ui, p, "EXPLORER");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if can_pick && ui.button("+").on_hover_text("Open project…").clicked() {
                client.send(Command::PickProject);
            }
        });
    });
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
            let detail = identity.and_then(|row| row["label"].as_str()).unwrap_or_default();
            let response = response.on_hover_text(format!("{}\n{detail}", backend.display(&root)));
            if !agent_names.is_empty() || can_add_worktree || can_close || identity.is_some() {
                response.context_menu(|ui| {
                    if let Some(row) = identity
                        && ui.button("Change identity…").clicked()
                    {
                        if let Ok(identity) = serde_json::from_value(row["identity"].clone()) {
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
