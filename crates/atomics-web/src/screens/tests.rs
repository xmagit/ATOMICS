//! Terminals against the fake host: following, the lease, typing and its echo, no replay, history.

use atomics_core::lease::GridSize;
use atomics_core::noise::Access;
use atomics_core::screen::Screen;
use atomics_core::wire::{Message, Topic};

use super::{Lost, MAX_REQUESTS, MAX_TYPED_BYTES, MAX_WAITING_BYTES, Refused};
use crate::driver::Status;
use crate::fake_host;
use crate::world::World;

fn live() -> World {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    world
}

/// One frame that shows `session`'s terminal, as the views do.
fn show(world: &mut World, session: &str) {
    world.driver.remote.show_screen(session, true);
    world.driver.remote.end_frame();
    world.driver.flush();
    world.pump();
}

/// The user's tap on the terminal: the one thing that asks for the lease.
fn acquire(world: &mut World, session: &str, takeover: bool) {
    world.driver.remote.acquire(session, takeover);
    world.driver.flush();
    world.pump();
}

fn type_text(world: &mut World, session: &str, text: &str) -> Result<(), Refused> {
    let typed = world.driver.remote.type_text(session, text);
    world.driver.flush();
    world.pump();
    typed
}

fn screen_text(world: &World, session: &str) -> String {
    world.driver.remote.with_screen(session, |s| s.map(|(screen, _)| screen.text()).unwrap_or_default())
}

fn inputs(world: &World) -> Vec<String> {
    world
        .sent()
        .iter()
        .filter_map(|m| match m {
            Message::TerminalInput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn acquires(world: &World) -> usize {
    world.sent().iter().filter(|m| matches!(m, Message::LeaseAcquire { .. })).count()
}

#[test]
fn a_shown_terminal_is_followed_and_text_typed_after_the_tap_waits_for_the_grant_and_echoes() {
    let mut world = live();
    show(&mut world, "s1");
    assert!(world.sent().iter().any(|m| matches!(m, Message::Subscribe { topics }
        if topics == &[Topic::Screen { session: "s1".into(), focused: true }])));
    assert_eq!(screen_text(&world, "s1"), "$");
    assert_eq!(world.driver.remote.lease("s1").holder, None, "nobody types yet");
    assert_eq!(acquires(&world), 0, "following never takes it");

    assert_eq!(type_text(&mut world, "s1", "early"), Err(Refused::NoLease), "typing never asks for the lease");
    assert_eq!(acquires(&world), 0);
    assert!(inputs(&world).is_empty(), "and nothing of it is sent or kept");

    // The tap asks; what is typed before the grant comes waits for it.
    world.driver.remote.acquire("s1", false);
    assert_eq!(world.driver.remote.type_text("s1", "ls\r"), Ok(()));
    world.driver.flush();
    world.pump();
    assert!(world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { takeover: false, .. })));
    assert_eq!(world.host.terminals["s1"].typed, "ls\r", "the text waited for the grant, then went");
    assert_eq!(screen_text(&world, "s1"), "$ ls", "the echo");
    let lease = world.driver.remote.lease("s1");
    assert!(lease.yours && lease.holder.as_deref() == Some("Safari on iPhone"), "{lease:?}");

    assert_eq!(type_text(&mut world, "s1", "pwd"), Ok(()));
    assert_eq!(inputs(&world), ["ls\r", "pwd"]);
}

/// What the host says of `session`'s lease as it stands, as it might in the moment before it reads a release.
fn state_of_the_lease(world: &World, session: &str) -> atomics_core::lease::LeaseState {
    use atomics_core::wire::Seq;

    atomics_core::lease::LeaseState {
        session: session.into(),
        holder: world.driver.remote.lease(session).holder,
        yours: true,
        lease: world.host.terminals[session].lease.as_ref().map(|l| Seq(l.token)),
        next: Some(Seq(1)),
        ttl_ms: Some(30_000),
        resized: false,
        reason: Some(atomics_core::lease::LeaseReason::Granted),
    }
}

/// The host's word on a lease, reaching this client now.
fn tell(world: &World, state: &atomics_core::lease::LeaseState) {
    let envelope = atomics_core::wire::Envelope::host(&world.host.epoch, Message::LeaseState(state.clone()));
    world.driver.remote.receive(envelope);
}

#[test]
fn asking_again_before_the_host_has_answered_a_release_settles_on_a_new_lease() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    assert_eq!(type_text(&mut world, "s1", "a"), Ok(()));
    let earlier = state_of_the_lease(&world, "s1");
    let (old, holder) = (earlier.lease, earlier.holder.clone());
    assert!(old.is_some() && holder.is_some());

    // Let go, and nothing has reached the host: nobody holds it here, so nothing offers to take it over.
    world.driver.remote.release("s1");
    let lease = world.driver.remote.lease("s1");
    assert!(!lease.yours && lease.holder.is_none() && !lease.acquiring, "{lease:?}");

    // A click back asks at once, and what is typed waits for the answer.
    world.driver.remote.acquire("s1", false);
    assert_eq!(world.driver.remote.type_text("s1", "b"), Ok(()));
    tell(&world, &earlier);
    let lease = world.driver.remote.lease("s1");
    assert!(lease.acquiring && !lease.yours && lease.lost.is_none(), "the request stands: {lease:?}");

    // The host reads both, in order, and it settles on a new lease with the text under it.
    world.driver.flush();
    world.pump();
    let kinds: Vec<&Message> = world
        .sent()
        .into_iter()
        .filter(|m| matches!(m, Message::LeaseRelease { .. } | Message::LeaseAcquire { .. }))
        .collect();
    assert!(matches!(kinds[..], [_, Message::LeaseRelease { .. }, Message::LeaseAcquire { .. }]), "{kinds:?}");
    assert_eq!(world.host.terminals["s1"].typed, "ab");
    let lease = world.driver.remote.lease("s1");
    assert!(lease.yours && !lease.acquiring && lease.holder == holder, "{lease:?}");
    let new = world.host.terminals["s1"].lease.as_ref().map(|l| l.token);
    assert_ne!(new, old.map(|l| l.0), "under a new token");
}

