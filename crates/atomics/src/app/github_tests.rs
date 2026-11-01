//! The GitHub controls against the in-process fake GitHub, a fake `gh`, a stand-in browser and the
//! test keychain. Nothing here reaches github.com.
use super::super::tests::{session, with_app};
use super::*;
use crate::connections::tests::{browser, opened};
use crate::github::fake::{self, FakeGithub};

const TOKEN: &str = "gho_PRIVATE-github-app-token";

type Texts = Vec<(String, Pos2)>;

fn repo() -> RepoId {
    RepoId::parse("octo/widgets").unwrap()
}

fn store(app: &Desktop<'_>) -> CredentialStore {
    CredentialStore::new(app.runtime.relay.keychain().clone())
}

/// Points the app at `fake` and lists `octo/widgets`.
fn connect_to(app: &mut Desktop<'_>, fake: &FakeGithub, dir: &Path) {
    app.runtime.github.endpoints = Endpoints { base: Some(fake.url.clone()), gh: Some(fake::gh(dir, None)) };
    app.runtime.github.browser = Some(browser(dir));
    app.runtime.config.github.repos = vec!["octo/widgets".into()];
    app.runtime.repos.clear();
}

fn settle(app: &mut Desktop<'_>, what: &str, done: impl Fn(&Desktop<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.runtime.github_tick();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Polls now and waits until the poll, with details when the modal is open, has been applied.
fn poll(app: &mut Desktop<'_>) {
    app.runtime.github.next_poll = None;
    app.runtime.github.backoff = None;
    app.runtime.github_tick();
    settle(app, "the poll", |app| !app.runtime.github.polling);
}

fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> (Texts, egui::PlatformOutput) {
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 1000.0))),
            time: Some(ctx.input(|input| input.time) + 0.1),
            events,
            ..Default::default()
        },
        |ui| app.github_modal(ui.ctx()),
    );
    app.runtime.drain_actions();
    let texts = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(t) => {
                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
            }
            _ => None,
        })
        .collect();
    let platform = output.platform_output.clone();
    output.drop_without_applying_deltas();
    (texts, platform)
}

fn show(app: &mut Desktop<'_>, ctx: &egui::Context) -> Texts {
    frame(app, ctx, vec![]);
    frame(app, ctx, vec![]).0
}

fn has(texts: &Texts, text: &str) -> bool {
    texts.iter().any(|(shown, _)| shown == text)
}

fn at(texts: &Texts, text: &str) -> Pos2 {
    texts.iter().rev().find(|(shown, _)| shown == text).unwrap_or_else(|| panic!("{text} in {texts:?}")).1
}

fn click(app: &mut Desktop<'_>, ctx: &egui::Context, text: &str) {
    let texts = show(app, ctx);
    let pos = at(&texts, text);
    for pressed in [true, false] {
        frame(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
}

fn key(key: Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers }
}

/// Runs the application shortcuts with a terminal holding focus, and returns the events left for it.
fn route(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<egui::Event> {
    route_to(app, ctx, egui::Id::NULL.with("agent-terminal"), events)
}

fn route_to(
    app: &mut Desktop<'_>,
    ctx: &egui::Context,
    focused: egui::Id,
    events: Vec<egui::Event>,
) -> Vec<egui::Event> {
    ctx.memory_mut(|memory| memory.request_focus(focused));
    // Each key is released in the next frame, so the next press is no repeat.
    let releases = events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Key { key, physical_key, modifiers, .. } => Some(egui::Event::Key {
                key: *key,
                physical_key: *physical_key,
                pressed: false,
                repeat: false,
                modifiers: *modifiers,
            }),
            _ => None,
        })
        .collect();
    let mut remaining = Vec::new();
    ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
        app.shortcuts(ui.ctx());
        ui.interact(ui.max_rect(), focused, Sense::click());
        ui.interact(ui.max_rect(), diff_view::focus_id(), Sense::focusable_noninteractive());
        remaining = ui.input(|input| input.events.clone());
    })
    .drop_without_applying_deltas();
    ctx.run_ui(egui::RawInput { events: releases, ..Default::default() }, |ui| {
        ui.interact(ui.max_rect(), focused, Sense::click());
        ui.interact(ui.max_rect(), diff_view::focus_id(), Sense::focusable_noninteractive());
    })
    .drop_without_applying_deltas();
    app.runtime.drain_actions();
    remaining
}

/// Command on macOS, Ctrl elsewhere, as the platform reports it.
fn primary() -> egui::Modifiers {
    (if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL })
        | egui::Modifiers::COMMAND
}

/// The fake's pull requests: #7 by the viewer and passing, #8 blocked with admin bypass, #9 blocked without.
fn pulls(fake: &FakeGithub) {
    fake.add_pull(&repo(), fake::pull(7, "alice", &[], "CLEAN", false));
    fake.add_pull(&repo(), fake::pull(8, "bob", &["alice"], "BLOCKED", true));
    fake.add_pull(&repo(), fake::pull(9, "carol", &[], "BLOCKED", false));
}

