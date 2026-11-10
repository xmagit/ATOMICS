//! The keys that move around the window, and what they do, on every client. The desktop and a host's clients (a
//! browser, `--connect`) reach their terminals differently, so each says for itself which events never reach a
//! terminal ([`before_terminal`]); from there on the decisions are these:
//!
//! - **Ctrl+1 to 9, Command on a Mac** ([`pick_agent`]): the Nth agent of the window's tab, the number its sidebar row
//!   shows, with its diff against main; the keyboard stays with the diff. Past the last agent nothing happens, and no
//!   digit reaches a terminal.
//! - **Shift+↓/↑** ([`walk`]): the next and previous changed file, from anywhere but a text field or the editor.
//!   **PageDown/PageUp**: the next and previous hunk across files, while the diff has the keyboard; a terminal keeps
//!   its own. The plain arrows scroll lines ([`crate::panel_nav`]).
//! - **Typing or Escape in the panel** ([`hand_back`]) gives the keyboard back to the active terminal.
//!
//! Two exceptions are deliberate: with Tickets open the reader follows the agent and no diff comes up, and in editor
//! mode the agent's editor takes the keyboard. A phone takes the same keys where it has the agents.
//!
//! A key records commands like any draw. Where one changes the keyboard's place, the platform carries the commands
//! out first and then calls [`Keyboard::settle`], since activating a session may ask for its terminal.

use std::path::Path;

use egui::os::OperatingSystem;
use egui::{Context, Event, Key, KeyboardShortcut, Modifiers};

use crate::keys::consume_physical_shortcut;
use crate::panel_nav::focus_id;
use crate::{Backend, Client, Command, PanelMode, ViewHints};

/// Whether a configured GitHub chord belongs to this window before terminal routing.
pub fn github_key(event: &Event, binding: &str, os: OperatingSystem) -> bool {
    let Some(chord) = crate::keys::shortcut_for_os(binding, os) else { return false };
    matches!(event, Event::Key { key, physical_key, pressed: true, modifiers, .. }
        if (if crate::keys::is_digit(chord.logical_key) { physical_key.unwrap_or(*key) } else { *key }) == chord.logical_key
            && modifiers.matches_exact(chord.modifiers))
}

/// Open the client list using the host's configured binding, before list or diff navigation.
pub fn pull_requests(ctx: &Context, backend: &dyn Backend, client: &mut Client) {
    let binding = backend.ui_config().keys.github.clone();
    let os = ctx.os();
    let mut toggle = false;
    ctx.input_mut(|i| {
        i.events.retain(|event| {
            if github_key(event, &binding, os) {
                toggle = true;
                false
            } else {
                true
            }
        })
    });
    if toggle {
        client.pull_requests.open = !client.pull_requests.open;
    }
    crate::pull_request::keys(ctx, backend, client);
}

const DIGITS: [Key; 9] =
    [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// Whether the agent chords use Command: on a Mac, and an iPad or iPhone with a keyboard. A browser's `os` comes from
/// its user agent, so this holds in a page as it does in a native window.
pub fn command_chords(os: OperatingSystem) -> bool {
    matches!(os, OperatingSystem::Mac | OperatingSystem::IOS)
}

fn agent_modifiers(os: OperatingSystem) -> Modifiers {
    if command_chords(os) { Modifiers::MAC_CMD } else { Modifiers::CTRL }
}

/// The agent chords as this client's keyboard says them, for the shortcut list.
pub fn agents_label(ctx: &Context) -> String {
    format!("{}…9", ctx.format_shortcut(&KeyboardShortcut::new(agent_modifiers(ctx.os()), Key::Num1)))
}

/// Which agent `event` picks, from 0. The physical key, because some layouts type digits only with Shift.
fn agent_key(event: &Event, os: OperatingSystem) -> Option<usize> {
    let Event::Key { key, physical_key, pressed: true, modifiers, .. } = event else { return None };
    let digit = physical_key.unwrap_or(*key);
    DIGITS.iter().position(|d| *d == digit).filter(|_| modifiers.matches_exact(agent_modifiers(os)))
}

fn file_key(event: &Event) -> bool {
    let Event::Key { key, physical_key, pressed: true, modifiers, .. } = event else { return false };
    matches!(physical_key.unwrap_or(*key), Key::ArrowDown | Key::ArrowUp) && modifiers.matches_exact(Modifiers::SHIFT)
}

/// Whether `event` is the window's even while a terminal has the keyboard. An editor (`editor`) keeps Shift with the
/// arrows, which select text there. PageUp and PageDown are not among these: a terminal keeps them.
pub fn before_terminal(event: &Event, os: OperatingSystem, editor: bool) -> bool {
    agent_key(event, os).is_some() || (!editor && file_key(event))
}

/// Takes the agent chords pressed this frame: which agent each picks, from 0.
pub fn take_agents(ctx: &Context) -> Vec<usize> {
    let os = ctx.os();
    let mut picked = Vec::new();
    ctx.input_mut(|i| {
        i.events.retain(|event| match agent_key(event, os) {
            Some(n) => {
                picked.push(n);
                false
            }
            None => true,
        });
    });
    picked
}

/// One step through a diff: `delta` is 1 or -1, by hunk or by file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Walk {
    pub delta: i32,
    pub hunk: bool,
}

