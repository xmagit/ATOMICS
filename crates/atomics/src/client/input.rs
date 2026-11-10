//! The native window's stand-in for the browser's terminal overlay (atomics-web's `web/terminal.js`):
//! the terminal pane says where its cells are and whether it wants the keyboard ([`Overlay`]); this takes
//! the terminal's share of each frame's raw input before egui sees it and hands it to the pane as the
//! overlay would ([`TermInput`]). A click on the cells focuses them and asks for the lease, keys and text
//! typed while they have the keyboard go to the terminal, and a click anywhere else, or a text box taking
//! the keyboard, lets go. Command on macOS and Ctrl+Shift elsewhere stay the window's, and so do the keys every client
//! takes before a terminal (`atomics_ui::window_keys`).

use atomics_ui::window_keys::before_terminal;
use atomics_web::gesture::{Kind, Phase, Pointer};
use atomics_web::term_pane::{Overlay, TermInput};
use egui::os::OperatingSystem;
use egui::{Event, Key, Modifiers, MouseWheelUnit, Pos2, RawInput, vec2};

/// How far one wheel line scrolls, in points.
const LINE: f32 = 20.0;

#[derive(Default)]
pub struct Keyboard {
    /// The terminal has the keyboard, as the overlay would have the browser's focus.
    pub focused: bool,
    /// A press began on the cells: moves and the release go there too.
    pressed: bool,
    pointer: Option<Pos2>,
}

/// The name the overlay gives a named key.
fn key_name(key: Key) -> Option<&'static str> {
    Some(match key {
        Key::Escape => "Escape",
        Key::Tab => "Tab",
        Key::Enter => "Enter",
        Key::Backspace => "Backspace",
        Key::Delete => "Delete",
        Key::Insert => "Insert",
        Key::ArrowUp => "ArrowUp",
        Key::ArrowDown => "ArrowDown",
        Key::ArrowLeft => "ArrowLeft",
        Key::ArrowRight => "ArrowRight",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        _ => return None,
    })
}

/// The character a key with Ctrl stands for, as a hardware keyboard sends it to the overlay.
fn ctrl_char(key: Key) -> Option<String> {
    let name = key.name();
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase().to_string()),
        _ => match key {
            Key::OpenBracket => Some("[".into()),
            Key::CloseBracket => Some("]".into()),
            Key::Backslash => Some("\\".into()),
            Key::Space => Some(" ".into()),
            Key::Minus => Some("-".into()),
            Key::Slash => Some("/".into()),
            _ => None,
        },
    }
}

/// The window's own shortcuts: Command on a Mac, Ctrl+Shift elsewhere; plain Ctrl belongs to terminals.
fn shortcut(modifiers: Modifiers) -> bool {
    modifiers.mac_cmd
        || (cfg!(target_os = "macos") && modifiers.command)
        || (!cfg!(target_os = "macos") && modifiers.ctrl && modifiers.shift)
}

fn over(overlay: &Overlay, pos: Pos2) -> bool {
    overlay.rect.is_some_and(|rect| rect.contains(pos)) && !overlay.covered.iter().any(|r| r.contains(pos))
}

impl Keyboard {
    /// What the pane asked of the overlay after the last frame: to take the keyboard or give it up.
    pub fn follow(&mut self, overlay: &mut Overlay, inputs: &mut Vec<TermInput>) {
        if std::mem::take(&mut overlay.blur) && self.focused {
            self.focused = false;
            inputs.push(TermInput::Focus(false));
        }
        if std::mem::take(&mut overlay.focus) && !self.focused && overlay.writable {
            self.focused = true;
            inputs.push(TermInput::Focus(true));
        }
    }

    /// Take the terminal's share of `raw`: what the overlay would have had. `text_box` says a text box of
    /// the window has the keyboard; `copy` puts the selection on the clipboard. `now_ms` stamps pointers
    /// on the views' clock.
    pub fn route(
        &mut self,
        raw: &mut RawInput,
        overlay: &Overlay,
        text_box: bool,
        now_ms: f64,
        inputs: &mut Vec<TermInput>,
        copy: &mut dyn FnMut(String),
    ) {
        if text_box && self.focused {
            self.focused = false;
            inputs.push(TermInput::Focus(false));
        }
        let mut kept = Vec::with_capacity(raw.events.len());
        for event in std::mem::take(&mut raw.events) {
            if !self.take(&event, overlay, now_ms, inputs, copy) {
                kept.push(event);
            }
        }
        raw.events = kept;
    }