#[test]
fn github_counter_lists_checks_and_follows_the_filter() {
    with_app("app::github::tests::github_counter_lists_checks_and_follows_the_filter", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        poll(app);
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::SignedOut);
        assert!(fake.seen().is_empty(), "no token, no request");
        store(app).put_github_app("github.com", TOKEN).unwrap();
        poll(app);
        assert_eq!(app.runtime.github.viewer("github.com"), Some("alice"));
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::Count { count: 1, failing: false });
        app.runtime.config.github.filter = atomics_core::github::Filter::Review;
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::Count { count: 1, failing: false });
        app.runtime.config.github.filter = atomics_core::github::Filter::All;
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::Count { count: 3, failing: false });
        // An unchanged list is a 304, and details are only fetched while the modal is open.
        poll(app);
        assert!(!fake.seen().iter().any(|seen| seen.path == "/graphql"));
        assert_eq!(fake.seen().last().unwrap().header("if-none-match").as_deref(), Some("\"v3\""));
        app.runtime.github_set_modal(true);
        poll(app);
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::Count { count: 3, failing: true });
        let texts = show(app, ctx);
        for text in [
            "GitHub · alice",
            "octo/widgets",
            "#7 Pull request 7 ◆",
            "alice · approved · clean",
            "bob · review required · blocked",
        ] {
            assert!(has(&texts, text), "{text} in {texts:?}");
        }
        assert!(has(&texts, "✓ build  ✓ lint"), "the selected pull request's checks: {texts:?}");
        assert!(!has(&texts, "Connect GitHub"));
        assert!(!texts.iter().any(|(text, _)| text.contains("PRIVATE")));
        fake.rate_limit(Some(90));
        poll(app);
        assert_eq!(app.runtime.github.backoff, Some(90));
        let wait = app.runtime.github.next_poll.unwrap() - Instant::now();
        assert!(wait > Duration::from_secs(80) && wait <= Duration::from_secs(90), "{wait:?}");
        assert!(app.runtime.github.error().unwrap().contains("rate limit"));
        assert_eq!(app.runtime.github.rows(atomics_core::github::Filter::All).len(), 3, "a rate limit keeps the list");
    });
}

#[test]
fn github_merge_and_admin_merge_confirm_once_and_only_when_allowed() {
    with_app("app::github::tests::github_merge_and_admin_merge_confirm_once_and_only_when_allowed", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        store(app).put_github_app("github.com", TOKEN).unwrap();
        app.runtime.github_set_modal(true);
        poll(app);
        // #7 is clean: Merge, no Admin merge, and the repository's two methods to pick from.
        app.client.github.selected = Some((repo(), 7));
        let texts = show(app, ctx);
        assert!(has(&texts, "Merge") && !has(&texts, "Admin merge") && has(&texts, "squash"));
        click(app, ctx, "Merge");
        assert!(fake.merges().is_empty(), "nothing merges before the confirmation");
        let texts = show(app, ctx);
        assert!(has(&texts, "Merge pull request?") && has(&texts, "Squash and merge"));
        click(app, ctx, "Merge");
        settle(app, "the merge", |app| !app.runtime.github.is_busy(&repo(), 7));
        let merges = fake.merges();
        assert_eq!(merges.len(), 1);
        assert_eq!(
            (merges[0].1, &merges[0].2["merge_method"], &merges[0].2["sha"]),
            (7, &json!("squash"), &json!("sha-7"))
        );
        assert_eq!(app.runtime.github.notice.as_deref(), Some("Merged octo/widgets#7"));
        poll(app);
        assert!(app.runtime.github.rows(atomics_core::github::Filter::All).iter().all(|row| row.pull.number != 7));
        // #9 is blocked and the viewer cannot bypass: neither button works.
        app.client.github.selected = Some((repo(), 9));
        let texts = show(app, ctx);
        assert!(!has(&texts, "Admin merge"));
        click(app, ctx, "Merge");
        assert!(app.client.github.confirm.is_none(), "a blocked pull request cannot start a merge");
        // #8 is blocked but the viewer may bypass the rules: Admin merge, confirmed once.
        app.client.github.selected = Some((repo(), 8));
        click(app, ctx, "Admin merge");
        let texts = show(app, ctx);
        assert!(
            has(&texts, "Admin merge pull request?") && has(&texts, "This bypasses the branch rules that block it.")
        );
        click(app, ctx, "Admin merge");
        settle(app, "the admin merge", |app| !app.runtime.github.is_busy(&repo(), 8));
        assert_eq!(fake.merges().iter().map(|merge| merge.1).collect::<Vec<_>>(), [7, 8]);
        assert_eq!(app.runtime.github.notice.as_deref(), Some("Admin merged octo/widgets#8"));
    });
}

#[test]
fn github_workflow_actions_fill_placeholders_and_show_the_latest_run() {
    with_app("app::github::tests::github_workflow_actions_fill_placeholders_and_show_the_latest_run", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        app.runtime.config.github.actions = vec![
            Action {
                label: "Ship it".into(),
                repo: "octo/widgets".into(),
                workflow: "ship.yml".into(),
                inputs: BTreeMap::from([
                    ("sha".into(), "{head_sha}".into()),
                    ("pr".into(), "{repo}#{pr}".into()),
                    ("note".into(), String::new()),
                ]),
                ..Action::default()
            },
            Action { label: "Nightly".into(), workflow: "nightly.yml".into(), scope: Scope::Repo, ..Action::default() },
            Action {
                label: "Elsewhere".into(),
                repo: "octo/other".into(),
                workflow: "x.yml".into(),
                ..Action::default()
            },
        ];
        store(app).put_github_app("github.com", TOKEN).unwrap();
        app.runtime.github_set_modal(true);
        poll(app);
        app.runtime.github_runs();
        settle(app, "the run status", |app| app.runtime.github.run(&repo(), "Nightly").is_some());
        app.client.github.selected = Some((repo(), 8));
        let texts = show(app, ctx);
        assert!(has(&texts, "Ship it") && has(&texts, "Nightly") && !has(&texts, "Elsewhere"));
        assert!(has(&texts, "never run"));
        click(app, ctx, "Ship it");
        assert!(fake.dispatches().is_empty(), "nothing runs before the confirmation");
        let texts = show(app, ctx);
        assert!(has(&texts, "Run workflow?") && has(&texts, "ref: main") && has(&texts, "sha-8"), "{texts:?}");
        let Some(Confirm::Dispatch { inputs, asked, .. }) = &mut app.client.github.confirm else {
            panic!("no confirmation")
        };
        assert_eq!(asked, &["note"]);
        inputs.insert("note".into(), "from the modal".into());
        click(app, ctx, "Run");
        settle(app, "the dispatch", |app| !app.runtime.github.is_dispatching(&repo(), "Ship it"));
        let dispatches = fake.dispatches();
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].1, "ship.yml");
        assert_eq!(
            dispatches[0].2,
            json!({ "ref": "main", "inputs": { "sha": "sha-8", "pr": "octo/widgets#8", "note": "from the modal" } })
        );
        assert_eq!(app.runtime.github.notice.as_deref(), Some("Started Ship it on octo/widgets at main"));
        assert!(has(&show(app, ctx), "queued"), "the latest run's status");
        // A repository action runs on the default branch, asked of GitHub.
        click(app, ctx, "Nightly");
        click(app, ctx, "Run");
        settle(app, "the repository dispatch", |app| !app.runtime.github.is_dispatching(&repo(), "Nightly"));
        assert_eq!(fake.dispatches()[1].2, json!({ "ref": "main", "inputs": {} }));
        let error = dispatch_confirm(
            &repo(),
            &Action { git_ref: "{pr}".into(), scope: Scope::Repo, ..Action::default() },
            None,
        );
        assert!(error.err().unwrap().contains("needs a pull request"));
    });
}