/// Takes the step this frame's keys ask for, unless a text field or the editor has the keyboard, or the chief's trace
/// lane, which steps with Shift+↓/↑ itself.
pub fn take_walk(ctx: &Context, client: &Client, hints: &ViewHints) -> Option<Walk> {
    if client.pull_requests.open {
        return None;
    }
    let elsewhere = ctx
        .memory(|m| client.editor_id.is_some_and(|id| m.has_focus(id)) || m.has_focus(crate::trace_lane::focus_id()));
    if ctx.text_edit_focused() || elsewhere {
        return None;
    }
    let in_diff = hints.diff_open && hints.panel == PanelMode::Diff && ctx.memory(|m| m.has_focus(focus_id()));
    ctx.input_mut(|i| {
        let mut pressed = |modifiers, key| consume_physical_shortcut(i, &KeyboardShortcut::new(modifiers, key));
        if pressed(Modifiers::SHIFT, Key::ArrowDown) {
            Some(Walk { delta: 1, hunk: false })
        } else if pressed(Modifiers::SHIFT, Key::ArrowUp) {
            Some(Walk { delta: -1, hunk: false })
        } else if in_diff && pressed(Modifiers::NONE, Key::PageDown) {
            Some(Walk { delta: 1, hunk: true })
        } else if in_diff && pressed(Modifiers::NONE, Key::PageUp) {
            Some(Walk { delta: -1, hunk: true })
        } else {
            None
        }
    })
}

/// Where a key leaves the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyboard {
    /// With the panel beside the terminals: the diff, or Tickets.
    Panel,
    /// With the agent's editor, which starts on demand.
    Editor,
}

impl Keyboard {
    /// Moves the keyboard, once the platform carried out what the key recorded.
    pub fn settle(self, ctx: &Context, client: &mut Client) {
        client.focus_terminal = None;
        client.focus_editor = false;
        if crate::management::window_open(client) || client.tickets.dialog {
            return;
        }
        client.focus_editor = self == Keyboard::Editor;
        ctx.memory_mut(|m| match self {
            Keyboard::Panel => m.request_focus(focus_id()),
            // A worktree's editor is started lazily: selecting it must not leave the previous agent's terminal
            // holding the keys.
            Keyboard::Editor => m.focused().into_iter().for_each(|id| m.surrender_focus(id)),
        });
    }
}

/// The agent an agent chord picked, and what became of the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub session: String,
    pub keyboard: Keyboard,
    /// Tickets is open: it keeps its place, and no diff comes up.
    pub tickets_open: bool,
}