#[test]
fn a_state_from_before_a_release_changes_nothing_and_the_hosts_answer_settles_it() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    let earlier = state_of_the_lease(&world, "s1");
    world.driver.remote.release("s1");
    tell(&world, &earlier);
    let lease = world.driver.remote.lease("s1");
    assert!(!lease.yours && lease.holder.is_none() && !lease.acquiring && lease.lost.is_none(), "{lease:?}");
    assert_eq!(world.driver.remote.type_text("s1", "x"), Err(Refused::NoLease), "nothing goes under the old lease");

    world.driver.flush();
    world.pump();
    let lease = world.driver.remote.lease("s1");
    assert!(!lease.yours && lease.holder.is_none() && !lease.acquiring, "{lease:?}");
    assert!(world.host.terminals["s1"].lease.is_none());
}

#[test]
fn the_first_keys_survive_the_lease_state_a_subscription_starts_with() {
    let mut world = live();
    // A desktop take back long ago: the state a subscription starts with still names its reason.
    world.host.terminals.get_mut("s1").unwrap().reason = Some(atomics_core::lease::LeaseReason::Desktop);
    // The tap comes before the subscription's first lease state: that state, saying nobody types, arrives
    // after the request and must not cancel it or what waits for it.
    world.driver.remote.show_screen("s1", true);
    world.driver.remote.end_frame();
    world.driver.remote.acquire("s1", false);
    assert_eq!(world.driver.remote.type_text("s1", "ls\r"), Ok(()));
    world.driver.flush();
    world.pump();
    assert_eq!(world.host.terminals["s1"].typed, "ls\r");
    let lease = world.driver.remote.lease("s1");
    assert!(lease.yours && lease.lost.is_none(), "{lease:?}");
}

#[test]
fn after_the_desktop_takes_the_lease_back_typing_neither_types_nor_asks_for_it() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    assert_eq!(type_text(&mut world, "s1", "a"), Ok(()));
    let back = world.host.desktop_types("s1", "b");
    world.deliver(back);
    let lease = world.driver.remote.lease("s1");
    assert_eq!((lease.yours, lease.holder, lease.lost), (false, None, Some(Lost::Desktop)));
    assert_eq!(screen_text(&world, "s1"), "$ ab");

    assert_eq!(type_text(&mut world, "s1", "c"), Err(Refused::NoLease), "text in flight is dropped");
    assert_eq!(acquires(&world), 1, "and does not ask for the free lease");
    assert_eq!(world.host.terminals["s1"].typed, "ab");
    assert!(world.host.terminals["s1"].lease.is_none());

    // A new tap asks again.
    acquire(&mut world, "s1", false);
    assert_eq!(type_text(&mut world, "s1", "c"), Ok(()));
    assert_eq!(acquires(&world), 2);
    assert_eq!(world.host.terminals["s1"].typed, "abc");
    assert_eq!(world.driver.remote.lease("s1").lost, None);
}