#[test]
fn github_pull_request_diff_loads_read_only_into_the_diff_viewer() {
    with_app("app::github::tests::github_pull_request_diff_loads_read_only_into_the_diff_viewer", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        let root = dir.path().join("worktree");
        std::fs::create_dir(&root).unwrap();
        app.runtime.sessions = vec![session("agent", AgentKind::Generic, AgentStatus::Working, &root)];
        app.runtime.active = Some("agent".into());
        store(app).put_github_app("github.com", TOKEN).unwrap();
        app.runtime.github_set_modal(true);
        poll(app);
        app.client.github.selected = Some((repo(), 7));
        let diffs_before: Vec<PathBuf> = app.runtime.diffs.keys().cloned().collect();
        click(app, ctx, "View diff");
        settle(app, "the diff", |app| app.runtime.github.pr_diff.as_ref().is_some_and(|view| view.prepared.is_some()));
        assert!(app.runtime.diff_open && app.runtime.panel == PanelMode::Diff);
        let prepared = app.runtime.github.pr_diff.as_ref().unwrap().prepared.clone().unwrap().unwrap();
        assert_eq!(prepared.diff.files.iter().map(|f| f.path()).collect::<Vec<_>>(), ["README.md", "src/pr7.rs"]);
        assert_eq!((prepared.additions, prepared.deletions), (3, 1));
        let draw = |app: &mut Desktop<'_>| {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                    ..Default::default()
                },
                |ui| app.diff_panel(ui, Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 900.0))),
            );
            let texts: Vec<String> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
            texts
        };
        let texts = draw(app);
        assert!(texts.iter().any(|t| t == "octo/widgets#7 Pull request 7"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "feature-7 → main · sha-7"), "the head the diff was loaded at");
        assert_eq!(app.runtime.github.pr_diff.as_ref().unwrap().head_sha.as_deref(), Some("sha-7"));
        assert!(texts.iter().any(|t| t.contains("new line from 7")), "the diff's lines are drawn");
        let state = &app.client.github.pr_view.as_ref().unwrap().1;
        assert_eq!(state.prepared.as_ref().unwrap().diff.files.len(), 2);
        let mut diffs_after: Vec<PathBuf> = app.runtime.diffs.keys().cloned().collect();
        diffs_after.retain(|path| !diffs_before.contains(path));
        assert!(diffs_after.iter().all(|path| path == &root), "no worktree diff for the pull request: {diffs_after:?}");
        assert!(!root.join(".git").exists(), "nothing was fetched into a worktree");
        // Selecting another session or worktree closes the pull request diff.
        app.runtime.active = None;
        app.runtime.diff_root = Some(root.clone());
        draw(app);
        assert!(app.runtime.github.pr_diff.is_none() && app.client.github.pr_view.is_none());
    });
}

#[test]
fn github_connect_uses_gh_after_one_confirmation() {
    with_app("app::github::tests::github_connect_uses_gh_after_one_confirmation", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        connect_to(app, &fake, dir.path());
        app.runtime.github.endpoints.gh = Some(fake::gh(dir.path(), Some(TOKEN)));
        app.runtime.github_set_modal(true);
        poll(app);
        click(app, ctx, "Connect GitHub");
        settle(app, "gh", |app| matches!(app.runtime.github.connect, Connect::ConfirmGh { .. }));
        assert!(store(app).github_app_token("github.com").unwrap().is_none(), "nothing is stored before Yes");
        let texts = show(app, ctx);
        assert!(has(&texts, "Use your gh account alice?"), "{texts:?}");
        click(app, ctx, "Use alice");
        settle(app, "the chip", |app| app.runtime.github.viewer("github.com").is_some());
        assert_eq!(store(app).github_app_token("github.com").unwrap().as_deref(), Some(TOKEN));
        assert!(store(app).credential(crate::credentials::GITHUB_APP).unwrap().is_none(), "never a relay credential");
        assert!(has(&show(app, ctx), "GitHub · alice"));
        assert!(opened(dir.path()).is_empty(), "gh needs no browser");
        // Disconnect deletes the stored token and links to GitHub's page for a full revoke.
        click(app, ctx, "GitHub · alice");
        click(app, ctx, "Disconnect");
        settle(app, "the disconnect", |app| app.runtime.github.viewer("github.com").is_none());
        assert!(store(app).github_app_token("github.com").unwrap().is_none());
        click(app, ctx, "Revoke on GitHub");
        settle(app, "the browser", |_| !opened(dir.path()).is_empty());
        assert_eq!(opened(dir.path()), ["https://github.com/settings/apps/authorizations"]);
        assert!(
            fake.seen().iter().all(|seen| !seen.path.contains("/grant") && !seen.path.contains("/token")),
            "no revoke API"
        );
    });
}

