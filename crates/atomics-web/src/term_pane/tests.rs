//! The terminal pane in the phone layout over a live fake host: what the overlay forwards is typed
//! encoded for the screen's modes, under the lease; the grid is never resized by looking at it.

use atomics_core::wire::Message;
use atomics_ui::Client;
use egui::{Modifiers, Rect, pos2, vec2};

use super::*;
use crate::driver::Status;
use crate::fake_host;
use crate::gesture::{Kind, Phase, Pointer};
use crate::screens::MAX_TYPED_BYTES;
use crate::views::{self, Tab, View};
use crate::world::World;

/// One frame of the views at a phone's size.
fn frame(ctx: &egui::Context, world: &mut World, client: &mut Client, view: &mut View, (w, h): (f32, f32)) {
    view.now_ms += 16.0;
    let input =
        egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h))), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| views::draw(ui, &mut world.driver, client, view));
    output.textures_delta.clear();
    world.pump();
}

const PHONE: (f32, f32) = (390.0, 844.0);

fn terminal(prepare: impl FnOnce(&mut fake_host::FakeHost)) -> (egui::Context, World, Client, View) {
    let mut host = fake_host::host();
    prepare(&mut host);
    let mut world = World::paired(host);
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    view.tab = Tab::Terminal;
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    (ctx, world, client, view)
}

fn send(ctx: &egui::Context, world: &mut World, client: &mut Client, view: &mut View, inputs: Vec<TermInput>) {
    view.term.inputs.extend(inputs);
    frame(ctx, world, client, view, PHONE);
}

fn typed(world: &World) -> String {
    world.host.terminals["s1"].typed.clone()
}

fn key(name: &str) -> TermInput {
    TermInput::Key { name: name.into(), mods: Modifiers::NONE }
}

fn touch(id: i32, phase: Phase, at: Pos2, time_ms: f64) -> TermInput {
    TermInput::Pointer(Pointer { id, phase, pos: at, kind: Kind::Touch, time_ms })
}

#[test]
fn what_the_overlay_forwards_is_typed_with_the_screens_modes() {
    let (ctx, mut world, mut client, mut view) = terminal(|host| {
        host.terminals.get_mut("s1").unwrap().screen.modes.app_cursor = true;
    });
    assert!(view.term.overlay.rect.is_some() && view.term.overlay.writable, "{:?}", view.term.overlay);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true)]);
    assert!(
        world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { takeover: false, .. })),
        "focus asks to type"
    );
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Text("ls".into()), key("Enter")]);
    assert_eq!(typed(&world), "ls\r");
    // The key bar: application-cursor arrows, sticky Ctrl for the next character only, Tab.
    send(&ctx, &mut world, &mut client, &mut view, vec![key("ArrowUp"), key("Ctrl")]);
    assert!(view.term.overlay.ctrl, "Ctrl lights until the next key");
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![TermInput::Text("c".into()), TermInput::Text("d".into()), key("Tab")],
    );
    assert!(!view.term.overlay.ctrl);
    assert_eq!(typed(&world), "ls\r\x1bOA\x03d\t");
    // A hardware keyboard's Ctrl+L and Shift+Return, and a bracketed paste.
    let modes = world.host.modes("s1", |m| m.bracketed_paste = true);
    world.deliver(modes);
    let ctrl = TermInput::Key { name: "l".into(), mods: Modifiers::CTRL };
    let shift_enter = TermInput::Key { name: "Enter".into(), mods: Modifiers::SHIFT };
    send(&ctx, &mut world, &mut client, &mut view, vec![ctrl, shift_enter, TermInput::Paste("a\nb".into())]);
    assert!(typed(&world).ends_with("\x0c\x1b\r\x1b[200~a\nb\x1b[201~"), "{:?}", typed(&world));
    // A composition shows as preedit and is typed once, when committed.
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Preedit("にほ".into())]);
    assert!(!typed(&world).contains("にほ"));
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![TermInput::Preedit(String::new()), TermInput::Text("日本".into())],
    );
    assert!(typed(&world).ends_with("日本"));
}

