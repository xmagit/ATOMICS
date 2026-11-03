//! The fleet: a tile per session, its header over its terminal, and the tiles of finished plan tasks in
//! the cells the sessions leave free; above them the tab strip, when not every session gets a tile. The
//! platform draws each terminal through a [`TerminalSurface`].

use atomics_core::layout::Layout;
use egui::{Color32, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::status::{status_dot, status_glyph};
use crate::terminal::Metrics;
use crate::terminal_surface::TerminalSurface;
use crate::text::truncate;
use crate::theme::lerp;
use crate::{Backend, Client, Command, approvals, board};

/// The height of a tile's header.
pub const TILE_HEADER: f32 = 26.0;

/// Side of the sandbox mark in a tile's header badge.
const SANDBOX_ICON: f32 = 10.0;

/// The sandbox mark: a closed box, drawn as an isometric cube.
fn sandbox_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let (c, r) = (rect.center(), rect.width() / 2.0);
    let (dx, dy) = (r * 0.87, r * 0.5);
    let corner = |x: f32, y: f32| pos2(c.x + x, c.y + y);
    let outline =
        vec![corner(0.0, -r), corner(dx, -dy), corner(dx, dy), corner(0.0, r), corner(-dx, dy), corner(-dx, -dy)];
    let stroke = Stroke::new(1.2, color);
    painter.add(egui::Shape::closed_line(outline, stroke));
    painter.line_segment([corner(-dx, -dy), c], stroke);
    painter.line_segment([corner(dx, -dy), c], stroke);
    painter.line_segment([c, corner(0.0, r)], stroke);
}