#[test]
fn github_connect_falls_back_to_the_github_app_device_flow() {
    with_app("app::github::tests::github_connect_falls_back_to_the_github_app_device_flow", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        fake.github_app();
        connect_to(app, &fake, dir.path());
        app.runtime.github_set_modal(true);
        poll(app);
        click(app, ctx, "Connect GitHub");
        settle(app, "the code", |app| matches!(app.runtime.github.connect, Connect::Device { .. }));
        let (texts, platform) = frame(app, ctx, vec![]);
        assert!(has(&texts, "WDJB-MJHT"));
        assert!(has(&texts, "The code is on your clipboard: paste it in the browser and press Authorize."));
        assert!(
            platform
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == "WDJB-MJHT")),
            "the code goes to the clipboard"
        );
        assert!(
            !frame(app, ctx, vec![]).1.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(_))),
            "once"
        );
        settle(app, "the browser", |_| !opened(dir.path()).is_empty());
        assert_eq!(opened(dir.path()), ["https://github.com/login/device"]);
        // The ATOMICS GitHub App: its client ID, and no scope, since the app's permissions apply.
        let request = fake.seen().into_iter().find(|seen| seen.path == "/login/device/code").unwrap();
        assert!(request.body.contains(&format!("client_id={}", atomics_core::github::DEFAULT_CLIENT_ID)));
        assert!(!request.body.contains("scope"), "{}", request.body);
        fake.authorize_device();
        settle(app, "the token", |app| app.runtime.github.viewer("github.com").is_some());
        let stored = store(app).github_app_grant("github.com").unwrap().unwrap();
        assert!(stored.token.starts_with("ghu_"));
        assert!(
            stored.expires.is_some(),
            "an app that kept expiration on: the expiry is stored, never a refresh token"
        );
        assert!(has(&show(app, ctx), "GitHub · alice"));
        // An [github] client_id for an OAuth app is sent the scopes it needs.
        app.runtime.config.github.client_id = "Ov23liOauthApp".into();
        app.runtime.github_disconnect("github.com".into());
        settle(app, "the disconnect", |app| app.runtime.github.viewer("github.com").is_none());
        app.runtime.github_connect();
        settle(app, "the code", |app| matches!(app.runtime.github.connect, Connect::Device { .. }));
        let request = fake.seen().into_iter().rev().find(|seen| seen.path == "/login/device/code").unwrap();
        assert!(request.body.contains("client_id=Ov23liOauthApp") && request.body.contains("scope=repo+workflow"));
        app.runtime.github_cancel_connect();
    });
}

#[test]
fn github_expired_or_refused_tokens_lead_back_to_connect() {
    with_app("app::github::tests::github_expired_or_refused_tokens_lead_back_to_connect", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // Past its expiry: removed without a request, and Connect GitHub shows.
        let expired = crate::credentials::AppGrant { token: fake.issue_user_token(), expires: Some(now - 10) };
        store(app).put_github_app_grant("github.com", &expired).unwrap();
        app.runtime.github_set_modal(true);
        poll(app);
        assert!(store(app).github_app_grant("github.com").unwrap().is_none());
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::SignedOut);
        assert!(has(&show(app, ctx), "Connect GitHub"));
        assert!(!fake.seen().iter().any(|seen| seen.path == "/user"), "an expired token is never sent");
        // A token GitHub refuses mid-session, here on a merge: removed, and the next poll asks to connect.
        let valid = crate::credentials::AppGrant { token: fake.issue_user_token(), expires: Some(now + 3600) };
        store(app).put_github_app_grant("github.com", &valid).unwrap();
        poll(app);
        assert_eq!(app.runtime.github.viewer("github.com"), Some("alice"));
        assert!(has(&show(app, ctx), "GitHub · alice"));
        fake.expire_tokens();
        app.runtime.github_merge(repo(), 7, MergeMethod::Squash, "sha-7".into(), false);
        settle(app, "the merge", |app| !app.runtime.github.is_busy(&repo(), 7));
        assert!(fake.merges().is_empty());
        assert!(store(app).github_app_grant("github.com").unwrap().is_none());
        poll(app);
        assert_eq!(app.runtime.github.counter(&app.runtime.config.github), Counter::SignedOut);
        let texts = show(app, ctx);
        assert!(has(&texts, "Connect GitHub") && !has(&texts, "GitHub · alice"), "{texts:?}");
        assert!(!fake.seen().iter().any(|seen| seen.body.contains("refresh_token")), "nothing is refreshed");
    });
}

#[test]
fn github_app_offers_to_install_where_it_is_missing() {
    with_app("app::github::tests::github_app_offers_to_install_where_it_is_missing", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        fake.install("octo", &["octo/widgets"]);
        connect_to(app, &fake, dir.path());
        app.runtime.config.github.repos = vec!["octo/widgets".into(), "octo/gears".into(), "other/thing".into()];
        let user = crate::credentials::AppGrant { token: fake.issue_user_token(), expires: None };
        store(app).put_github_app_grant("github.com", &user).unwrap();
        poll(app);
        assert!(app.runtime.github.installs("atomiskide").is_empty(), "coverage is checked while the modal is open");
        app.runtime.github_set_modal(true);
        poll(app);
        let texts = show(app, ctx);
        assert!(has(&texts, "Install ATOMICS on octo") && has(&texts, "Install ATOMICS on other"), "{texts:?}");
        assert_eq!(
            texts.iter().filter(|(text, _)| text.starts_with("Install ATOMICS")).count(),
            2,
            "one row per account"
        );
        click(app, ctx, "Install ATOMICS on other");
        settle(app, "the browser", |_| !opened(dir.path()).is_empty());
        assert_eq!(opened(dir.path()), ["https://github.com/apps/atomiskide/installations/new"]);
        // Installed everywhere: no rows.
        fake.install("octo", &["octo/gears"]);
        fake.install("other", &["other/thing"]);
        poll(app);
        assert!(!show(app, ctx).iter().any(|(text, _)| text.starts_with("Install ATOMICS")));
    });
}