#[test]
fn another_client_typing_keeps_input_out_until_the_user_takes_over() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "a").unwrap();
    let taken = world.host.other_takes("s1", "Firefox on Linux");
    world.deliver(taken);
    let lease = world.driver.remote.lease("s1");
    assert_eq!(
        (lease.yours, lease.holder.as_deref(), lease.lost),
        (false, Some("Firefox on Linux"), Some(Lost::TakenOver))
    );
    assert_eq!(type_text(&mut world, "s1", "x"), Err(Refused::NoLease), "nothing is typed while another client types");
    acquire(&mut world, "s1", false);
    assert_eq!(world.driver.remote.lease("s1").lost, Some(Lost::Held), "asking without takeover is refused");

    acquire(&mut world, "s1", true);
    assert!(world.driver.remote.lease("s1").yours, "a takeover the user asked for");
    assert_eq!(type_text(&mut world, "s1", "y"), Ok(()));
    assert_eq!(world.host.terminals["s1"].typed, "ay", "x was never sent");
}

#[test]
fn nothing_typed_is_kept_or_replayed_across_a_disconnect() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "one").unwrap();
    world.drop_socket(1006);
    assert!(!world.driver.remote.lease("s1").yours, "the lease went with the channel");
    assert_eq!(type_text(&mut world, "s1", "lost"), Err(Refused::NoLease), "offline: nothing is typed");
    world.driver.remote.with_screen("s1", |s| assert!(s.unwrap().1, "the screen shows, stale"));

    // Back: a fresh ticket, hello, the screen subscribed again from the next frame.
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    world.host.received.clear();
    show(&mut world, "s1");
    world.driver.remote.with_screen("s1", |s| assert!(!s.unwrap().1, "a fresh snapshot"));
    assert!(inputs(&world).is_empty(), "nothing from before or during the outage");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "two").unwrap();
    assert_eq!(world.host.terminals["s1"].typed, "onetwo");
}

#[test]
fn input_refused_as_slow_client_is_typed_again_once_the_program_reads() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "a").unwrap();
    world.host.terminals.get_mut("s1").unwrap().stalled = true;
    type_text(&mut world, "s1", "bc").unwrap();
    assert_eq!(world.host.terminals["s1"].typed, "a");
    type_text(&mut world, "s1", "d").unwrap();
    world.host.terminals.get_mut("s1").unwrap().stalled = false;
    let output = world.host.output("s1", "!");
    world.deliver(output);
    assert_eq!(world.host.terminals["s1"].typed, "abcd", "in the order typed");
}

#[test]
fn a_slow_program_gets_at_most_a_mebibyte_again_per_change_and_more_than_may_wait_is_refused() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "a").unwrap();
    let typed_bytes = |world: &World| {
        world
            .sent()
            .iter()
            .map(|m| if let Message::TerminalInput { data, .. } = m { data.len() } else { 0 })
            .sum::<usize>()
    };

    let over = "x".repeat(MAX_TYPED_BYTES + 1);
    let before = typed_bytes(&world);
    assert_eq!(type_text(&mut world, "s1", &over), Err(Refused::TooMuch(MAX_TYPED_BYTES)), "a paste over the limit");
    assert_eq!(typed_bytes(&world), before, "is not typed at all");

    // Two pastes of a mebibyte, sent before the program stopped reading the first: both come back.
    world.host.terminals.get_mut("s1").unwrap().stalled = true;
    let paste = format!("{}\r", "y".repeat(1023)).repeat(MAX_TYPED_BYTES / 1024);
    assert_eq!(paste.len(), MAX_TYPED_BYTES);
    assert_eq!(world.driver.remote.type_text("s1", &paste), Ok(()));
    assert_eq!(world.driver.remote.type_text("s1", &paste), Ok(()));
    world.driver.flush();
    world.pump();
    assert_eq!(world.host.terminals["s1"].typed, "a");
    assert_eq!(type_text(&mut world, "s1", "z"), Err(Refused::TooMuch(MAX_TYPED_BYTES)), "no more waits behind them");

    // A change while the program still does not read: one mebibyte is typed again, not all of it.
    let before = typed_bytes(&world);
    let output = world.host.output("s1", "!");
    world.deliver(output);
    let again = typed_bytes(&world) - before;
    assert!(again > 0 && again <= MAX_TYPED_BYTES, "{again} bytes typed again at one change");
    assert_eq!(world.host.terminals["s1"].typed, "a");

    // Reading again: the rest follows, a mebibyte per change, in order.
    world.host.terminals.get_mut("s1").unwrap().stalled = false;
    let output = world.host.output("s1", "!");
    world.deliver(output);
    assert!(world.host.terminals["s1"].typed == format!("a{paste}{paste}"), "everything, once, in order");
    assert_eq!(type_text(&mut world, "s1", "z"), Ok(()));
}