/// The `n`th agent of the window's tab, from 0, as [the module](self) says; `None` past the last one.
pub fn pick_agent(ctx: &Context, backend: &dyn Backend, client: &mut Client, n: usize) -> Option<Picked> {
    let sessions = backend.session_list();
    let session = backend.visible_sessions().get(n).map(|&index| sessions[index].clone())?;
    let hints = backend.view_hints();
    // A menu left open would cover the diff the chord brings up.
    egui::Popup::close_all(ctx);
    // Only an open Tickets panel keeps its place; activating opens the panel, so ask first.
    let tickets_open = hints.panel == PanelMode::Tickets && hints.diff_open;
    client.pr_view = None;
    client.pr_target = None;
    client.send(Command::Activate { session: session.id.clone(), open_diff: true });
    let keyboard = if hints.panel == PanelMode::Editor {
        if !backend.editors().iter().any(|e| Path::new(&e.root) == session.root && e.running) {
            let state = client.diff_views.get(&session.root);
            let path = client.editor_file.get(&session.root).cloned().or_else(|| {
                let state = state?;
                state.focused.clone().or_else(|| state.prepared.as_ref()?.diff.files.first().map(|f| f.path().into()))
            });
            client.send(Command::Edit { root: session.root.clone(), path, line: None });
        }
        Keyboard::Editor
    } else {
        if !tickets_open {
            client.send(Command::SetPanel(PanelMode::Diff));
        } else if client.tickets.active() {
            reader_follows(ctx, backend, client, &session.id);
        }
        Keyboard::Panel
    };
    Some(Picked { session: session.id, keyboard, tickets_open })
}

/// With a ticket open in the reader: the agent's ticket. Find stays in Find.
fn reader_follows(ctx: &Context, backend: &dyn Backend, client: &mut Client, session: &str) {
    let Some(read) = &client.tickets.read else { return };
    match backend.session_ticket(session) {
        Some(link) if read.link.same_ticket(&link) => {}
        Some(link) => client.tickets.open(link, false, ctx.input(|i| i.time), &mut client.commands),
        None => client.send(Command::Toast("That agent has no ticket".into())),
    }
}

/// One step through what the panel shows: a pull request's diff, Tickets, or else the diff target's diff, which it
/// brings up. `None` with nothing to step through.
pub fn walk(ctx: &Context, backend: &dyn Backend, client: &mut Client, step: Walk) -> Option<Keyboard> {
    let hints = backend.view_hints();
    if hints.panel == PanelMode::Diff
        && let Some((_, state)) = client.pr_view.as_mut()
    {
        if step.hunk {
            state.step_hunk(step.delta);
        } else {
            state.step(step.delta);
        }
        client.send(Command::OpenDiff);
    } else if hints.panel == PanelMode::Tickets && hints.diff_open {
        client.tickets.step(step.delta, ctx.input(|i| i.time), &mut client.commands);
    } else {
        let root = hints.diff_target?;
        client.send(Command::OpenDiff);
        client.send(Command::SetPanel(PanelMode::Diff));
        let state = client.diff_views.entry(root.clone()).or_default();
        let target = if step.hunk { state.step_hunk(step.delta) } else { state.step(step.delta) };
        if target.is_some() {
            // Keep the host in diff mode without reopening the file over the jump.
            client.send(Command::LeaveFile(root));
        }
    }
    Some(Keyboard::Panel)
}