#[test]
fn github_tokens_never_reach_logs_plans_doctor_or_the_screen() {
    with_app("app::github::tests::github_tokens_never_reach_logs_plans_doctor_or_the_screen", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        app.runtime.github.endpoints.gh = Some(fake::gh(dir.path(), Some(TOKEN)));
        app.runtime.github_set_modal(true);
        poll(app);
        app.runtime.github_connect();
        settle(app, "gh", |app| matches!(app.runtime.github.connect, Connect::ConfirmGh { .. }));
        let mut screen = show(app, ctx);
        app.runtime.github_use_gh(true);
        settle(app, "the chip", |app| app.runtime.github.viewer("github.com").is_some());
        poll(app);
        app.client.github.selected = Some((repo(), 7));
        screen.extend(show(app, ctx));
        app.runtime.github_merge(repo(), 7, MergeMethod::Squash, "sha-7".into(), false);
        app.runtime.github_view_diff(repo(), &Pull { number: 8, ..Pull::default() });
        settle(app, "the merge and the diff", |app| {
            !app.runtime.github.is_busy(&repo(), 7)
                && app.runtime.github.pr_diff.as_ref().is_some_and(|view| view.prepared.is_some())
        });
        let mut seen = String::new();
        for (text, _) in &screen {
            seen.push_str(text);
            seen.push('\n');
        }
        if let Connect::Failed(error) = &app.runtime.github.connect {
            seen.push_str(error);
        }
        seen.push_str(&crate::credentials::doctor_lines(&store(app)).join("\n"));
        seen.push_str(&serde_json::to_string(&app.runtime.config).unwrap());
        // Every file ATOMICS wrote: logs, plans, the store, the relay's usage and the config.
        let mut stack = vec![paths::atomics_home(), paths::config_file().parent().unwrap().to_path_buf()];
        let mut files = 0;
        while let Some(path) = stack.pop() {
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().filter_map(|entry| entry.ok().map(|e| e.path())));
            } else if !meta.is_file() {
                // Sockets and the jobserver FIFO hold no files; reading a FIFO would block.
                continue;
            } else if let Ok(bytes) = std::fs::read(&path) {
                files += 1;
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(TOKEN),
                    "the token was written to {}",
                    path.display()
                );
            }
        }
        assert!(files > 0);
        assert!(!seen.contains(TOKEN) && !seen.contains("PRIVATE"), "{seen}");
        assert!(store(app).github_app_token("github.com").unwrap().is_some(), "the keychain holds it");
        // Sandboxes and relay grants never get it: it is no relay credential.
        assert!(store(app).credential("github-app").unwrap().is_none());
    });
}

#[test]
fn github_shortcut_toggles_the_modal_before_the_terminal() {
    with_app("app::github::tests::github_shortcut_toggles_the_modal_before_the_terminal", |app, ctx| {
        // Typing in the same frame; another letter, since two presses of G at once would be a repeat.
        // Once the shortcut opens the controls they hold the keyboard; once it closes them, the
        // terminal gets its keys back.
        let typed = vec![key(Key::H, egui::Modifiers::NONE), egui::Event::Text("h".into())];
        let mut events = vec![key(Key::G, primary())];
        events.extend(typed.clone());
        assert!(route(app, ctx, events.clone()).is_empty(), "the open controls take the typing too");
        assert!(app.runtime.github.modal);
        assert_eq!(route(app, ctx, events), typed, "only the shortcut is withheld from the terminal");
        assert!(!app.runtime.github.modal, "pressing it again closes the modal");
        // Letters follow the layout, like the other [keys] bindings. On Dvorak the key at QWERTY's G
        // types I, so Ctrl/Command+I (Tab in a terminal) stays the terminal's, and the key labelled G
        // (at QWERTY's U) opens the controls.
        let dvorak = |key: Key, physical: Key| egui::Event::Key {
            key,
            physical_key: Some(physical),
            pressed: true,
            repeat: false,
            modifiers: primary(),
        };
        assert_eq!(route(app, ctx, vec![dvorak(Key::I, Key::G)]), [dvorak(Key::I, Key::G)]);
        assert!(!app.runtime.github.modal, "the key at G's place is not G");
        assert!(route(app, ctx, vec![dvorak(Key::G, Key::U)]).is_empty());
        assert!(app.runtime.github.modal, "the key labelled G opens the controls");
        assert!(route(app, ctx, vec![dvorak(Key::G, Key::U)]).is_empty());
        assert!(!app.runtime.github.modal);
        let plain = vec![key(Key::G, egui::Modifiers::NONE), egui::Event::Text("g".into())];
        assert_eq!(route(app, ctx, plain.clone()), plain, "a plain g is the terminal's");
        assert!(!app.runtime.github.modal);
        if !cfg!(target_os = "macos") {
            let ctrl_shift = key(Key::G, egui::Modifiers::CTRL | egui::Modifiers::SHIFT | egui::Modifiers::COMMAND);
            assert_eq!(route(app, ctx, vec![ctrl_shift.clone()]), [ctrl_shift], "Ctrl+Shift+G stays the terminal's");
            assert!(!app.runtime.github.modal);
        }
        // Configurable: another chord opens it and primary+g goes back to the terminal.
        app.runtime.config.keys.github = "alt+g".into();
        route(app, ctx, vec![key(Key::G, egui::Modifiers::ALT)]);
        assert!(app.runtime.github.modal);
        route(app, ctx, vec![key(Key::G, egui::Modifiers::ALT)]);
        assert!(!app.runtime.github.modal);
        let old = key(Key::G, primary());
        assert_eq!(route(app, ctx, vec![old.clone()]), [old]);
    });
}

#[test]
fn tickets_panel_filter_shortcut_conflicts_are_reported_and_fall_back() {
    for n in 1..=9 {
        let keys = atomics_core::config::KeyConfig { github: format!("alt+{n}"), ..Default::default() };
        let (chord, problem) = github_shortcut(&keys);
        assert_eq!(chord, crate::theme::shortcut("primary+g"));
        assert!(problem.unwrap().contains(&format!("the ticket filter chord for chip {n}")));
    }
}

