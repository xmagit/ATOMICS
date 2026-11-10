//! GitHub and Azure pull requests read by the host, with client-local diff navigation.
use crate::diff_view::{self, DiffAction, DiffState, Header};
use crate::{Backend, Client, Command, PanelMode};
use atomics_core::layout::DiffSplit;
use egui::Rect;
use std::path::Path;

/// The list belongs to a window, including its folds and keyboard selection.
#[derive(Default)]
pub struct List {
    pub open: bool,
    selected: Option<String>,
    others_open: Option<bool>,
    scroll_selected: bool,
}

fn visible<'a>(
    pulls: &'a [atomics_core::projection::PullRequestInfo],
    list: &List,
) -> Vec<&'a atomics_core::projection::PullRequestInfo> {
    use atomics_core::github::Group;
    let others = pulls.iter().filter(|p| p.group.unwrap_or_default() == Group::Other).count();
    let expanded = list.others_open.unwrap_or(others <= 8 || others == pulls.len());
    let mut rows: Vec<_> = pulls.iter().filter(|p| expanded || p.group.unwrap_or_default() != Group::Other).collect();
    rows.sort_by_key(|p| p.group.unwrap_or_default());
    rows
}

fn load(
    ctx: &egui::Context,
    backend: &dyn Backend,
    client: &mut Client,
    pull: &atomics_core::projection::PullRequestInfo,
) {
    let command = Command::LoadPullRequest(pull.key.clone());
    let may_load = backend.available(&command);
    if !may_load && pull.head_sha.is_none() {
        return;
    }
    client.pr_target = None;
    client.pr_view = Some((pull.key.clone(), DiffState::default()));
    if may_load {
        client.send(command);
    }
    client.send(Command::SetPanel(PanelMode::Diff));
    client.send(Command::OpenDiff);
    client.pull_requests.open = false;
    crate::window_keys::Keyboard::Panel.settle(ctx, client);
}

/// Refuse external protocols even when metadata came from a remote host.
fn safe_web_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else { return false };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !rest.is_empty()
        && !rest.starts_with(['/', '?', '#'])
        && !url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\')
}

fn open_url(ctx: &egui::Context, url: &str) {
    if safe_web_url(url) {
        ctx.open_url(egui::OpenUrl::new_tab(url));
    }
}

pub fn menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let count = backend.pull_requests().len();
    if count > 0 && ui.button(format!("Pull requests · {count}")).clicked() {
        client.pull_requests.open = !client.pull_requests.open;
    }
}

/// Consume list navigation before any diff or terminal sees it.
pub fn keys(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    if !client.pull_requests.open {
        return;
    }
    let pulls = backend.pull_requests();
    let rows = visible(&pulls, &client.pull_requests);
    let index = rows.iter().position(|p| Some(&p.key) == client.pull_requests.selected.as_ref()).unwrap_or(0);
    let (down, up, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
        )
    });
    let index = if down {
        (index + 1).min(rows.len().saturating_sub(1))
    } else if up {
        index.saturating_sub(1)
    } else {
        index
    };
    client.pull_requests.scroll_selected |= down || up;
    client.pull_requests.selected = rows.get(index).map(|p| p.key.clone());
    if enter && let Some(pull) = rows.get(index) {
        load(ctx, backend, client, pull);
    }
}

pub fn dialog(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    use atomics_core::github::Group;
    if !client.pull_requests.open {
        return;
    }
    let pulls = backend.pull_requests();
    let rows = visible(&pulls, &client.pull_requests);
    let scroll_selected = std::mem::take(&mut client.pull_requests.scroll_selected);
    let muted = client.palette.muted;
    let mut open = true;
    egui::Window::new("Pull requests")
        .id(egui::Id::unique("client_pull_requests"))
        .open(&mut open)
        .default_width(620.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
                for group in [Group::Assigned, Group::Mine, Group::Other] {
                    let count = pulls.iter().filter(|p| p.group.unwrap_or_default() == group).count();
                    if group == Group::Other && count > 0 {
                        let expanded = rows.iter().any(|p| p.group.unwrap_or_default() == Group::Other);
                        if ui.button(format!("{} Other · {count}", if expanded { "▾" } else { "▸" })).clicked() {
                            client.pull_requests.others_open = Some(!expanded);
                        }
                    } else {
                        ui.strong(group.label());
                    }
                    for pull in rows.iter().filter(|p| p.group.unwrap_or_default() == group) {
                        ui.push_id(&pull.key, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                let selected = client.pull_requests.selected.as_ref() == Some(&pull.key);
                                let title = if pull.draft == Some(true) {
                                    format!("{} · Draft", pull.title)
                                } else {
                                    pull.title.clone()
                                };
                                let response = ui.selectable_label(selected, title).on_hover_text("Load diff");
                                if selected && scroll_selected {
                                    response.scroll_to_me(Some(egui::Align::Center));
                                }
                                if response.clicked() {
                                    load(ctx, backend, client, pull);
                                }
                                if ui
                                    .add_enabled(
                                        backend.available(&Command::LoadPullRequest(pull.key.clone()))
                                            || pull.head_sha.is_some(),
                                        egui::Button::new("View diff"),
                                    )
                                    .clicked()
                                {
                                    load(ctx, backend, client, pull);
                                }
                                if ui
                                    .add_enabled(safe_web_url(&pull.url), egui::Button::new("Open"))
                                    .on_hover_text("Open in browser")
                                    .clicked()
                                {
                                    open_url(ctx, &pull.url);
                                }
                            });
                            let author = pull.author.as_deref().map(|a| format!(" · {a}")).unwrap_or_default();
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}#{}{} · {} → {}",
                                    pull.repo, pull.number, author, pull.head, pull.base
                                ))
                                .color(muted),
                            );
                        });
                    }
                }
                if pulls.is_empty() {
                    ui.label("No pull requests");
                }
            });
        });
    client.pull_requests.open &= open;
}