#[test]
fn the_phone_shows_the_desktops_grid_and_resizes_only_when_asked() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    let rect = view.term.overlay.rect.expect("the overlay's place");
    assert_eq!((rect.left(), rect.width()), (0.0, PHONE.0), "the terminal fills the width");
    let resized = |world: &World| world.sent().iter().any(|m| matches!(m, Message::TerminalResize { .. }));
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("x".into())]);
    // Pinching zooms; nothing is resized by it.
    let at = rect.center();
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(1, Phase::Down, at - vec2(40.0, 0.0), 0.0),
            touch(2, Phase::Down, at + vec2(40.0, 0.0), 5.0),
            touch(2, Phase::Move, at + vec2(120.0, 0.0), 20.0),
            touch(1, Phase::Up, at, 30.0),
            touch(2, Phase::Up, at, 31.0),
        ],
    );
    let font = view.term.font.expect("the pinched size");
    assert!(font > 10.0, "zoomed in to {font}");
    assert!(!resized(&world), "looking at the grid never resizes it");
    assert_eq!(world.host.terminals["s1"].screen.cols, 80);

    // Fit to this screen is the explicit resize, and lasts until the desktop types.
    let frame_rect = view.term.overlay.rect.unwrap();
    let fit = fit_size(&Frame {
        rect: frame_rect,
        pad: vec2(PAD, PAD),
        origin: frame_rect.min,
        size: font,
        cell: vec2(1.0, 1.0),
        first: 0,
        top: 0,
        rows: 1,
        cols: 1,
        live_top: 0,
        oldest: 0,
    });
    assert!(fit.cols > 80 && fit.cell_width == Some(1), "the grid that fits: {fit:?}");
    world.driver.remote.resize("s1", Some(GridSize::new(30, 20)));
    world.driver.flush();
    world.pump();
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(world.host.terminals["s1"].screen.cols, 30);
    let back = world.host.desktop_types("s1", "y");
    world.deliver(back);
    assert_eq!(world.host.terminals["s1"].screen.cols, 80, "the desktop's size when it takes the lease back");
}

#[test]
fn losing_the_lease_blurs_the_overlay_and_another_typist_makes_it_read_only() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("a".into())]);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.term.typing());
    let back = world.host.desktop_types("s1", "b");
    world.deliver(back);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.term.overlay.blur && !view.term.typing(), "typing again is a new tap");
    assert!(view.term.overlay.writable, "the lease is free again");

    let taken = world.host.other_takes("s1", "Firefox on Linux");
    world.deliver(taken);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(!view.term.overlay.writable, "read only while another client types");
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Text("x".into())]);
    assert_eq!(typed(&world), "ab");
}

fn acquires(world: &World) -> usize {
    world.sent().iter().filter(|m| matches!(m, Message::LeaseAcquire { .. })).count()
}

#[test]
fn after_a_take_back_nothing_in_flight_types_or_asks_for_the_lease_again() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Text("early".into())]);
    assert_eq!((typed(&world).as_str(), acquires(&world)), ("", 0), "text without the focus types nothing");

    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("a".into())]);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!((typed(&world).as_str(), acquires(&world)), ("a", 1), "the tap asks, once");

    // The desktop's Take back ends the lease (typing nothing) while keys are on their way.
    let back = world.host.desktop_types("s1", "");
    world.deliver(back);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Text("x".into()), key("Enter")]);
    assert!(view.term.overlay.blur && !view.term.typing(), "the overlay loses the focus");
    // Losing it, the keyboard commits the word it was composing, then the field reports the blur.
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            TermInput::Preedit("wor".into()),
            TermInput::Preedit(String::new()),
            TermInput::Text("word".into()),
            TermInput::Focus(false),
        ],
    );
    // The key bar's Paste, whose clipboard read finishes late.
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Paste("make\n".into())]);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(typed(&world), "a", "nothing typed after the take back");
    assert_eq!(acquires(&world), 1, "nor was the free lease asked for");
    assert!(world.host.terminals["s1"].lease.is_none());
    let lease = world.driver.remote.lease("s1");
    assert_eq!((lease.yours, lease.lost), (false, Some(Lost::Desktop)));
    assert_eq!(world.driver.remote.notice(), None, "and it says nothing of another typist");

    // A new tap asks again.
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("b".into())]);
    assert_eq!((typed(&world).as_str(), acquires(&world)), ("ab", 2));
}

#[test]
fn a_paste_over_the_limit_is_not_typed_and_says_so() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("a".into())]);
    let paste = "p".repeat(MAX_TYPED_BYTES + 1);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Paste(paste)]);
    assert_eq!(typed(&world), "a");
    let notice = world.driver.remote.notice().expect("a note");
    assert!(notice.starts_with("Too much to type at once") && notice.contains("up to 1 MiB"), "{notice}");
    assert!(view.term.typing(), "still typing: only the paste was refused");
}

#[test]
fn a_paste_before_the_grant_names_the_smaller_limit() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    // The tap asks for the lease; the paste in the same frame would wait for the grant.
    let paste = "p".repeat(MAX_WAITING_BYTES + 1);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Paste(paste)]);
    assert_eq!(typed(&world), "");
    let notice = world.driver.remote.notice().expect("a note");
    assert!(notice.contains("Up to 16 KiB waits") && !notice.contains("MiB"), "{notice}");
    // Once granted, the same paste is typed.
    let paste = "q".repeat(MAX_WAITING_BYTES + 1);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Paste(paste.clone())]);
    assert_eq!(typed(&world), paste);
}