#[test]
fn github_shortcut_conflicts_are_reported_and_fall_back() {
    let keys = atomics_core::config::KeyConfig::default();
    let (chord, problem) = github_shortcut(&keys);
    assert_eq!(chord, crate::theme::shortcut("primary+g"));
    assert!(problem.is_none());
    let expected = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
    assert_eq!(chord.unwrap().modifiers, expected);
    let agent = atomics_core::config::KeyConfig { github: "primary+1".into(), ..keys.clone() };
    let (chord, problem) = github_shortcut(&agent);
    assert_eq!(chord, crate::theme::shortcut("primary+g"));
    assert!(problem.as_deref().unwrap().contains("the agent chord for agent 1; using primary+g"), "{problem:?}");
    let other = atomics_core::config::KeyConfig { github: keys.toggle_diff.clone(), ..keys.clone() };
    assert!(github_shortcut(&other).1.unwrap().contains("is already [keys] toggle_diff"));
    let tickets = atomics_core::config::KeyConfig { github: keys.toggle_tickets.clone(), ..keys.clone() };
    assert!(github_shortcut(&tickets).1.unwrap().contains("is already [keys] toggle_tickets"));
    let taken = atomics_core::config::KeyConfig { toggle_diff: "primary+g".into(), ..keys.clone() };
    let (chord, problem) = github_shortcut(&taken);
    assert!(chord.is_none() && problem.unwrap().ends_with("no GitHub shortcut"));
    let broken = atomics_core::config::KeyConfig { github: "ctrl+".into(), ..keys.clone() };
    assert!(github_shortcut(&broken).1.unwrap().contains("is not a shortcut"));
    let config = Config { keys: agent, ..Config::default() };
    assert!(config_problems(&config).unwrap().contains("[keys] github"));
    assert!(config_problems(&Config::default()).is_none());
}

#[test]
fn github_controls_take_focus_immediately_and_restore_the_previous_pane() {
    with_app("app::github::tests::github_controls_take_focus_immediately_and_restore_the_previous_pane", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        store(app).put_github_app("github.com", TOKEN).unwrap();
        poll(app);
        let terminal = egui::Id::unique("focus-test-terminal");
        let agent = egui::Id::unique("focus-test-agent");
        let mut draft = String::from("keep this draft");
        let mut run = |app: &mut Desktop<'_>, events: Vec<egui::Event>| {
            let mut received = Vec::new();
            ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                app.shortcuts(ui.ctx());
                let response = ui.interact(ui.max_rect(), terminal, Sense::click());
                if response.has_focus() {
                    received = ui.input(|input| input.events.clone());
                }
                ui.add(egui::TextEdit::singleline(&mut draft).id(agent));
                app.github_modal(ui.ctx());
            })
            .drop_without_applying_deltas();
            received
        };
        run(app, vec![]);
        for previous in [terminal, agent] {
            for close in [key(Key::Escape, egui::Modifiers::NONE), key(Key::G, primary())] {
                ctx.memory_mut(|memory| memory.request_focus(previous));
                app.client.github.selected = None;
                assert!(run(app, vec![key(Key::G, primary())]).is_empty());
                assert!(app.runtime.github.modal);
                assert!(
                    ctx.memory(|memory| memory.has_focus(github_focus_id())),
                    "controls take focus from {previous:?}, got {:?}",
                    ctx.memory(|memory| memory.focused())
                );
                // Release the opening chord; subsequent keys arrive without a click or focus reset.
                run(
                    app,
                    vec![egui::Event::Key {
                        key: Key::G,
                        physical_key: Some(Key::G),
                        pressed: false,
                        repeat: false,
                        modifiers: primary(),
                    }],
                );
                assert!(run(app, vec![key(Key::ArrowDown, egui::Modifiers::NONE)]).is_empty());
                assert_eq!(app.client.github.selected, Some((repo(), 8)), "plain Down selects the next PR");
                // Even if a background pane requests focus again, modal input stays in the controls.
                ctx.memory_mut(|memory| memory.request_focus(previous));
                assert!(
                    run(
                        app,
                        vec![
                            egui::Event::Text("x".into()),
                            egui::Event::Paste("paste".into()),
                            key(Key::Backspace, egui::Modifiers::NONE)
                        ]
                    )
                    .is_empty()
                );
                assert!(run(app, vec![close.clone()]).is_empty(), "closing key never reaches the terminal");
                assert!(!app.runtime.github.modal);
                assert!(ctx.memory(|memory| memory.has_focus(previous)), "closing restores {previous:?}");
                // Release the closing key so reopening is a fresh press.
                if let egui::Event::Key { key, physical_key, modifiers, .. } = close {
                    run(app, vec![egui::Event::Key { key, physical_key, modifiers, pressed: false, repeat: false }]);
                }
            }
        }
        assert_eq!(draft, "keep this draft", "the agent's draft receives no modal input");
    });
}

#[test]
fn github_controls_keep_workflow_input_focus() {
    with_app("app::github::tests::github_controls_keep_workflow_input_focus", |app, ctx| {
        app.runtime.github_set_modal(true);
        app.client.github.confirm = Some(Confirm::Dispatch {
            repo: repo(),
            action: Action { label: "Release".into(), workflow: "release.yml".into(), ..Action::default() },
            git_ref: Some("main".into()),
            inputs: BTreeMap::from([("note".into(), "workflow draft".into())]),
            asked: vec!["note".into()],
        });
        click(app, ctx, "workflow draft");
        let focused = app.client.github.input_focus.expect("the clicked workflow input");
        ctx.run_ui(
            egui::RawInput {
                events: vec![key(Key::M, egui::Modifiers::NONE), egui::Event::Text("m".into())],
                ..Default::default()
            },
            |ui| {
                app.shortcuts(ui.ctx());
                app.github_modal(ui.ctx());
            },
        )
        .drop_without_applying_deltas();
        assert!(ctx.memory(|memory| memory.has_focus(focused)), "the input keeps focus");
        let Some(Confirm::Dispatch { inputs, .. }) = &app.client.github.confirm else {
            panic!("typing M must not open a merge confirmation");
        };
        assert!(inputs["note"].contains('m'), "M is typed into the workflow input");
    });
}

