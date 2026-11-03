//! The page's view of its host follows the desktop's rules.

use std::path::Path;

use atomics_core::config::ChiefDock;
use atomics_ui::{Backend, Command, PanelMode};

use super::*;
use crate::fake_host::{self, WORKTREE};
use crate::world::World;

fn live() -> World {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world
}

/// Settle `desk` over `world`'s backend, the tab's sessions as the backend picks them.
fn settle(desk: &mut Desk, world: &World) {
    let remote = &world.driver.remote;
    desk.settle(remote, |hints| {
        remote.set_view(hints.clone());
        remote.visible_sessions()
    });
}

#[test]
fn a_page_opens_on_the_first_plans_tab_with_its_first_agent_active() {
    let world = live();
    let mut desk = Desk::default();
    settle(&mut desk, &world);
    assert_eq!(desk.active.as_deref(), Some("s1"), "the plan's agent, never the chief");
    let hints = desk.hints(&world.driver.remote);
    assert!(hints.plan_view && hints.diff_open && hints.panel == PanelMode::Plan, "{hints:?}");
    assert_eq!(hints.diff_target.as_deref(), Some(Path::new(WORKTREE)), "the diff follows the active session");
    assert_eq!(desk.chief, ChiefWindow { open: true, dock: ChiefDock::Docked, width: None });
    assert_eq!(desk.typing_target().as_deref(), Some("s1"));
}

#[test]
fn view_commands_move_the_page_as_they_move_the_desktop() {
    let world = live();
    let remote = &world.driver.remote;
    let mut desk = Desk::default();
    settle(&mut desk, &world);
    desk.apply(Command::TogglePlan, remote);
    assert_eq!((desk.panel, desk.diff_open), (PanelMode::Diff, true), "the Plan button turns to the diff");
    desk.apply(Command::ToggleDiff, remote);
    assert!(!desk.diff_open);
    desk.apply(Command::TogglePlan, remote);
    assert_eq!((desk.panel, desk.diff_open), (PanelMode::Plan, true), "and back to the board, opening the panel");

    // A worktree picked on its own: no session is active, and none is picked for it again.
    desk.apply(Command::ShowRoot("/w/other".into()), remote);
    settle(&mut desk, &world);
    assert_eq!((desk.active.as_deref(), desk.diff_target(remote)), (None, Some("/w/other".into())));
    desk.apply(Command::Activate { session: "s1".into(), open_diff: false }, remote);
    assert_eq!(desk.diff_target(remote).as_deref(), Some(Path::new(WORKTREE)));

    // A tab: the Ungrouped one shows no agent of the fake host's (s1 follows a plan), the plan's shows it.
    desk.apply(Command::ShowUngrouped, remote);
    settle(&mut desk, &world);
    assert_eq!((desk.ungrouped, desk.plan_view, desk.active.as_deref()), (true, false, None));
    desk.apply(Command::ShowPlan(0), remote);
    settle(&mut desk, &world);
    assert_eq!((desk.plan_view, desk.panel, desk.active.as_deref()), (true, PanelMode::Diff, Some("s1")));
    assert_eq!(remote.visible_sessions(), [1], "the plan's agent, by its place in the list");

    // The chief's window is the page's.
    desk.apply(Command::HideChief, remote);
    assert!(!desk.chief.open);
    desk.apply(Command::ToggleChief, remote);
    desk.apply(Command::SetChiefDock(ChiefDock::Floating), remote);
    desk.apply(Command::ResizeChief(Some(700.0)), remote);
    assert_eq!(desk.chief, ChiefWindow { open: true, dock: ChiefDock::Floating, width: Some(700.0) });
}

#[test]
fn input_goes_to_the_terminal_clicked_into_until_an_agent_is_activated() {
    let world = live();
    let remote = &world.driver.remote;
    let mut desk = Desk::default();
    settle(&mut desk, &world);
    desk.typing_in = Some("chief".into());
    assert_eq!(desk.typing_target().as_deref(), Some("chief"));
    desk.apply(Command::Activate { session: "s1".into(), open_diff: false }, remote);
    assert_eq!(desk.typing_target().as_deref(), Some("s1"));
    desk.typing_in = Some("chief".into());
    desk.apply(Command::HideChief, remote);
    assert_eq!(desk.typing_target().as_deref(), Some("s1"), "a hidden chief takes no input");
}

#[test]
fn the_wide_layout_alone_carries_out_the_chiefs_window() {
    assert!(carries_out(&Command::ShowUngrouped, false) && carries_out(&Command::CloseDiff, false));
    for command in [Command::HideChief, Command::ToggleChief, Command::SetChiefDock(ChiefDock::Docked)] {
        assert!(window_command(&command) && carries_out(&command, true) && !carries_out(&command, false));
    }
    for host in [Command::NewSession, Command::PickProject, Command::SetView(None), Command::Rename("s1".into())] {
        assert!(!carries_out(&host, true), "{host:?}");
    }
    let world = live();
    let remote = &world.driver.remote;
    remote.set_chief_window(ChiefWindow { open: true, dock: ChiefDock::Docked, width: None }, true);
    assert!(remote.available(&Command::ToggleChief) && remote.chief().open);
    remote.set_chief_window(ChiefWindow { open: true, dock: ChiefDock::Docked, width: None }, false);
    assert!(!remote.available(&Command::HideChief), "a phone has no window to hide");
}