/// The strip of session tabs over the tiles: the current tab's sessions (`visible`, positions in
/// [`Backend::session_list`]), the tiled ones brighter and the active one highlighted. A click makes a
/// session the active one.
pub fn tab_strip(
    ui: &mut egui::Ui,
    rect: Rect,
    tiled: &[usize],
    visible: &[usize],
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.panel);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("tab-strip")
            .max_rect(rect.shrink2(vec2(6.0, 3.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut clicked = None;
    let sessions = backend.session_list();
    let active_id = backend.view_hints().active;
    egui::ScrollArea::horizontal().id_salt("tab-strip-scroll").show(&mut child, |ui| {
        ui.horizontal(|ui| {
            for &i in visible {
                let s = &sessions[i];
                let active = active_id.as_deref() == Some(s.id.as_str());
                let shown = tiled.contains(&i);
                let color = if active {
                    p.foreground
                } else if shown {
                    lerp(p.muted, p.foreground, 0.5)
                } else {
                    p.muted
                };
                let text = RichText::new(format!("{}  {}", status_glyph(s.status), s.title)).color(color).size(12.5);
                let response = ui.add(
                    egui::Button::new(text)
                        .fill(if active { lerp(p.panel, p.accent, 0.25) } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE),
                );
                if response.clicked() {
                    clicked = Some(s.id.clone());
                }
            }
        });
    });
    if let Some(session) = clicked {
        client.send(Command::Activate { session, open_diff: false });
    }
}

/// The tiles of the sessions at `tiled` (positions in [`Backend::session_list`]) in `layout`'s grid, then
/// the current tab's finished tasks ([`Backend::completed_tasks`]) in the cells left. A session's tile is
/// its header (status, title, agent, branch and status, its sandbox, close), its approval banner, the
/// banner of MCP servers it needs, its plan task's last report and its terminal, which `surface` draws;
/// for a session outside ATOMICS, its last exchange instead. A click on a title shows the session's diff,
/// a double-click renames it; a click in a terminal makes its session the active one.
pub fn tiles(
    ui: &mut egui::Ui,
    rect: Rect,
    layout: &Layout,
    tiled: &[usize],
    backend: &dyn Backend,
    client: &mut Client,
    surface: &mut dyn TerminalSurface,
) {
    if layout.tiled + layout.completed == 0 {
        return;
    }
    let p = client.palette.clone();
    let metrics = Metrics::new(ui, &p, backend.ui_config().font.size);
    let (cols, rows) = (layout.columns.max(1), layout.rows.max(1));
    let size = vec2(rect.width() / cols as f32, rect.height() / rows as f32);
    let mut activate: Option<(String, bool)> = None;
    // The session to close, and whether to ask first.
    let mut close: Option<(String, bool)> = None;
    let mut rename: Option<String> = None;
    let mut resume: Option<String> = None;
    // A banner's Connect button: the server, whether to store its environment, the agent's URL.
    let mut connect: Option<(String, bool, Option<String>)> = None;
    let mut dismiss: Option<String> = None;
    let focus_request = client.focus_terminal.take();
    let now = ui.input(|i| i.time);
    let sessions = backend.session_list();
    let active_id = backend.view_hints().active;

    for (slot, &index) in tiled.iter().enumerate() {
        let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
        let tile = Rect::from_min_size(min, size).shrink(3.0);
        let row = &sessions[index];
        let view = backend.tile(index).unwrap_or_default();
        let active = active_id.as_deref() == Some(row.id.as_str());
        // Closing a scratch session deletes its folder, so it asks even once the agent exited.
        let ask = row.scratch || (!row.outside && !view.exited);

        let header = tile.with_max_y(tile.top() + TILE_HEADER);
        let mut body = tile.with_min_y(header.bottom());
        ui.painter().rect_filled(header, 2.0, if active { lerp(p.raised, p.accent, 0.12) } else { p.raised });
        ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        if active {
            ui.painter().rect_filled(header.with_max_x(header.left() + 2.0), 0.0, p.accent);
        }
        ui.painter().circle_filled(pos2(header.left() + 12.0, header.center().y), 3.5, status_dot(row.status, &p, now));
        let mut badge_width = 0.0;
        if let Some(name) = &row.sandbox {
            let text =
                view.sandbox_usage.as_ref().map_or_else(|| "Sandbox".to_string(), |usage| format!("Sandbox · {usage}"));
            let galley = ui.fonts_mut(|f| f.layout_no_wrap(text, p.ui(10.0), p.info));
            badge_width = galley.size().x + SANDBOX_ICON + 19.0;
            let badge = Rect::from_min_max(
                pos2((header.right() - 28.0 - badge_width).max(header.left() + 22.0), header.center().y - 8.5),
                pos2(header.right() - 31.0, header.center().y + 8.5),
            );
            ui.painter().rect_filled(badge, 4.0, lerp(p.raised, p.info, 0.15));
            let painter = ui.painter().with_clip_rect(badge);
            let icon = Rect::from_center_size(
                badge.left_center() + vec2(5.0 + SANDBOX_ICON / 2.0, 0.0),
                vec2(SANDBOX_ICON, SANDBOX_ICON),
            );
            sandbox_icon(&painter, icon, p.info);
            painter.galley(badge.left_center() + vec2(9.0 + SANDBOX_ICON, -galley.size().y / 2.0), galley, p.info);
            let resources = match (&view.sandbox_usage, &view.sandbox_error) {
                (Some(_), _) => "Host VM process tree: resident memory and ps CPU (100% = one core).",
                (None, Some(error)) => error.as_str(),
                (None, None) => "Its memory and CPU show here once ATOMICS finds the VM's host process.",
            };
            ui.interact(badge, ui.scope_id().with(("sandbox-usage", name)), Sense::hover()).on_hover_text(format!(
                "Runs sandboxed in Docker Sandboxes VM {name}: it sees its worktree and git's write folders, \
                     not the rest of this machine.\n{resources}"
            ));
        }
        let title_rect = Rect::from_min_max(
            pos2(header.left() + 22.0, header.top()),
            pos2(header.right() - 28.0 - badge_width, header.bottom()),
        );
        let title_response = ui.interact(title_rect, ui.scope_id().with(("tile-title", &row.id)), Sense::click());
        let title_color = if title_response.hovered() || active { p.accent } else { p.foreground };
        let mut subtitle = match (row.scratch, row.branch.as_deref()) {
            (true, _) => format!("{} · scratch · {}", row.agent, row.status.label()),
            (false, Some(branch)) => format!("{} · {} · {}", row.agent, branch, row.status.label()),
            (false, None) => format!("{} · {}", row.agent, row.status.label()),
        };
        if let Some(note) = &view.queue_note {
            subtitle.push_str(&format!(" · {note}"));
        }
        let galley = ui.fonts_mut(|f| f.layout_no_wrap(row.title.clone(), p.ui(13.0), title_color));
        let title_width = galley.size().x;
        ui.painter().with_clip_rect(title_rect).galley(
            pos2(title_rect.left(), header.center().y - galley.size().y / 2.0),
            galley,
            title_color,
        );
        ui.painter().with_clip_rect(title_rect).text(
            pos2(title_rect.left() + title_width + 10.0, header.center().y),
            egui::Align2::LEFT_CENTER,
            subtitle,
            p.ui(11.5),
            p.muted,
        );
        let hover = if row.chief {
            "Show this worktree's diff against main"
        } else {
            "Show this worktree's diff against main · double-click to rename"
        };
        let title_response = title_response.on_hover_text(hover);
        if title_response.double_clicked() {
            rename = Some(row.id.clone());
        } else if title_response.clicked() {
            activate = Some((row.id.clone(), true));
        }
        let close_rect = Rect::from_center_size(pos2(header.right() - 14.0, header.center().y), Vec2::splat(18.0));
        let close_response = ui.interact(close_rect, ui.scope_id().with(("tile-close", &row.id)), Sense::click());
        ui.painter().text(
            close_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            p.ui(15.0),
            if close_response.hovered() { p.error } else { p.muted },
        );
        if close_response.clicked() {
            close = Some((row.id.clone(), ask));
        }
        let edge = if active { p.accent.gamma_multiply(0.7) } else { p.border };
        ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Outside);

        body = approvals::banner(ui, body, index, backend, client);
        if !view.connect.is_empty() {
            let shown = view.connect.len().min(3);
            let banner = body.with_max_y((body.top() + 8.0 + 28.0 * shown as f32).min(body.bottom()));
            ui.painter().rect_filled(banner, 0.0, p.panel);
            let mut child = ui.new_child(
                egui::UiBuilder::new().id_salt(("mcp-needed", &row.id)).max_rect(banner.shrink2(vec2(8.0, 4.0))),
            );
            child.set_clip_rect(banner);
            for (line, button) in view.connect.iter().take(shown).enumerate() {
                child.horizontal(|ui| {
                    if ui.add_enabled(button.enabled, egui::Button::new(&button.label)).clicked() {
                        connect = Some((button.name.clone(), button.store_env, button.url.clone()));
                    }
                    ui.label(RichText::new(&button.detail).size(11.5).color(p.muted)).on_hover_text(
                        "This agent needs an MCP server that is not connected. Connecting it opens its sign-in \
                         page in your browser, and every sandboxed session then gets it.",
                    );
                    if line == 0 && ui.small_button("×").on_hover_text("Dismiss").clicked() {
                        dismiss = Some(row.id.clone());
                    }
                });
            }
            body.min.y = banner.bottom();
        }
        let term_id = ui.scope_id().with(("term", &row.id));
        if let Some(reported) = view.reported {
            let banner = body.with_max_y(body.top() + 24.0);
            ui.painter().rect_filled(banner, 0.0, p.panel);
            ui.painter().text(
                banner.left_center() + vec2(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                reported,
                p.ui(12.0),
                p.muted,
            );
            body.min.y = banner.bottom();
        }
        if !row.outside {
            // Focus before reading this frame's events, including the key that left the diff.
            if focus_request.as_deref() == Some(row.id.as_str()) {
                ui.memory_mut(|m| m.request_focus(term_id));
            }
            let response = surface.show(ui, index, &row.id, body, term_id, &metrics, &p);
            if (response.clicked() || response.gained_focus()) && !active {
                activate = Some((row.id.clone(), false));
            }
            if view.exited {
                let overlay = Rect::from_center_size(body.center(), vec2(body.width().min(320.0), 80.0));
                ui.painter().rect_filled(overlay, 8.0, p.panel);
                let mut child =
                    ui.new_child(egui::UiBuilder::new().id_salt(("exited", &row.id)).max_rect(overlay.shrink(12.0)));
                child.vertical_centered(|ui| {
                    ui.label(RichText::new("Process exited").color(p.foreground));
                    ui.horizontal(|ui| {
                        if view.resumable && ui.button("Resume").clicked() {
                            resume = Some(row.id.clone());
                        }
                        if ui.button("Close").clicked() {
                            close = Some((row.id.clone(), ask));
                        }
                    });
                });
            }
        } else {
            ui.painter().rect_filled(body, 0.0, p.background);
            let mut child =
                ui.new_child(egui::UiBuilder::new().id_salt(("outside", &row.id)).max_rect(body.shrink(14.0)));
            child.label(RichText::new(format!("Running outside ATOMICS in {}", view.folder)).color(p.muted).size(12.0));
            child
                .label(RichText::new("Messages reach it at its next tool call or turn end.").color(p.muted).size(12.0));
            child.add_space(8.0);
            if let Some(prompt) = &view.last_prompt {
                child.label(RichText::new("You").color(p.accent).size(12.0));
                child.label(RichText::new(truncate(prompt, 600)).color(p.foreground));
                child.add_space(6.0);
            }
            if let Some(message) = &view.last_message {
                child.label(RichText::new("Claude").color(p.success).size(12.0));
                child.label(RichText::new(truncate(message, 1500)).color(p.foreground));
            }
            if ui.interact(body, term_id, Sense::click()).clicked() && !active {
                activate = Some((row.id.clone(), false));
            }
        }
    }

    if let Some((name, store_env, url)) = connect {
        client.send(Command::Connect { name, store_env, url });
    }
    if let Some(id) = dismiss {
        client.send(Command::DismissMcp(id));
    }
    let mut task_action = None;
    for (offset, done) in backend.completed_tasks().into_iter().take(layout.completed).enumerate() {
        let slot = tiled.len() + offset;
        let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
        let tile = Rect::from_min_size(min, size).shrink(3.0);
        ui.painter().rect_filled(tile, 2.0, p.raised);
        ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        let mut child = ui.new_child(
            egui::UiBuilder::new().id_salt(("completed", done.plan, done.index)).max_rect(tile.shrink(14.0)),
        );
        child.label(RichText::new(&done.task.title).color(p.foreground).strong().size(13.0));
        if let Some(action) = board::task_result(
            &mut child,
            &done.task,
            None,
            false,
            done.read_only,
            done.worktree,
            done.codex,
            done.now,
            &p,
        ) {
            task_action = Some((done.plan, done.index, action));
        }
    }
    if let Some(command) = task_action.and_then(|(plan, task, action)| Command::task(plan, task, action)) {
        client.send(command);
    }

    if let Some((session, open_diff)) = activate {
        client.send(Command::Activate { session, open_diff });
    }
    if let Some(id) = rename {
        client.send(Command::Rename(id));
    }
    if let Some((id, ask)) = close {
        if ask { client.confirm_close = Some(id) } else { client.send(Command::CloseSession(id)) }
    }
    if let Some(id) = resume {
        client.send(Command::ResumeSession(id));
    }
}