#[test]
fn github_modal_keys_select_open_view_merge_and_close() {
    with_app("app::github::tests::github_modal_keys_select_open_view_merge_and_close", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        let gears = RepoId::parse("octo/gears").unwrap();
        fake.add_pull(&gears, fake::pull(3, "alice", &[], "CLEAN", false));
        connect_to(app, &fake, dir.path());
        app.runtime.config.github.repos = vec!["octo/widgets".into(), "octo/gears".into()];
        store(app).put_github_app("github.com", TOKEN).unwrap();
        app.runtime.github_set_modal(true);
        poll(app);
        let selected =
            |app: &Desktop<'_>| app.client.github.selected.clone().map(|(repo, number)| (repo.to_string(), number));
        let press = |app: &mut Desktop<'_>, code: Key, modifiers: egui::Modifiers| {
            assert!(route(app, ctx, vec![key(code, modifiers)]).is_empty(), "{code:?} is the modal's");
            selected(app)
        };
        let at = |repo: &str, number: u64| Some((repo.to_string(), number));
        assert_eq!(press(app, Key::ArrowDown, egui::Modifiers::SHIFT), at("octo/widgets", 8));
        assert_eq!(press(app, Key::ArrowDown, egui::Modifiers::SHIFT), at("octo/widgets", 9));
        assert_eq!(press(app, Key::ArrowDown, egui::Modifiers::SHIFT), at("octo/gears", 3), "across repositories");
        assert_eq!(press(app, Key::ArrowDown, egui::Modifiers::SHIFT), at("octo/gears", 3));
        assert_eq!(press(app, Key::PageUp, egui::Modifiers::NONE), at("octo/widgets", 7), "the previous repository");
        assert_eq!(press(app, Key::ArrowUp, egui::Modifiers::SHIFT), at("octo/widgets", 7));
        assert_eq!(press(app, Key::PageDown, egui::Modifiers::NONE), at("octo/gears", 3), "the next repository");
        assert_eq!(press(app, Key::PageUp, egui::Modifiers::NONE), at("octo/widgets", 7));
        assert_eq!(press(app, Key::ArrowDown, egui::Modifiers::SHIFT), at("octo/widgets", 8));
        assert_eq!(press(app, Key::PageUp, egui::Modifiers::NONE), at("octo/widgets", 7), "this repository's first");
        press(app, Key::Enter, egui::Modifiers::NONE);
        settle(app, "the browser", |_| !opened(dir.path()).is_empty());
        assert_eq!(opened(dir.path()), ["https://github.com/octo/widgets/pull/7"]);
        let remaining = route(app, ctx, vec![key(Key::D, egui::Modifiers::NONE), egui::Event::Text("d".into())]);
        assert!(remaining.is_empty(), "neither D nor its text reaches the terminal: {remaining:?}");
        assert_eq!(app.runtime.github.pr_diff.as_ref().map(|view| view.number), Some(7));
        // Letters follow the layout: on AZERTY the key at QWERTY's M types a comma, and M sits at its
        // semicolon. The comma does nothing, and neither key's text reaches the terminal.
        let typed = |key: Key, physical: Key, text: &str| {
            let modifiers = egui::Modifiers::NONE;
            vec![
                egui::Event::Key { key, physical_key: Some(physical), pressed: true, repeat: false, modifiers },
                egui::Event::Text(text.into()),
            ]
        };
        assert!(route(app, ctx, typed(Key::Comma, Key::M, ",")).is_empty());
        assert!(app.client.github.confirm.is_none(), "a comma is no M");
        assert!(route(app, ctx, typed(Key::M, Key::Semicolon, "m")).is_empty());
        assert!(matches!(app.client.github.confirm, Some(Confirm::Merge { number: 7, admin: false, .. })));
        assert!(fake.merges().is_empty(), "M only asks");
        // The confirmation holds the keyboard too: text, a paste, Backspace and Shift+↓ reach nothing.
        let none = egui::Modifiers::NONE;
        let typing = vec![
            egui::Event::Text("half a prompt".into()),
            egui::Event::Paste("pasted".into()),
            key(Key::Backspace, none),
            key(Key::ArrowDown, egui::Modifiers::SHIFT),
        ];
        assert!(route(app, ctx, typing).is_empty(), "nothing reaches the terminal behind the confirmation");
        assert!(matches!(app.client.github.confirm, Some(Confirm::Merge { number: 7, .. })), "it stays on #7");
        // Esc cancels the confirmation only; the agent never receives it.
        assert!(route(app, ctx, vec![key(Key::Escape, none)]).is_empty(), "Esc is the confirmation's");
        assert!(app.client.github.confirm.is_none() && app.runtime.github.modal);
        assert!(fake.merges().is_empty());
        // Enter confirms, and is not the agent's either.
        route(app, ctx, vec![key(Key::M, none)]);
        assert!(route(app, ctx, vec![key(Key::Enter, none)]).is_empty(), "Enter is the confirmation's");
        assert!(app.client.github.confirm.is_none());
        settle(app, "the merge", |app| !app.runtime.github.is_busy(&repo(), 7));
        assert_eq!(fake.merges().iter().map(|merge| merge.1).collect::<Vec<_>>(), [7]);
        poll(app);
        // #7 is gone, so #8 is selected: it is blocked, so M offers no merge.
        route(app, ctx, vec![key(Key::M, none)]);
        assert!(app.client.github.confirm.is_none(), "#8 is blocked: no merge");
        let escape = key(Key::Escape, none);
        assert!(app.runtime.github.notice.as_deref().unwrap().contains("cannot merge yet"));
        assert!(route(app, ctx, vec![escape.clone()]).is_empty());
        assert!(!app.runtime.github.modal);
        // Closed, the modal takes nothing.
        let typed = vec![key(Key::D, egui::Modifiers::NONE), egui::Event::Text("d".into())];
        assert_eq!(route(app, ctx, typed.clone()), typed);
    });
}

