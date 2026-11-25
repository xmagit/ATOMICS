//! The fleet: a tile per session, its header over its terminal, and the tiles of finished plan tasks in
//! the cells the sessions leave free; above them the tab strip, when not every session gets a tile. The
//! platform draws each terminal through a [`TerminalSurface`].

use atomics_core::layout::Layout;
use atomics_core::preview::{Preview, PreviewSource};
use egui::{Color32, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::status::{status_color, status_dot, status_glyph, status_icon};
use crate::terminal::Metrics;
use crate::terminal_surface::TerminalSurface;
use crate::text::truncate;
use crate::theme::lerp;
use crate::{Backend, Client, Command, CompletedTile, PlanRow, PreviewStatus, approvals, board, boards};

/// The height of a tile's header.
pub const TILE_HEADER: f32 = 26.0;

/// The Preview menu and any open forwards that fit beside it in this tile's header.
pub(crate) fn tile_previews(
    ui: &mut egui::Ui,
    header: Rect,
    session: &str,
    previews: &[Preview],
    backend: &dyn Backend,
    client: &mut Client,
) -> f32 {
    if previews.is_empty() {
        return 0.0;
    }
    let open = |preview: &Preview| Command::OpenPreview {
        source: PreviewSource::Session(session.into()),
        preview: preview.clone(),
    };
    let status = |port| backend.preview_status(Some(session), port);
    let can_open = backend.can_preview();
    let menu_width = 76.0;
    let menu = header.with_min_x(header.right() - menu_width);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("preview", session))
            .max_rect(menu)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let has_forward = previews.iter().any(|preview| status(preview.port).is_some());
    child
        .add_enabled_ui(can_open || has_forward, |ui| {
            if previews.len() == 1 && !has_forward {
                if ui.button("Preview").on_hover_text(previews[0].title()).clicked() {
                    client.send(open(&previews[0]));
                }
            } else {
                ui.menu_button("Preview", |ui| {
                    for preview in previews {
                        let port = preview.port;
                        match status(port) {
                            Some(PreviewStatus::Paused) => {
                                if ui.button(format!("Retry {}", preview.title())).clicked() {
                                    client.send(Command::RetryPreview { port });
                                    ui.close();
                                }
                            }
                            Some(PreviewStatus::Waiting) => {
                                ui.label(format!("{} · Waiting", preview.title()));
                            }
                            _ => {
                                if ui.add_enabled(can_open, egui::Button::new(preview.title())).clicked() {
                                    client.send(open(preview));
                                    ui.close();
                                }
                            }
                        }
                        if status(port).is_some() && ui.button(format!("Stop port {port}")).clicked() {
                            client.send(Command::StopPreview { port });
                            ui.close();
                        }
                    }
                });
            }
        })
        .response
        .on_disabled_hover_text("Preview needs a native client connected to a host that supports tunnels");

    let gap = ui.spacing().item_spacing.x;
    let mut left = menu.left() - gap;
    let font = egui::TextStyle::Button.resolve(ui.style());
    let painter = ui.painter().clone();
    let padding = ui.spacing().button_padding.x;
    let color = client.palette.foreground;
    let button_width = |text: String| painter.layout_no_wrap(text, font.clone(), color).size().x + 2.0 * padding;
    for preview in previews {
        if status(preview.port) != Some(PreviewStatus::Open) {
            continue;
        }
        let title = preview.title();
        let width = button_width(title.clone()) + gap + button_width("Stop".into());
        let right = left;
        if right - width < header.left() + 92.0 {
            continue;
        }
        left = right - width;
        let rect = header.with_min_x(left).with_max_x(right);
        let mut actions = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("open-preview", session, preview.port))
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        if actions.button(&title).on_hover_text(format!("Open {title}")).clicked() {
            client.send(open(preview));
        }
        if actions.button("Stop").on_hover_text(format!("Stop {title}")).clicked() {
            client.send(Command::StopPreview { port: preview.port });
        }
        left -= gap;
    }
    header.right() - left
}

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
                let title = RichText::new(&s.title).color(color).size(12.0);
                let button = match crate::icons::text(status_icon(s.status), 12.0) {
                    Some(icon) => egui::Button::new((icon.color(status_color(s.status, &p)), title)),
                    None => egui::Button::new(
                        RichText::new(format!("{}  {}", status_glyph(s.status), s.title)).color(color).size(12.0),
                    ),
                };
                let button = if active {
                    button.fill(lerp(p.panel, p.accent, 0.25)).stroke(Stroke::NONE)
                } else {
                    button.frame_when_inactive(false)
                };
                let response = ui.add(button).on_hover_text(s.hover());
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