#[test]
fn a_touch_whose_lift_was_dropped_leaves_the_next_touch_a_touch() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    let at = view.term.overlay.rect.unwrap().center();
    let now = view.now_ms;
    send(&ctx, &mut world, &mut client, &mut view, vec![touch(1, Phase::Down, at, now)]);
    // The terminal leaves the screen before the lift is carried out: it goes with the overlay's other input.
    view.tab = Tab::Board;
    let now = view.now_ms;
    send(&ctx, &mut world, &mut client, &mut view, vec![touch(1, Phase::Up, at, now)]);
    view.tab = Tab::Terminal;
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    let now = view.now_ms;
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(2, Phase::Down, at + vec2(0.0, 10.0), now),
            touch(2, Phase::Move, at + vec2(0.0, 130.0), now + 20.0),
            touch(2, Phase::Up, at + vec2(0.0, 130.0), now + 40.0),
        ],
    );
    assert_eq!(view.term.font, None, "a drag, not a pinch with a finger long gone");
}

#[test]
fn offline_the_terminal_shows_disconnected_and_drops_what_is_typed() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("one".into())]);
    world.drop_socket(1006);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Text("lost".into())]);
    assert!(!view.term.overlay.writable && view.term.overlay.rect.is_some(), "on view, read only");
    world.advance(world.driver.wake_in().expect("a retry"));
    world.ticket("ticket-2");
    for _ in 0..2 {
        frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    }
    assert!(view.term.overlay.writable);
    assert_eq!(typed(&world), "one", "nothing typed offline is sent later");
}

#[test]
fn a_drag_scrolls_this_views_history_and_a_tap_clicks_where_the_program_asked() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    for start in [1, 11, 21] {
        let lines: String = (start..start + 10).map(|n| format!("line {n}\r")).collect();
        let output = world.host.output("s1", &lines);
        world.deliver(output);
    }
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    let at = view.term.overlay.rect.unwrap().center();
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(1, Phase::Down, at, 0.0),
            touch(1, Phase::Move, at + vec2(0.0, 120.0), 40.0),
            touch(1, Phase::Up, at + vec2(0.0, 120.0), 60.0),
        ],
    );
    assert!(view.term.top.is_some(), "scrolled back");
    assert!(view.term.view.text().contains("line 15"), "{}", view.term.view.text());
    assert!(!world.sent().iter().any(|m| matches!(m, Message::TerminalInput { .. })), "scrolling types nothing");
    let output = world.host.output("s1", "more\r");
    world.deliver(output);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.term.view.text().contains("line 15"), "new output does not move a view scrolled back");

    // Typing returns to the live screen; where the program takes mouse reports, a tap clicks the cell.
    let modes = world.host.modes("s1", |m| {
        m.mouse_click = true;
        m.sgr_mouse = true;
    });
    world.deliver(modes);
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Focus(true), TermInput::Text("q".into())]);
    assert_eq!(view.term.top, None);
    let rect = view.term.overlay.rect.unwrap();
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(2, Phase::Down, rect.min + vec2(PAD + 1.0, PAD + 1.0), 100.0),
            touch(2, Phase::Up, rect.min + vec2(PAD + 1.0, PAD + 1.0), 150.0),
        ],
    );
    let typed = typed(&world);
    let top_row = view.term.view.first_line - world.host.terminals["s1"].screen.first_line;
    let click = format!("\x1b[<0;1;{row}M\x1b[<0;1;{row}m", row = top_row + 1);
    assert!(typed.ends_with(&click), "a click on the first cell shown: {typed:?}, {click:?}");
}

#[test]
fn a_long_press_selects_and_offers_copy() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    let output = world.host.output("s1", "hello world\r");
    world.deliver(output);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    let rect = view.term.overlay.rect.unwrap();
    let start = rect.min + vec2(PAD + 1.0, PAD + 1.0);
    let now = view.now_ms;
    send(&ctx, &mut world, &mut client, &mut view, vec![touch(1, Phase::Down, start, now)]);
    view.now_ms += 600.0;
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    let now = view.now_ms;
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(1, Phase::Move, start + vec2(300.0, 0.0), now),
            touch(1, Phase::Up, start + vec2(300.0, 0.0), now + 10.0),
        ],
    );
    let (text, _) = view.term.overlay.copy.clone().expect("Copy offered");
    assert_eq!(text, "$ hello world");
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Copied]);
    assert_eq!(view.term.overlay.copy, None);
    assert!(world.host.terminals["s1"].typed.is_empty(), "selecting types nothing");
}