    /// Whether `event` went to the terminal.
    fn take(
        &mut self,
        event: &Event,
        overlay: &Overlay,
        now_ms: f64,
        inputs: &mut Vec<TermInput>,
        copy: &mut dyn FnMut(String),
    ) -> bool {
        let pointer =
            |phase, pos| TermInput::Pointer(Pointer { id: 0, phase, pos, kind: Kind::Mouse, time_ms: now_ms });
        match event {
            Event::PointerMoved(pos) => {
                self.pointer = Some(*pos);
                if self.pressed {
                    inputs.push(pointer(Phase::Move, *pos));
                }
                false
            }
            Event::PointerGone => {
                self.pointer = None;
                if std::mem::take(&mut self.pressed) {
                    inputs.push(TermInput::Pointer(Pointer {
                        id: 0,
                        phase: Phase::Cancel,
                        pos: Pos2::ZERO,
                        kind: Kind::Mouse,
                        time_ms: now_ms,
                    }));
                }
                false
            }
            Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. } => {
                if over(overlay, *pos) {
                    if !self.focused && overlay.writable {
                        self.focused = true;
                        inputs.push(TermInput::Focus(true));
                    }
                    self.pressed = true;
                    inputs.push(pointer(Phase::Down, *pos));
                    true
                } else {
                    // Anything else the window has takes the keyboard from the terminal, as a click away
                    // from the overlay takes the browser's focus.
                    if self.focused {
                        self.focused = false;
                        inputs.push(TermInput::Focus(false));
                    }
                    false
                }
            }
            Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, .. } if self.pressed => {
                self.pressed = false;
                inputs.push(pointer(Phase::Up, *pos));
                true
            }
            Event::MouseWheel { unit, delta, modifiers, .. } => {
                let Some(at) = self.pointer.filter(|pos| over(overlay, *pos)) else { return false };
                let scale = match unit {
                    MouseWheelUnit::Point => 1.0,
                    MouseWheelUnit::Line => LINE,
                    MouseWheelUnit::Page => overlay.rect.map_or(400.0, |r| r.height()),
                };
                let zoom = modifiers.ctrl || shortcut(*modifiers);
                inputs.push(TermInput::Wheel { delta: vec2(delta.x, delta.y) * scale, at, zoom });
                true
            }
            _ if !self.focused => false,
            Event::Text(text) => {
                inputs.push(TermInput::Text(text.clone()));
                true
            }
            Event::Paste(text) => {
                inputs.push(TermInput::Paste(text.clone()));
                true
            }
            Event::Copy => {
                if let Some((text, _, _)) = &overlay.copy {
                    copy(text.clone());
                    inputs.push(TermInput::Copied);
                } else if !cfg!(target_os = "macos") {
                    inputs.push(TermInput::Key { name: "c".into(), mods: Modifiers::CTRL });
                }
                true
            }
            Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                inputs.push(TermInput::Preedit(text.clone()));
                true
            }
            Event::Ime(egui::ImeEvent::Commit(text)) => {
                inputs.push(TermInput::Preedit(String::new()));
                inputs.push(TermInput::Text(text.clone()));
                true
            }
            Event::Key { modifiers, .. } if shortcut(*modifiers) => false,
            event
                if before_terminal(event, OperatingSystem::from_target_os(), overlay.editor)
                    || atomics_ui::window_keys::github_key(
                        event,
                        &overlay.github_key,
                        OperatingSystem::from_target_os(),
                    ) =>
            {
                false
            }
            Event::Key { key, pressed, modifiers, .. } => {
                if *pressed {
                    let name = key_name(*key)
                        .map(str::to_string)
                        .or_else(|| (modifiers.ctrl && !modifiers.alt).then(|| ctrl_char(*key)).flatten());
                    if let Some(name) = name {
                        let mods = Modifiers { command: false, mac_cmd: false, ..*modifiers };
                        inputs.push(TermInput::Key { name, mods });
                    }
                }
                // Every key the terminal has stays out of egui: Space never presses a button, Tab never
                // moves the focus.
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Rect, pos2};

    fn overlay() -> Overlay {
        Overlay {
            rect: Some(Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0))),
            covered: vec![Rect::from_min_max(pos2(80.0, 80.0), pos2(100.0, 100.0))],
            writable: true,
            ..Overlay::default()
        }
    }

    fn route(keyboard: &mut Keyboard, overlay: &Overlay, events: Vec<Event>) -> (Vec<TermInput>, Vec<Event>) {
        let mut raw = RawInput { events, ..RawInput::default() };
        let mut inputs = Vec::new();
        keyboard.route(&mut raw, overlay, false, 1.0, &mut inputs, &mut |_| {});
        (inputs, raw.events)
    }

    fn click(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn a_click_on_the_cells_takes_the_keyboard_and_keys_go_to_the_terminal() {
        let mut keyboard = Keyboard::default();
        let overlay = overlay();
        let (inputs, kept) = route(&mut keyboard, &overlay, vec![key(Key::A, Modifiers::NONE)]);
        assert!(inputs.is_empty() && kept.len() == 1, "nothing is the terminal's before a click");
        let (inputs, kept) =
            route(&mut keyboard, &overlay, vec![click(pos2(10.0, 10.0), true), click(pos2(10.0, 10.0), false)]);
        assert!(keyboard.focused && kept.is_empty());
        assert_eq!(inputs[0], TermInput::Focus(true));
        assert!(matches!(inputs[1], TermInput::Pointer(Pointer { phase: Phase::Down, .. })));
        let (inputs, kept) = route(
            &mut keyboard,
            &overlay,
            vec![
                Event::Text("ls".into()),
                key(Key::Enter, Modifiers::NONE),
                key(Key::C, Modifiers::CTRL),
                key(Key::Tab, Modifiers::NONE),
                key(Key::Q, Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE }),
                Event::Paste("p".into()),
            ],
        );
        assert_eq!(
            inputs,
            vec![
                TermInput::Text("ls".into()),
                TermInput::Key { name: "Enter".into(), mods: Modifiers::NONE },
                TermInput::Key { name: "c".into(), mods: Modifiers::CTRL },
                TermInput::Key { name: "Tab".into(), mods: Modifiers::NONE },
                TermInput::Paste("p".into()),
            ]
        );
        assert_eq!(kept.len(), 1, "the window keeps its Command shortcuts");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn ctrl_shift_chords_stay_in_the_window() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let overlay = overlay();
        let modifiers = Modifiers { ctrl: true, shift: true, command: true, ..Modifiers::NONE };
        for code in [Key::B, Key::D, Key::T, Key::K] {
            let events = vec![
                key(code, modifiers),
                Event::Key { key: code, physical_key: None, pressed: false, repeat: false, modifiers },
            ];
            let (inputs, kept) = route(&mut keyboard, &overlay, events.clone());
            assert!(inputs.is_empty(), "{code:?} with Ctrl+Shift must never become terminal input");
            assert_eq!(kept, events, "egui keeps the chord even if the client has no handler for it");
            assert!(keyboard.focused, "a window shortcut does not blur the terminal");
        }
    }

    #[test]
    fn plain_ctrl_and_other_terminal_keys_still_reach_the_terminal() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let ctrl = Modifiers { command: !cfg!(target_os = "macos"), ..Modifiers::CTRL };
        let (inputs, kept) = route(
            &mut keyboard,
            &overlay(),
            vec![
                key(Key::D, ctrl),
                key(Key::C, ctrl),
                key(Key::Enter, Modifiers::NONE),
                key(Key::Tab, Modifiers::NONE),
                key(Key::ArrowLeft, Modifiers::SHIFT),
                Event::Text("d".into()),
            ],
        );
        assert_eq!(
            inputs,
            vec![
                TermInput::Key { name: "d".into(), mods: Modifiers::CTRL },
                TermInput::Key { name: "c".into(), mods: Modifiers::CTRL },
                TermInput::Key { name: "Enter".into(), mods: Modifiers::NONE },
                TermInput::Key { name: "Tab".into(), mods: Modifiers::NONE },
                TermInput::Key { name: "ArrowLeft".into(), mods: Modifiers::SHIFT },
                TermInput::Text("d".into()),
            ]
        );
        assert!(kept.is_empty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn mac_command_shortcuts_and_ctrl_shift_are_unchanged() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        for modifiers in [Modifiers::MAC_CMD, Modifiers::COMMAND] {
            let event = key(Key::D, modifiers);
            let (inputs, kept) = route(&mut keyboard, &overlay(), vec![event.clone()]);
            assert!(inputs.is_empty());
            assert_eq!(kept, vec![event]);
        }
        let modifiers = Modifiers::CTRL | Modifiers::SHIFT;
        let (inputs, kept) = route(&mut keyboard, &overlay(), vec![key(Key::D, modifiers)]);
        assert_eq!(inputs, vec![TermInput::Key { name: "d".into(), mods: modifiers }]);
        assert!(kept.is_empty(), "Ctrl+Shift still belongs to the terminal on a Mac");
    }

    /// The window's agent chord on this platform.
    fn agent_chord() -> Modifiers {
        if cfg!(target_os = "macos") {
            Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE }
        } else {
            Modifiers { ctrl: true, command: true, ..Modifiers::NONE }
        }
    }

    #[test]
    fn the_windows_keys_stay_in_the_window_for_the_shared_handler_and_no_byte_reaches_the_terminal() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let overlay = overlay();
        for code in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9]
        {
            let event = key(code, agent_chord());
            let (inputs, kept) = route(&mut keyboard, &overlay, vec![event.clone()]);
            assert!(inputs.is_empty(), "{code:?} must never become terminal input");
            assert_eq!(kept, vec![event], "egui keeps it, for `window_keys`");
        }
        for code in [Key::ArrowDown, Key::ArrowUp] {
            let event = key(code, Modifiers::SHIFT);
            let (inputs, kept) = route(&mut keyboard, &overlay, vec![event.clone()]);
            assert!(inputs.is_empty(), "{code:?} with Shift walks the diff's files");
            assert_eq!(kept, vec![event]);
        }
        assert!(keyboard.focused, "the key itself moves no keyboard: the handler does, with the frame");

        // And the handler gets them: the digit picks the agent, whatever the layout types there.
        let ctx = egui::Context::default();
        let (_, kept) = route(&mut keyboard, &overlay, vec![key(Key::Num3, agent_chord())]);
        let mut picked = Vec::new();
        let mut output = ctx.run_ui(RawInput { events: kept, ..RawInput::default() }, |ui| {
            picked = atomics_ui::window_keys::take_agents(ui.ctx());
        });
        output.textures_delta.clear();
        assert_eq!(picked, vec![2]);
    }

    #[test]
    fn an_editor_keeps_shift_with_the_arrows_and_a_terminal_its_page_keys() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let editor = Overlay { editor: true, ..overlay() };
        let (inputs, kept) = route(&mut keyboard, &editor, vec![key(Key::ArrowDown, Modifiers::SHIFT)]);
        assert_eq!(inputs, vec![TermInput::Key { name: "ArrowDown".into(), mods: Modifiers::SHIFT }]);
        assert!(kept.is_empty());
        let (inputs, kept) = route(&mut keyboard, &editor, vec![key(Key::Num2, agent_chord())]);
        assert!(inputs.is_empty() && kept.len() == 1, "an agent chord leaves an editor too");

        let (inputs, kept) = route(&mut keyboard, &overlay(), vec![key(Key::PageDown, Modifiers::NONE)]);
        assert_eq!(inputs, vec![TermInput::Key { name: "PageDown".into(), mods: Modifiers::NONE }]);
        assert!(kept.is_empty());
    }

    #[test]
    fn a_digit_with_ctrl_that_is_no_agent_chord_types_nothing() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let modes = atomics_core::screen::Modes::default();
        // Ctrl+0 on every platform, and Ctrl with any digit on a Mac, where the chord is Command's.
        let digits: &[Key] = if cfg!(target_os = "macos") { &[Key::Num0, Key::Num1, Key::Num3] } else { &[Key::Num0] };
        for code in digits {
            let (inputs, _) = route(&mut keyboard, &overlay(), vec![key(*code, Modifiers::CTRL)]);
            for input in inputs {
                let TermInput::Key { name, mods } = input else { panic!("{input:?}") };
                assert_eq!(atomics_web::term_pane::key_text(&name, mods, (false, false), &modes), None, "{code:?}");
            }
        }
    }

    #[test]
    fn a_click_elsewhere_or_on_a_window_over_the_cells_lets_go() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let overlay = overlay();
        let (inputs, kept) = route(&mut keyboard, &overlay, vec![click(pos2(90.0, 90.0), true)]);
        assert_eq!(inputs, vec![TermInput::Focus(false)]);
        assert_eq!(kept.len(), 1, "the window over the cells gets its click");
        let (inputs, kept) = route(&mut keyboard, &overlay, vec![Event::Text("x".into())]);
        assert!(inputs.is_empty() && kept.len() == 1);
        // A text box taking the keyboard lets go too.
        keyboard.focused = true;
        let mut raw = RawInput::default();
        let mut inputs = Vec::new();
        keyboard.route(&mut raw, &overlay, true, 0.0, &mut inputs, &mut |_| {});
        assert_eq!(inputs, vec![TermInput::Focus(false)]);
    }

    #[test]
    fn the_wheel_over_the_cells_scrolls_the_terminal_and_copy_takes_the_selection() {
        let mut keyboard = Keyboard::default();
        let mut overlay = overlay();
        overlay.copy = Some(("selected".into(), pos2(0.0, 0.0), 0.0));
        let wheel = |modifiers| Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: vec2(0.0, -1.0),
            modifiers,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
        };
        let (inputs, kept) =
            route(&mut keyboard, &overlay, vec![Event::PointerMoved(pos2(20.0, 20.0)), wheel(Modifiers::NONE)]);
        assert_eq!(kept.len(), 1, "only the move stays egui's");
        assert_eq!(inputs, vec![TermInput::Wheel { delta: vec2(0.0, -LINE), at: pos2(20.0, 20.0), zoom: false }]);
        keyboard.focused = true;
        let mut copied = None;
        let mut raw = RawInput { events: vec![Event::Copy], ..RawInput::default() };
        let mut inputs = Vec::new();
        keyboard.route(&mut raw, &overlay, false, 0.0, &mut inputs, &mut |text| copied = Some(text));
        assert_eq!(copied.as_deref(), Some("selected"));
        assert_eq!(inputs, vec![TermInput::Copied]);
    }

    #[test]
    fn copy_without_a_selection_routes_control_c_off_macos() {
        let mut keyboard = Keyboard { focused: true, ..Keyboard::default() };
        let (inputs, kept) = route(&mut keyboard, &overlay(), vec![Event::Copy]);
        assert!(kept.is_empty());
        #[cfg(not(target_os = "macos"))]
        assert_eq!(inputs, vec![TermInput::Key { name: "c".into(), mods: Modifiers::CTRL }]);
        #[cfg(target_os = "macos")]
        assert!(inputs.is_empty());
    }

    #[test]
    fn the_pane_gives_and_takes_the_keyboard() {
        let mut keyboard = Keyboard::default();
        let mut overlay = Overlay { focus: true, writable: true, ..Overlay::default() };
        let mut inputs = Vec::new();
        keyboard.follow(&mut overlay, &mut inputs);
        assert!(keyboard.focused && !overlay.focus);
        overlay.blur = true;
        keyboard.follow(&mut overlay, &mut inputs);
        assert!(!keyboard.focused);
        assert_eq!(inputs, vec![TermInput::Focus(true), TermInput::Focus(false)]);
    }
}

#[cfg(test)]
mod github_key_tests {
    use super::*;
    #[test]
    fn configured_github_chord_stays_out_of_a_focused_terminal() {
        let mut keyboard = Keyboard { focused: true, ..Default::default() };
        let overlay = Overlay { github_key: "ctrl+g".into(), writable: true, ..Default::default() };
        let event = Event::Key {
            key: Key::G,
            physical_key: Some(Key::U),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        let mut raw = RawInput { events: vec![event.clone()], ..Default::default() };
        let mut inputs = Vec::new();
        keyboard.route(&mut raw, &overlay, false, 1.0, &mut inputs, &mut |_| {});
        assert_eq!(raw.events, vec![event]);
        assert!(inputs.is_empty());
    }
}
