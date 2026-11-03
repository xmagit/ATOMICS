//! The sidebar's views: the AGENTS list with the RECENT sessions to resume, and the EXPLORER of the open
//! projects, their worktrees and files. The sidebar around them is [`crate::shell::sidebar`], with the
//! host's USAGE and TICKETS. A menu entry or button whose command the backend cannot carry out is left out.

use std::path::PathBuf;

use egui::{FontId, RichText, Sense, pos2, vec2};

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

fn session_row(ui: &mut egui::Ui, p: &Palette, s: &SessionRow, active: bool, selected: bool) -> egui::Response {
    let height = 38.0;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.accent, 0.14));
        painter.rect_filled(rect.with_max_x(rect.left() + 2.0), 0.0, p.accent);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, p.raised);
    }
    let now = ui.input(|i| i.time);
    painter.circle_filled(pos2(rect.left() + 10.0, rect.top() + 13.0), 3.5, status_dot(s.status, p, now));
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
    let mut detail = if s.scratch {
        format!("{} · scratch · {}", s.agent, s.status.label())
    } else {
        format!("{} · {}", s.agent, s.status.label())
    };
    if s.outside {
        detail.push_str(" · outside");
    }
    if selected {
        detail.push_str(" · ✔");
    }
    clip.text(pos2(rect.left() + 20.0, rect.top() + 21.0), egui::Align2::LEFT_TOP, detail, p.ui(11.0), p.muted);
    response
}

/// The AGENTS section: the sessions the current tab shows, worktrees being checked out, and the RECENT
/// sessions to resume. A click activates a session; its menu selects it for steering, renames or closes it.
pub fn sessions(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
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
    for (index, session) in sessions.iter().enumerate() {
        if !visible.contains(&index) {
            continue;
        }
        let active = active_id.as_deref() == Some(session.id.as_str());
        let row = session_row(ui, &p, session, active, client.selected.contains(&session.id));
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
                ui.selectable_label(false, RichText::new(&record.title).color(p.muted).size(12.5)).on_hover_text(hover);
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
            if can_pick && ui.small_button("+").on_hover_text("Open project…").clicked() {
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
    let mut switch_to: Option<PathBuf> = None;
    let mut close: Option<PathBuf> = None;
    ui.horizontal_wrapped(|ui| {
        for root in repos {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let agents = sessions.iter().filter(|s| s.project.as_ref() == Some(&root)).count();
            let label = if agents > 0 { format!("{name} ·{agents}") } else { name };
            let selected = focus.as_ref() == Some(&root);
            let color = if selected { p.accent } else { p.foreground };
            let response = ui.selectable_label(selected, RichText::new(label).color(color).size(12.5));
            if response.clicked() && !selected {
                switch_to = Some(root.clone());
            }
            let response = response.on_hover_text(backend.display(&root));
            if !agent_names.is_empty() || can_add_worktree || can_close {
                response.context_menu(|ui| {
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
            && ui
                .small_button("clean up…")
                .on_hover_text("Find merged or abandoned worktrees and remove them")
                .clicked()
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
                            ui.label(RichText::new("●").color(status_color(agent.status, p)).size(10.0));
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
            match sessions.iter().find(|s| s.root == path).map(|s| s.id.clone()) {
                Some(id) => client.send(Command::Activate { session: id, open_diff: true }),
                None => client.send(Command::ShowRoot(path)),
            }
        }
    }

    let review = target.filter(|t| backend.project_of(t).as_ref() == Some(&project)).unwrap_or_else(|| project.clone());
    if review != project {
        let name = review.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.label(RichText::new(format!("changes from {name}")).color(p.accent).size(11.0));
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