#[test]
fn github_pull_request_diff_takes_the_diff_keys() {
    with_app("app::github::tests::github_pull_request_diff_takes_the_diff_keys", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        store(app).put_github_app("github.com", TOKEN).unwrap();
        poll(app);
        let pull = app.runtime.github.rows(atomics_core::github::Filter::All)[0].pull.clone();
        app.runtime.github_view_diff(repo(), &pull);
        settle(app, "the diff", |app| app.runtime.github.pr_diff.as_ref().is_some_and(|view| view.prepared.is_some()));
        // One frame of the panel creates its view state.
        ctx.run_ui(egui::RawInput::default(), |ui| {
            app.diff_panel(ui, Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0)));
        })
        .drop_without_applying_deltas();
        let focused =
            |app: &Desktop<'_>| app.client.github.pr_view.as_ref().and_then(|(_, state)| state.focused.clone());
        for expected in ["README.md", "src/pr7.rs", "src/pr7.rs"] {
            assert!(route(app, ctx, vec![key(Key::ArrowDown, egui::Modifiers::SHIFT)]).is_empty());
            assert_eq!(focused(app).as_deref(), Some(expected));
            assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())));
        }
        assert!(route(app, ctx, vec![key(Key::ArrowUp, egui::Modifiers::SHIFT)]).is_empty());
        assert_eq!(focused(app).as_deref(), Some("README.md"));
        // With the diff focused, PageDown walks the hunks into the next file.
        let page_down = || vec![key(Key::PageDown, egui::Modifiers::NONE)];
        assert!(route_to(app, ctx, diff_view::focus_id(), page_down()).is_empty());
        assert!(route_to(app, ctx, diff_view::focus_id(), page_down()).is_empty());
        assert_eq!(focused(app).as_deref(), Some("src/pr7.rs"));
        assert!(app.client.diff_views.values().all(|state| state.focused.is_none()), "no worktree diff moved");
    });
}

#[test]
fn github_merge_sends_the_reviewed_head_and_a_moved_branch_is_refused() {
    with_app("app::github::tests::github_merge_sends_the_reviewed_head_and_a_moved_branch_is_refused", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let fake = FakeGithub::start(TOKEN);
        pulls(&fake);
        connect_to(app, &fake, dir.path());
        store(app).put_github_app("github.com", TOKEN).unwrap();
        app.runtime.github_set_modal(true);
        poll(app);
        app.client.github.selected = Some((repo(), 7));
        // Without a loaded diff, Merge sends the head the list shows, and says so.
        click(app, ctx, "Merge");
        assert!(has(&show(app, ctx), "Head sha-7; its diff was not loaded here"));
        app.client.github.confirm = None;
        // View diff records the head it was loaded at.
        let loaded = |sha: &'static str| {
            move |app: &Desktop<'_>| {
                app.runtime.github.pr_diff.as_ref().is_some_and(|view| view.head_sha.as_deref() == Some(sha))
            }
        };
        click(app, ctx, "View diff");
        settle(app, "the diff at sha-7", loaded("sha-7"));
        // A push after the review: the list shows the new head, the merge still sends the reviewed one.
        fake.push(&repo(), 7, "sha-7-pushed");
        poll(app);
        let row = |app: &Desktop<'_>| {
            app.runtime.github.rows(atomics_core::github::Filter::All).into_iter().find(|row| row.pull.number == 7)
        };
        assert_eq!(row(app).unwrap().pull.head_sha, "sha-7-pushed");
        click(app, ctx, "Merge");
        let texts = show(app, ctx);
        assert!(has(&texts, "Head sha-7, the commit whose diff you loaded"), "{texts:?}");
        assert!(
            texts.iter().any(|(text, _)| text.starts_with("The branch has moved to sha-7-p since")),
            "the moved head is named: {texts:?}"
        );
        click(app, ctx, "Merge");
        settle(app, "the refused merge", |app| !app.runtime.github.is_busy(&repo(), 7));
        assert!(fake.merges().is_empty(), "nothing merged");
        let sent: Vec<serde_json::Value> = fake
            .seen()
            .iter()
            .filter(|seen| seen.method == "PUT")
            .map(|seen| serde_json::from_str(&seen.body).unwrap())
            .collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["sha"], json!("sha-7"), "the reviewed head, not the pushed one");
        let notice = app.runtime.github.notice.clone().unwrap();
        assert!(notice.contains("409") && notice.contains("Head branch was modified"), "{notice}");
        assert!(row(app).is_some(), "the pull request is still open");
        // Loading the diff again reviews the new head, which then merges.
        click(app, ctx, "View diff");
        settle(app, "the diff at the pushed head", loaded("sha-7-pushed"));
        click(app, ctx, "Merge");
        assert!(has(&show(app, ctx), "Head sha-7-p, the commit whose diff you loaded"));
        click(app, ctx, "Merge");
        settle(app, "the merge", |app| !app.runtime.github.is_busy(&repo(), 7));
        let merges = fake.merges();
        assert_eq!(merges.iter().map(|merge| merge.1).collect::<Vec<_>>(), [7]);
        assert_eq!(merges[0].2["sha"], json!("sha-7-pushed"));
    });
}

#[test]
fn github_config_problems_are_reported_while_the_config_still_loads() {
    with_app("app::github::tests::github_config_problems_are_reported_while_the_config_still_loads", |app, ctx| {
        let path = app.runtime.config_path.clone();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            "\n[[github.actions]]\nlabel = \"Release\"\nworkflow = \"release.yml\"\nscope = \"repo\"\ninputs = { pr = \"{pr}\" }\n",
        );
        std::fs::write(&path, text).unwrap();
        app.runtime.config_checked = Instant::now() - Duration::from_secs(2);
        app.runtime.runtime_tick(ctx);
        assert_eq!(app.runtime.config.github.actions.len(), 1, "the config loaded and applies");
        assert!(app.runtime.config_error.is_none(), "{:?}", app.runtime.config_error);
        let problems = app.runtime.config_problems.clone().unwrap();
        assert!(problems.contains("[[github.actions]] Release: {pr} needs a pull request"), "{problems}");
        let got = app.runtime.config_get(None, None).unwrap();
        assert_eq!(got["running_on_last_good_config"], json!(false), "the chief is not told it failed to load");
        assert!(got["load_error"].is_null());
    });
}