/// While the panel has the keyboard: typing or Escape, or the panel going away, hands it back to the active terminal
/// (`client.focus_terminal`). A typed key stays in the input for the terminal, and true says the keyboard moved. The
/// open windows hold the keyboard, and the reader takes Escape in a ticket itself. Where the diff fills the
/// window (`diff_fills`), it closes, or the terminal would not show.
pub fn hand_back(ctx: &Context, backend: &dyn Backend, client: &mut Client, diff_fills: bool) -> bool {
    let hints = backend.view_hints();
    let reader = hints.panel == PanelMode::Tickets && hints.diff_open && client.tickets.active();
    if crate::management::window_open(client) || !ctx.memory(|m| m.has_focus(focus_id())) || client.tickets.dialog {
        return false;
    }
    let escape =
        !(reader && client.tickets.read.is_some()) && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    let typing =
        ctx.input(|i| i.events.iter().any(|e| matches!(e, Event::Text(text) | Event::Paste(text) if !text.is_empty())));
    if client.focus_editor {
        ctx.memory_mut(|m| m.surrender_focus(focus_id()));
        return false;
    }
    if !(escape || typing || !hints.diff_open || !matches!(hints.panel, PanelMode::Diff | PanelMode::Tickets)) {
        return false;
    }
    ctx.memory_mut(|m| m.surrender_focus(focus_id()));
    client.focus_terminal = hints.active;
    if diff_fills {
        client.send(Command::CloseDiff);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    const LINUX: OperatingSystem = OperatingSystem::Nix;
    const MAC: OperatingSystem = OperatingSystem::Mac;

    #[test]
    fn ctrl_with_a_digit_picks_an_agent_and_command_does_on_a_mac() {
        let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
        assert_eq!(agent_key(&press(Key::Num1, ctrl), LINUX), Some(0));
        assert_eq!(agent_key(&press(Key::Num9, Modifiers::CTRL), LINUX), Some(8));
        let command = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };
        for os in [MAC, OperatingSystem::IOS] {
            assert_eq!(agent_key(&press(Key::Num2, command), os), Some(1), "{os:?}");
            assert_eq!(agent_key(&press(Key::Num2, Modifiers::CTRL), os), None, "{os:?}: Command there");
        }
        assert_eq!(agent_key(&press(Key::Num2, command), LINUX), None);
    }

    #[test]
    fn extra_modifiers_other_keys_and_a_release_are_no_agent_chord() {
        for event in [
            press(Key::Num0, Modifiers::CTRL),
            press(Key::Num1, Modifiers::NONE),
            press(Key::Num1, Modifiers::ALT),
            press(Key::Num1, Modifiers::CTRL | Modifiers::SHIFT),
            press(Key::Num1, Modifiers::CTRL | Modifiers::ALT),
            press(Key::D, Modifiers::CTRL),
            Event::Key {
                key: Key::Num1,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
        ] {
            assert_eq!(agent_key(&event, LINUX), None, "{event:?}");
        }
    }

    #[test]
    fn a_digit_only_shift_types_is_still_its_physical_key() {
        let event = Event::Key {
            key: Key::Exclamationmark,
            physical_key: Some(Key::Num1),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        assert_eq!(agent_key(&event, LINUX), Some(0));
    }

    #[test]
    fn a_terminal_keeps_what_is_not_the_windows() {
        for (event, editor, windows) in [
            (press(Key::Num3, Modifiers::CTRL), false, true),
            (press(Key::Num3, Modifiers::CTRL), true, true),
            (press(Key::ArrowDown, Modifiers::SHIFT), false, true),
            (press(Key::ArrowUp, Modifiers::SHIFT), false, true),
            (press(Key::ArrowDown, Modifiers::SHIFT), true, false),
            (press(Key::ArrowDown, Modifiers::NONE), false, false),
            (press(Key::ArrowLeft, Modifiers::SHIFT), false, false),
            (press(Key::PageDown, Modifiers::NONE), false, false),
            (press(Key::D, Modifiers::CTRL), false, false),
            (press(Key::C, Modifiers::CTRL), false, false),
        ] {
            assert_eq!(before_terminal(&event, LINUX, editor), windows, "{event:?} editor={editor}");
        }
    }
}

#[cfg(test)]
mod github_tests {
    use super::*;
    #[test]
    fn github_binding_uses_logical_letters_exact_modifiers_and_client_os() {
        let event = |key, physical_key, modifiers| Event::Key {
            key,
            physical_key: Some(physical_key),
            pressed: true,
            repeat: false,
            modifiers,
        };
        assert!(github_key(&event(Key::G, Key::U, Modifiers::CTRL), "primary+g", OperatingSystem::Nix));
        assert!(!github_key(&event(Key::U, Key::G, Modifiers::CTRL), "primary+g", OperatingSystem::Nix));
        assert!(!github_key(
            &event(Key::G, Key::G, Modifiers::CTRL | Modifiers::SHIFT),
            "primary+g",
            OperatingSystem::Nix
        ));
        assert!(github_key(&event(Key::G, Key::G, Modifiers::MAC_CMD), "primary+g", OperatingSystem::Mac));
        assert!(github_key(&event(Key::P, Key::P, Modifiers::ALT), "alt+p", OperatingSystem::Nix));
    }
}