/// The finished cards this window hid, and the done boards whose cards it asked to see, by id.
const HIDDEN: &str = "atomics-finished-hidden";
const ASKED: &str = "atomics-finished-asked";
/// How many of either a window remembers.
const REMEMBERED: usize = 400;

fn remembered(ctx: &egui::Context, key: &str) -> Vec<String> {
    ctx.data_mut(|d| d.get_persisted::<Vec<String>>(egui::Id::unique(key))).unwrap_or_default()
}

fn remember(ctx: &egui::Context, key: &str, mut ids: Vec<String>) {
    ids.drain(..ids.len().saturating_sub(REMEMBERED));
    ctx.data_mut(|d| d.insert_persisted(egui::Id::unique(key), ids));
}

fn card_id(plans: &[PlanRow], done: &CompletedTile) -> Option<String> {
    plans.get(done.plan).map(|plan| format!("{}/{}", plan.id, done.task.id))
}

/// The current tab's finished tasks that take a tile: those this window did not hide, and on a done board none, whose
/// work is on the board beside them, unless this window asked for them ([`show_finished`]). A project's view keeps
/// the cards that lead back to its done boards.
pub fn finished(ctx: &egui::Context, backend: &dyn Backend) -> Vec<CompletedTile> {
    let mut finished = backend.completed_tasks();
    if finished.is_empty() {
        return finished;
    }
    let plans = boards::in_frame(ctx, backend);
    let (hidden, asked) = (remembered(ctx, HIDDEN), remembered(ctx, ASKED));
    let on_board = backend.view_hints().plan_view;
    finished.retain(|done| {
        let asked = || plans.0.get(done.plan).is_some_and(|plan| asked.contains(&plan.id));
        (!on_board || !done.read_only || asked()) && card_id(&plans.0, done).is_some_and(|id| !hidden.contains(&id))
    });
    finished
}

fn hide_finished(ctx: &egui::Context, plans: &[PlanRow], cards: &[&CompletedTile]) {
    let mut hidden = remembered(ctx, HIDDEN);
    for id in cards.iter().filter_map(|done| card_id(plans, done)) {
        if !hidden.contains(&id) {
            hidden.push(id);
        }
    }
    remember(ctx, HIDDEN, hidden);
}

/// Whether the board in view has finished cards out of sight, for the menu that brings them back.
pub fn hides_finished(ctx: &egui::Context, backend: &dyn Backend) -> bool {
    finished(ctx, backend).len() < backend.completed_tasks().len()
}

/// Every finished card of the board `plan` again.
pub fn show_finished(ctx: &egui::Context, plan: &str) {
    let mut hidden = remembered(ctx, HIDDEN);
    hidden.retain(|id| id.split_once('/').is_none_or(|(board, _)| board != plan));
    remember(ctx, HIDDEN, hidden);
    let mut asked = remembered(ctx, ASKED);
    asked.retain(|id| id != plan);
    asked.push(plan.into());
    remember(ctx, ASKED, asked);
}