/// True while this client has a PR selected. Comments and editor actions are absent: PR files do not
/// belong to a local worktree, and reviewing one must never type into an agent or open an editor.
pub fn panel(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) -> bool {
    let hints = backend.view_hints();
    let target = (hints.active, hints.diff_target);
    if client.pr_target.as_ref().is_some_and(|t| *t != target) {
        client.pr_view = None;
        client.pr_target = None;
    }
    if client.pr_view.is_some() {
        client.pr_target = Some(target);
    }
    let Some((key, state)) = client.pr_view.as_mut() else { return false };
    let Some(pull) = backend.pull_requests().into_iter().find(|p| p.key == *key) else {
        client.pr_view = None;
        return false;
    };
    let data = backend.diff(Path::new(key));
    state.error = data.as_ref().and_then(|d| d.error.clone()).or(pull.error);
    state.prepared = data.and_then(|d| d.prepared);
    state.loading = state.prepared.is_none() && state.error.is_none();
    state.comments.clear();
    let title = format!("{}#{} {}", pull.repo, pull.number, pull.title);
    let branch = format!(
        "{} → {}{}",
        pull.head,
        pull.base,
        pull.head_sha.map(|s| format!(" · {}", s.chars().take(8).collect::<String>())).unwrap_or_default()
    );
    let p = client.palette.clone();
    let config = backend.ui_config();
    let rect = if rect.width() >= 760.0 {
        let column = rect.with_max_x(rect.left() + 250.0);
        let current = state.focused.clone();
        if let Some((path, _)) = diff_view::changed_files(ui, column, state, current.as_deref(), &p) {
            state.open(&path);
        }
        rect.with_min_x(column.right())
    } else {
        rect
    };
    let header = Header {
        refresh: backend.available(&Command::LoadPullRequest(key.clone())),
        title: &title,
        branch: Some(&branch),
        can_send: false,
        split: client.diff_split.unwrap_or(config.layout.diff_split),
        editor_shortcut: &config.keys.toggle_editor,
        touch: client.touch,
        review: false,
        edit: false,
    };
    let font = p.mono(p.fonts.diff_size.unwrap_or(config.font.diff_size));
    let key = key.clone();
    match diff_view::show(ui, rect, state, header, &p, &font) {
        DiffAction::Close => client.pr_view = None,
        DiffAction::Refresh => client.send(Command::LoadPullRequest(key.clone())),
        DiffAction::Split(on) => client.diff_split = Some(if on { DiffSplit::Always } else { DiffSplit::Never }),
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::{github::Group, projection::PullRequestInfo};

    fn pull(group: Option<Group>, number: u64) -> PullRequestInfo {
        PullRequestInfo {
            group,
            author: Some("contoso".into()),
            draft: Some(false),
            key: format!("pull:contoso/repo#{number}"),
            repo: "contoso/repo".into(),
            number,
            title: "Update parser".into(),
            head: "feature".into(),
            base: "main".into(),
            url: "https://example.com/pull/1".into(),
            head_sha: None,
            loading: false,
            error: None,
        }
    }

    #[test]
    fn groups_order_rows_and_fold_only_a_long_other_group_with_personal_rows() {
        let mut pulls: Vec<_> = (1..=9).map(|n| pull(None, n)).collect();
        assert_eq!(visible(&pulls, &List::default()).len(), 9, "old hosts remain fully visible");
        pulls.push(pull(Some(Group::Mine), 10));
        pulls.push(pull(Some(Group::Assigned), 11));
        let rows = visible(&pulls, &List::default());
        assert_eq!(rows.iter().map(|p| p.number).collect::<Vec<_>>(), [11, 10]);
        let list = List { others_open: Some(true), ..Default::default() };
        assert_eq!(visible(&pulls, &list).len(), 11);
        pulls.remove(0);
        assert_eq!(visible(&pulls, &List::default()).len(), 10, "eight others start expanded");
    }

    #[test]
    fn browser_output_accepts_only_http_and_https() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            for url in [
                "file:///tmp/review",
                "javascript:alert(1)",
                "data:text/html,test",
                "mailto:review@example.com",
                "//example.com",
                "https:///missing",
                "https://example.com\nfile:///tmp/test",
            ] {
                open_url(ui.ctx(), url);
            }
        });
        output.textures_delta.clear();
        assert!(output.platform_output.commands.is_empty());
        for url in ["https://example.com/pull/1", "http://example.com/pull/1", "HTTPS://example.com/pull/1"] {
            let mut output = ctx.run_ui(Default::default(), |ui| open_url(ui.ctx(), url));
            output.textures_delta.clear();
            assert!(
                matches!(&output.platform_output.commands[..], [egui::OutputCommand::OpenUrl(open)] if open.url == url)
            );
        }
    }
}