#[test]
fn the_requests_kept_to_match_refusals_are_bounded() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    // A key at a time, never answered when applied: more than the bound.
    for _ in 0..MAX_REQUESTS + 100 {
        assert_eq!(world.driver.remote.type_text("s1", "k"), Ok(()));
    }
    let kept = |world: &World| world.driver.remote.with_screens(|s| s.requests.len());
    assert_eq!(kept(&world), MAX_REQUESTS);
    world.driver.flush();
    world.pump();
    assert_eq!(world.host.terminals["s1"].typed.len(), MAX_REQUESTS + 100);
    // A lease state that says what was applied forgets those; its answer forgets the renew.
    world.driver.remote.renew("s1");
    world.driver.flush();
    world.pump();
    assert_eq!(kept(&world), 0);
}

#[test]
fn renew_release_and_resize_carry_request_ids_and_a_refusal_of_one_is_quiet() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    world.driver.remote.renew("s1");
    world.driver.remote.resize("s1", Some(GridSize::new(40, 10)));
    world.driver.remote.show_screen("chief", true);
    world.driver.remote.end_frame();
    let frames = world.driver.remote.take_outgoing();
    for name in ["renew", "resize", "release"] {
        let frame = frames
            .iter()
            .find(|e| match name {
                "renew" => matches!(e.message, Message::LeaseRenew { .. }),
                "resize" => matches!(e.message, Message::TerminalResize { .. }),
                _ => matches!(e.message, Message::LeaseRelease { .. }),
            })
            .unwrap_or_else(|| panic!("a {name}"));
        assert!(frame.request_id.as_deref().is_some_and(|id| id.starts_with("t-")), "{name}: {frame:?}");
    }

    // A renew that crosses the desktop's take back is refused, and says nothing: the lease state does.
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    world.driver.remote.renew("s1");
    let back = world.host.desktop_types("s1", "b");
    world.deliver(back);
    assert!(world.sent().iter().any(|m| matches!(m, Message::LeaseRenew { .. })));
    assert_eq!(world.driver.remote.notice(), None);
    assert_eq!(world.driver.remote.lease("s1").lost, Some(Lost::Desktop));
}

#[test]
fn history_is_kept_as_lines_scroll_off_and_fetched_when_the_view_reaches_it() {
    let mut world = live();
    show(&mut world, "s1");
    // Ten lines at a time: each change scrolls fewer lines than the screen holds.
    for start in [1, 11, 21] {
        let lines: String = (start..start + 10).map(|n| format!("line {n}\r")).collect();
        let output = world.host.output("s1", &lines);
        world.deliver(output);
    }
    let first = world.driver.remote.with_screen("s1", |s| s.unwrap().0.first_line);
    let mut view = Screen::blank(0, 0);
    assert!(world.driver.remote.fill_screen("s1", first - 3, 3, &mut view));
    assert_eq!(view.text(), "line 17\nline 18\nline 19", "kept as they scrolled off");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::HistoryGet(_))));

    // A new connection's snapshot starts over: the view asks the host for the lines it reaches.
    world.drop_socket(1006);
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    show(&mut world, "s1");
    let first = world.driver.remote.with_screen("s1", |s| s.unwrap().0.first_line);
    world.driver.remote.fill_screen("s1", first - 2, 4, &mut view);
    world.driver.flush();
    world.pump();
    assert!(world.sent().iter().any(|m| matches!(m, Message::HistoryGet(h) if h.before.0 == first)));
    world.driver.remote.fill_screen("s1", first - 2, 4, &mut view);
    assert_eq!(view.text(), "line 18\nline 19\nline 20\nline 21");
}