/// The tiles of the sessions at `tiled` (positions in [`Backend::session_list`]) in `layout`'s grid, then
/// the current tab's finished tasks ([`Backend::completed_tasks`]) in the cells left. A session's tile is
/// its header (status, title, agent, branch and status, its sandbox, close), its approval banner, the
/// banner of MCP servers it needs, its plan task's last report and its terminal, which `surface` draws;
/// for a session outside ATOMICS, its last exchange instead. A click on a title shows the session's diff,
/// a double-click renames it; a click in a terminal makes its session the active one. Under
/// [`Client::touch`] a tile fills its cell to the screen's sides and its header and close are a finger's
/// size.
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
    // The actions the backend can carry out; the others are not drawn.
    let can_rename = backend.available(&Command::Rename(String::new()));
    let can_close = backend.available(&Command::CloseSession(String::new()));
    let can_resume = backend.available(&Command::ResumeSession(String::new()));
    let can_connect = backend.available(&Command::Connect { name: String::new(), store_env: false, url: None });
    let can_dismiss = backend.available(&Command::DismissMcp(String::new()));
    // Explicit requests may leave a field or a dialog, as a click on a terminal does. Background starts must not
    // issue them; window shortcuts decide separately whether to hand the keyboard back.
    let focus_request = client.focus_terminal.take();
    let (inset, header_height, close_size) = match client.touch {
        Some(touch) => (0.0, touch.target, touch.target),
        None => (3.0, TILE_HEADER, 18.0),
    };
    let now = ui.input(|i| i.time);
    let still = crate::simple::reduce_motion(backend, client);
    let sessions = backend.session_list();
    let active_id = backend.view_hints().active;

    for (slot, &index) in tiled.iter().enumerate() {
        let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
        let tile = Rect::from_min_size(min, size).shrink(inset);
        let row = &sessions[index];
        let view = backend.tile(index).unwrap_or_default();
        let active = active_id.as_deref() == Some(row.id.as_str());
        // Closing a scratch session deletes its folder, so it asks even once the agent exited.
        let ask = row.scratch || (!row.outside && !view.exited);

        let header = tile.with_max_y(tile.top() + header_height);
        let mut body = tile.with_min_y(header.bottom());
        ui.painter().rect_filled(header, 2.0, if active { lerp(p.raised, p.accent, 0.12) } else { p.raised });
        ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        if active {
            ui.painter().rect_filled(header.with_max_x(header.left() + 2.0), 0.0, p.accent);
        }
        let mark = pos2(header.left() + 12.0, header.center().y);
        let ink = if still { status_color(row.status, &p) } else { status_dot(row.status, &p, now) };
        // Idle and starting keep their quiet dot; a state that says something gets its icon.
        let quiet = matches!(
            row.status,
            atomics_core::session::AgentStatus::Idle | atomics_core::session::AgentStatus::Starting
        );
        if quiet || !crate::icons::paint(ui.painter(), mark, status_icon(row.status), 12.0, ink) {
            ui.painter().circle_filled(mark, 3.5, ink);
        }
        let mut badge_width = 0.0;
        if let Some(name) = &row.sandbox {
            let text =
                view.sandbox_usage.as_ref().map_or_else(|| "Sandbox".to_string(), |usage| format!("Sandbox · {usage}"));
            let galley = ui.fonts_mut(|f| f.layout_no_wrap(text, p.ui(10.5), p.info));
            badge_width = galley.size().x + SANDBOX_ICON + 19.0;
            let badge = Rect::from_min_max(
                pos2(
                    (header.right() - close_size - 10.0 - badge_width).max(header.left() + 22.0),
                    header.center().y - 8.5,
                ),
                pos2(header.right() - close_size - 13.0, header.center().y + 8.5),
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
        let previews = backend.previews(Some(&row.id));
        let preview_rect = header.with_max_x(header.right() - close_size - 10.0 - badge_width);
        badge_width += tile_previews(ui, preview_rect, &row.id, &previews, backend, client);
        let title_rect = Rect::from_min_max(
            pos2(header.left() + 22.0, header.top()),
            pos2(header.right() - close_size - 10.0 - badge_width, header.bottom()),
        );
        let title_response = ui.interact(title_rect, ui.scope_id().with(("tile-title", &row.id)), Sense::click());
        let hover = crate::controls::hover_amount(ui, title_response.id, title_response.hovered());
        let title_color = if active { p.accent } else { lerp(p.foreground, p.accent, hover) };
        let place = match (row.scratch, row.branch.as_deref()) {
            (true, _) => format!("{} · scratch · ", row.agent),
            (false, Some(branch)) if row.task.is_none() => format!("{} · {} · ", row.agent, branch),
            (false, _) => format!("{} · ", row.agent),
        };
        let note = view.queue_note.as_ref().map(|note| format!(" · {note}")).unwrap_or_default();
        let subtitle = format!("{place}{}{note}", row.status_label());
        let galley = ui.fonts_mut(|f| f.layout_no_wrap(row.title.clone(), p.ui(13.0), title_color));
        let title_width = galley.size().x;
        ui.painter().with_clip_rect(title_rect).galley(
            pos2(title_rect.left(), header.center().y - galley.size().y / 2.0),
            galley,
            title_color,
        );
        // The status in its own color between the place and the queue's note.
        let mut job = egui::text::LayoutJob::default();
        let format = |color| egui::TextFormat { font_id: p.ui(11.0), color, ..Default::default() };
        job.append(&place, 0.0, format(p.muted));
        job.append(row.status_label(), 0.0, format(status_color(row.status, &p)));
        job.append(&note, 0.0, format(p.muted));
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        ui.painter().with_clip_rect(title_rect).galley(
            pos2(title_rect.left() + title_width + 10.0, header.center().y - galley.size().y / 2.0),
            galley,
            p.muted,
        );
        // The header may cut a long title: the hover has all of it.
        let action = if row.chief {
            "Click to show this worktree's diff against its base."
        } else {
            "Click to show this worktree's diff against its base; double-click to rename."
        };
        let preview_details = if previews.is_empty() {
            String::new()
        } else {
            format!(
                "\n{}",
                previews
                    .iter()
                    .map(|p| format!("{} /{}", p.title(), p.path.trim_start_matches('/')))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let title_response =
            title_response.on_hover_text(format!("{}\n{subtitle}{preview_details}\n{action}", row.hover()));
        if title_response.double_clicked() && can_rename {
            rename = Some(row.id.clone());
        } else if title_response.clicked() {
            activate = Some((row.id.clone(), true));
        }
        if can_close {
            let close_rect = Rect::from_center_size(
                pos2(header.right() - close_size / 2.0 - 5.0, header.center().y),
                Vec2::splat(close_size),
            );
            let close_response = ui.interact(close_rect, ui.scope_id().with(("tile-close", &row.id)), Sense::click());
            let hover = crate::controls::hover_amount(ui, close_response.id, close_response.hovered());
            let wash = if close_response.is_pointer_button_down_on() { 0.3 } else { 0.16 * hover };
            if wash > 0.0 {
                ui.painter().rect_filled(close_rect, 3.0, p.error.gamma_multiply(wash));
            }
            if close_response.has_focus() {
                let ring = Stroke::new(2.0, ui.visuals().selection.stroke.color);
                ui.painter().rect_stroke(close_rect, 3.0, ring, egui::StrokeKind::Inside);
            }
            ui.painter().text(
                close_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                p.ui(15.0),
                lerp(p.muted, p.error, hover),
            );
            let close_response = close_response.on_hover_text(if ask {
                "Close this session; it asks first"
            } else {
                "Close this session"
            });
            if close_response.clicked() {
                close = Some((row.id.clone(), ask));
            }
        }
        let edge = if active { p.accent.gamma_multiply(0.7) } else { p.border };
        ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Outside);

        body = approvals::banner(ui, body, index, backend, client);
        if !view.connect.is_empty() && can_connect {
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
                    ui.label(RichText::new(&button.detail).size(11.0).color(p.muted)).on_hover_text(
                        "This agent needs an MCP server that is not connected. Connecting it opens its sign-in \
                         page in your browser, and every sandboxed session then gets it.",
                    );
                    if line == 0 && can_dismiss && ui.button("×").on_hover_text("Dismiss").clicked() {
                        dismiss = Some(row.id.clone());
                    }
                });
            }
            body.min.y = banner.bottom();
        }
        let term_id = ui.scope_id().with(("term", &row.id));
        for warning in row.status_hint.iter().chain(view.warning.iter()) {
            let wrap = (body.width() - 16.0).max(40.0);
            let galley = ui.fonts_mut(|f| f.layout(warning.clone(), p.ui(12.0), p.warning, wrap));
            let banner = body.with_max_y((body.top() + galley.size().y + 10.0).min(body.bottom()));
            ui.painter().rect_filled(banner, 0.0, lerp(p.panel, p.warning, 0.12));
            ui.painter().with_clip_rect(banner).galley(banner.left_top() + vec2(8.0, 5.0), galley, p.warning);
            body.min.y = banner.bottom();
        }
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
                        if view.resumable && can_resume && ui.button("Resume").clicked() {
                            resume = Some(row.id.clone());
                        }
                        if can_close && ui.button("Close").clicked() {
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
            if crate::controls::surface(ui.interact(body, term_id, Sense::click())).clicked() && !active {
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
    let mut show_board = None;
    let plans = boards::in_frame(ui.ctx(), backend);
    let finished = finished(ui.ctx(), backend);
    let several = finished.len() > 1;
    let mut hide = Vec::new();
    for (offset, done) in finished.iter().take(layout.completed).enumerate() {
        let slot = tiled.len() + offset;
        let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
        let tile = Rect::from_min_size(min, size).shrink(3.0);
        ui.painter().rect_filled(tile, 2.0, p.raised);
        ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        let mut child = ui.new_child(
            egui::UiBuilder::new().id_salt(("completed", done.plan, done.index)).max_rect(tile.shrink(14.0)),
        );
        let close = client.touch.map_or(18.0, |touch| touch.target);
        let cross = Rect::from_min_size(tile.right_top() + vec2(-close - 6.0, 6.0), vec2(close, close));
        child.scope(|ui| {
            ui.set_max_width((cross.left() - ui.min_rect().left() - 6.0).max(40.0));
            ui.label(RichText::new(&done.task.title).color(p.foreground).strong().size(13.0));
        });
        let hit = ui.interact(cross, ui.scope_id().with(("hide-finished", done.plan, done.index)), Sense::click());
        crate::controls::feedback(ui, cross, &hit, 4.0, &p);
        let hover = crate::controls::hover_amount(ui, hit.id, hit.hovered());
        let mark = crate::status::Mark::Cross;
        crate::status::paint_mark(ui.painter(), cross.center(), mark, lerp(p.muted, p.foreground, hover));
        crate::flow::record(ui, &format!("hide-finished:{}:{}", done.plan, done.index), cross);
        if hit.on_hover_text("Hide this card; the work stays on its board").clicked() {
            hide.push(done);
        }
        child.horizontal_wrapped(|ui| {
            // Finished boards have left the top bar: the work they did leads back to them.
            let show = plans.0.get(done.plan).map(|row| boards::show_command(row, done.plan));
            if let Some(show) = show.filter(|show| backend.available(show))
                && ui.button("Open board").on_hover_text("The board of the plan this task belongs to").clicked()
            {
                show_board = Some(show);
            }
            if several && ui.button("Hide all").on_hover_text("Hide every finished card here").clicked() {
                hide.extend(&finished);
            }
        });
        if let Some(action) = board::task_result_for(
            &mut child,
            &done.task,
            None,
            false,
            done.read_only,
            done.worktree,
            done.codex,
            done.now,
            &p,
            None,
            board::TaskButtons::of(backend, done.plan),
        ) {
            task_action = Some((done.plan, done.index, action));
        }
    }
    if !hide.is_empty() {
        hide_finished(ui.ctx(), &plans.0, &hide);
    }
    if let Some(command) = task_action.and_then(|(plan, task, action)| Command::task(plan, task, action)) {
        client.send(command);
    }
    if let Some(show) = show_board {
        client.send(show);
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