#[test]
fn a_long_press_selects_the_word_and_its_handles_move_its_ends() {
    let (ctx, mut world, mut client, mut view) = terminal(|_| {});
    let output = world.host.output("s1", "echo hello world\r");
    world.deliver(output);
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    // "$ echo hello world": the finger rests on "hello", columns 7 to 11.
    let (on_hello, cell) = view.term.cell_centre(0, 9);
    let now = view.now_ms;
    send(&ctx, &mut world, &mut client, &mut view, vec![touch(1, Phase::Down, on_hello, now)]);
    view.now_ms += 600.0;
    frame(&ctx, &mut world, &mut client, &mut view, PHONE);
    let now = view.now_ms;
    send(&ctx, &mut world, &mut client, &mut view, vec![touch(1, Phase::Up, on_hello, now)]);
    let copy = |view: &View| view.term.overlay.copy.clone().map(|(text, _)| text);
    assert_eq!(copy(&view).as_deref(), Some("hello"), "the word under the finger, nothing more");

    // The end handle's knob hangs under the word's last cell; dragging it over "world" takes that word in.
    let knob = |col: u16| view.term.cell_centre(0, col).0 + vec2(cell.x / 2.0, cell.y / 2.0 + KNOB);
    let (end, to) = (knob(11), knob(17));
    let now = view.now_ms;
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(2, Phase::Down, end, now),
            touch(2, Phase::Move, end + (to - end) / 2.0, now + 30.0),
            touch(2, Phase::Move, to, now + 60.0),
            touch(2, Phase::Up, to, now + 90.0),
        ],
    );
    assert_eq!(copy(&view).as_deref(), Some("hello world"));
    // The start handle, dragged left past "echo".
    let start = view.term.cell_centre(0, 7).0 + vec2(-cell.x / 2.0, cell.y / 2.0 + KNOB);
    let now = view.now_ms;
    send(
        &ctx,
        &mut world,
        &mut client,
        &mut view,
        vec![
            touch(3, Phase::Down, start, now),
            touch(3, Phase::Move, start - vec2(5.0 * cell.x, 0.0), now + 30.0),
            touch(3, Phase::Up, start - vec2(5.0 * cell.x, 0.0), now + 60.0),
        ],
    );
    assert_eq!(copy(&view).as_deref(), Some("echo hello world"));
    assert_eq!(view.term.font, None, "dragging a handle neither pinches nor pans");
    assert_eq!(view.term.top, None, "nor scrolls");
    // Copy takes the selection away; a tap elsewhere would too. Nothing was typed.
    send(&ctx, &mut world, &mut client, &mut view, vec![TermInput::Copied]);
    assert_eq!(copy(&view), None);
    assert!(world.host.terminals["s1"].typed.is_empty());
}

#[test]
fn the_phone_layout_at_phone_widths() {
    for size in [(360.0, 780.0), (390.0, 844.0), (430.0, 932.0)] {
        let (ctx, mut world, mut client, mut view) = terminal(|_| {});
        frame(&ctx, &mut world, &mut client, &mut view, size);
        assert!(view.compact, "{size:?}");
        let rect = view.term.overlay.rect.expect("a terminal");
        assert_eq!(rect.width(), size.0, "{size:?}: the terminal fills the width");
        assert!(rect.bottom() <= size.1 - crate::layout::BOTTOM_BAR, "{size:?}: above the pane switcher");
        // Typing: the key bar's space replaces the switcher, and the terminal grows into it. The page says
        // how tall the key bar is in CSS pixels.
        view.viewport.keybar = 88.0;
        view.term.inputs.push(TermInput::Focus(true));
        frame(&ctx, &mut world, &mut client, &mut view, size);
        frame(&ctx, &mut world, &mut client, &mut view, size);
        let typing = view.term.overlay.rect.unwrap();
        // egui rounds the layout to 1/32 of a point.
        assert!((typing.bottom() - (size.1 - 88.0 / crate::layout::ZOOM)).abs() < 0.05, "{size:?}: {typing:?}");
        // Every pane draws at this width.
        for tab in [Tab::Agents, Tab::Board, Tab::Diffs, Tab::Approvals, Tab::Chief] {
            view.tab = tab;
            frame(&ctx, &mut world, &mut client, &mut view, size);
            frame(&ctx, &mut world, &mut client, &mut view, size);
            assert_eq!(view.term.overlay.rect.is_some(), tab == Tab::Chief, "{tab:?}");
        }
        assert!(world.host.terminals["chief"].followed && !world.host.terminals["s1"].followed);
    }
}