#[test]
fn history_is_freed_when_the_terminal_is_no_longer_shown() {
    let mut world = live();
    show(&mut world, "s1");
    for start in [1, 11, 21] {
        let lines: String = (start..start + 10).map(|n| format!("line {n}\r")).collect();
        let output = world.host.output("s1", &lines);
        world.deliver(output);
    }
    let history = |world: &World| world.driver.remote.with_screens(|s| s.terms["s1"].history.len());
    assert!(history(&world) > 0);
    show(&mut world, "chief");
    assert_eq!(history(&world), 0, "unsubscribed, its history goes");
    assert!(screen_text(&world, "s1").contains("line 30"), "its last screen stays");
}

#[test]
fn a_view_only_device_follows_but_never_takes_the_lease() {
    let mut world = World::paired(fake_host::host());
    world.host.allowed[0].1 = Access::View;
    world.boot(None);
    show(&mut world, "s1");
    assert_eq!(screen_text(&world, "s1"), "$");
    assert_eq!(type_text(&mut world, "s1", "ls"), Err(Refused::NoLease));
    acquire(&mut world, "s1", true);
    assert!(!world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { .. } | Message::TerminalInput { .. })));
}

#[test]
fn fit_to_this_screen_resizes_until_the_desktop_takes_the_terminal_back() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "x").unwrap();
    world.driver.remote.resize("s1", Some(GridSize::new(40, 10)));
    world.driver.flush();
    world.pump();
    let cols = |world: &World| world.driver.remote.with_screen("s1", |s| s.unwrap().0.cols);
    assert_eq!(cols(&world), 40);
    assert!(world.driver.remote.lease("s1").resized);
    let back = world.host.desktop_types("s1", "y");
    world.deliver(back);
    assert_eq!(cols(&world), 80, "the desktop's size again");
    assert!(!world.driver.remote.lease("s1").resized);
}

#[test]
fn a_pane_gives_a_terminal_its_size_when_it_changes_and_again_when_the_host_may_not_have_it() {
    let mut host = fake_host::host();
    host.headless = true;
    let mut world = World::paired(host);
    world.boot(None);
    let fit = |world: &mut World, cols| {
        world.driver.remote.fit("s1", GridSize::new(cols, 30));
        world.driver.flush();
        world.pump();
    };
    let fits = |world: &World| world.sent().iter().filter(|m| matches!(m, Message::TerminalFit { .. })).count();
    fit(&mut world, 100);
    assert_eq!(fits(&world), 0, "a screen not followed has no size to give");
    show(&mut world, "s1");
    for _ in 0..3 {
        fit(&mut world, 100);
    }
    assert_eq!(fits(&world), 1, "once, not at every frame");
    assert_eq!(world.host.terminals["s1"].screen.cols, 100);
    fit(&mut world, 120);
    assert_eq!((fits(&world), world.host.terminals["s1"].screen.cols), (2, 120));

    // A new terminal under the session starts with a size of its own: its lease never changed hands.
    let fresh = atomics_core::lease::LeaseState {
        session: "s1".into(),
        holder: None,
        yours: false,
        lease: None,
        next: None,
        ttl_ms: None,
        resized: false,
        reason: None,
    };
    tell(&world, &fresh);
    fit(&mut world, 120);
    assert_eq!(fits(&world), 3);

    // Across a reconnect, the host may be a new one.
    world.drop_socket(1006);
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    show(&mut world, "s1");
    fit(&mut world, 120);
    assert_eq!(fits(&world), 4);

    // A host with a desktop is never sent one.
    let mut world = live();
    show(&mut world, "s1");
    assert!(!world.driver.remote.fits());
    fit(&mut world, 100);
    assert_eq!((fits(&world), world.host.terminals["s1"].screen.cols), (0, 80));
}

#[test]
fn going_on_to_another_terminal_lets_go_of_its_lease_and_of_a_grant_on_its_way() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    assert!(world.driver.remote.lease("s1").yours);
    world.driver.remote.release("s1");
    world.driver.flush();
    world.pump();
    assert!(world.sent().iter().any(|m| matches!(m, Message::LeaseRelease { .. })), "the host hears it");
    let terminal = &world.host.terminals["s1"];
    assert!(terminal.lease.is_none() && terminal.followed, "it stays on screen, free");
    let lease = world.driver.remote.lease("s1");
    assert!(!lease.yours && lease.holder.is_none() && lease.lost.is_none(), "{lease:?}");
    assert_eq!(type_text(&mut world, "s1", "x"), Err(Refused::NoLease), "typing there is a new tap");
    acquire(&mut world, "s1", false);
    assert!(world.driver.remote.lease("s1").yours, "and that tap takes it again");

    // Left before the grant came: what waits for it is dropped, and the grant is let go when it comes.
    let mut world = live();
    show(&mut world, "s1");
    world.driver.remote.acquire("s1", false);
    world.driver.remote.type_text("s1", "ls").unwrap();
    world.driver.remote.release("s1");
    world.driver.flush();
    world.pump();
    assert!(world.host.terminals["s1"].lease.is_none(), "the grant went straight back");
    assert!(world.host.terminals["s1"].typed.is_empty(), "nothing typed into the terminal the user left");
    assert!(!world.driver.remote.lease("s1").yours && !world.driver.remote.lease("s1").acquiring);

    // Coming back before it came: the same request stands, and the grant is kept.
    let mut world = live();
    show(&mut world, "s1");
    world.driver.remote.acquire("s1", false);
    world.driver.remote.release("s1");
    world.driver.remote.acquire("s1", false);
    world.driver.flush();
    world.pump();
    assert!(world.driver.remote.lease("s1").yours && world.host.terminals["s1"].lease.is_some());
    assert!(!world.sent().iter().any(|m| matches!(m, Message::LeaseRelease { .. })));
}

#[test]
fn a_terminal_no_frame_shows_is_let_go() {
    let mut world = live();
    show(&mut world, "s1");
    acquire(&mut world, "s1", false);
    type_text(&mut world, "s1", "x").unwrap();
    show(&mut world, "chief");
    let sent = world.sent();
    assert!(sent.iter().any(|m| matches!(m, Message::LeaseRelease { session, .. } if session == "s1")));
    assert!(sent.iter().any(|m| matches!(m, Message::Unsubscribe { topics }
        if topics.iter().any(|t| matches!(t, Topic::Screen { session, .. } if session == "s1")))));
    assert!(!world.host.terminals["s1"].followed && world.host.terminals["chief"].followed);
    assert!(world.host.terminals["s1"].lease.is_none());
}

#[test]
fn text_waiting_for_the_grant_is_bounded() {
    let mut world = live();
    show(&mut world, "s1");
    world.driver.remote.acquire("s1", false);
    let most = "w".repeat(MAX_WAITING_BYTES);
    assert_eq!(world.driver.remote.type_text("s1", &most), Ok(()));
    assert_eq!(world.driver.remote.type_text("s1", "!"), Err(Refused::TooMuch(MAX_WAITING_BYTES)));
    world.driver.flush();
    world.pump();
    assert_eq!(world.host.terminals["s1"].typed, most);
}

/// A session the host has on its board before its terminal starts: its screen is refused, not asked for
/// again at every frame, and asked for again once the session changes, when the terminal is there.
#[test]
fn a_screen_whose_terminal_has_not_started_is_asked_for_again_when_its_session_changes() {
    use atomics_core::projection::SessionInfo;
    use atomics_core::session::AgentStatus;
    let mut world = live();
    let mut starting = SessionInfo {
        id: "new".into(),
        agent: "shell".into(),
        title: "new".into(),
        cwd: "/w/repo".into(),
        worktree: None,
        branch: None,
        status: AgentStatus::Starting,
        chief: false,
        task: None,
        pending_approvals: 0,
    };
    let subscribes = |world: &World| {
        world
            .sent()
            .iter()
            .filter(|m| matches!(m, Message::Subscribe { topics } if topics.iter().any(|t| matches!(t, Topic::Screen { session, .. } if session == "new"))))
            .count()
    };
    let session = starting.clone();
    let frames = world.host.event(|cursor| Message::SessionUpsert { cursor, session });
    world.deliver(frames);
    for _ in 0..3 {
        show(&mut world, "new");
    }
    assert_eq!(subscribes(&world), 1, "refused once, not again at every frame");
    assert!(world.driver.remote.with_screen("new", |s| s.is_none()));

    world.host.terminals.insert("new".into(), fake_host::FakeTerminal::new(40, 5, "$ "));
    starting.status = AgentStatus::Idle;
    let frames = world.host.event(|cursor| Message::SessionUpsert { cursor, session: starting });
    world.deliver(frames);
    show(&mut world, "new");
    assert_eq!(subscribes(&world), 2, "asked again once the session changed");
    assert!(screen_text(&world, "new").contains('$'), "{}", screen_text(&world, "new"));
}
