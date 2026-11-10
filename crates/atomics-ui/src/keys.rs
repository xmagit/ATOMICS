//! Keyboard chords: a `[keys]` binding as an egui shortcut, taking it from the input before a terminal sees
//! it, and its label in the platform's own words.

use atomics_core::config::Chord;
use egui::{Key, KeyboardShortcut, Modifiers};

/// The modifier reserved for ATOMICS's own shortcuts, so it never reaches a terminal:
/// Command on macOS, Ctrl+Shift elsewhere (plain Ctrl belongs to the shell).
pub fn is_app_chord(m: &Modifiers) -> bool {
    if cfg!(target_os = "macos") { m.mac_cmd } else { m.ctrl && m.shift }
}

pub fn shortcut(text: &str) -> Option<KeyboardShortcut> {
    shortcut_for_os(text, egui::os::OperatingSystem::from_target_os())
}

/// Resolve primary/cmd on the client's OS, including a browser running on another OS than its host.
pub fn shortcut_for_os(text: &str, os: egui::os::OperatingSystem) -> Option<KeyboardShortcut> {
    let chord = Chord::parse(text)?;
    let mut chars = chord.key.chars();
    let capitalized: String =
        chars.next().into_iter().flat_map(char::to_uppercase).chain(chars.flat_map(char::to_lowercase)).collect();
    let key = Key::from_name(&chord.key)
        .or_else(|| Key::from_name(&chord.key.to_uppercase()))
        .or_else(|| Key::from_name(&capitalized))?;
    let mut modifiers = Modifiers::NONE;
    if chord.cmd {
        modifiers |= if crate::window_keys::command_chords(os) {
            Modifiers::MAC_CMD
        } else {
            Modifiers::CTRL | Modifiers::SHIFT
        };
    }
    if chord.primary {
        modifiers |= if crate::window_keys::command_chords(os) { Modifiers::MAC_CMD } else { Modifiers::CTRL };
    }
    if chord.ctrl {
        modifiers |= Modifiers::CTRL;
    }
    if chord.alt {
        modifiers |= Modifiers::ALT;
    }
    if chord.shift {
        modifiers |= Modifiers::SHIFT;
    }
    Some(KeyboardShortcut::new(modifiers, key))
}

/// Navigation follows physical keys across layouts; extra modifiers belong to the terminal.
pub fn consume_physical_shortcut(input: &mut egui::InputState, chord: &KeyboardShortcut) -> bool {
    let before = input.events.len();
    input.events.retain(|event| {
        !matches!(event, egui::Event::Key { key, physical_key, modifiers, pressed: true, .. }
            if physical_key.unwrap_or(*key) == chord.logical_key
                && modifiers.matches_exact(chord.modifiers))
    });
    input.events.len() < before
}

/// Consumes a configured chord taken before the terminal, with exactly its modifiers. A letter
/// matches the key the keyboard layout types, as the other `[keys]` bindings do, so Ctrl+G is the
/// key labelled G on Colemak or Dvorak too. A digit matches its physical key, like Ctrl/Command+1…9,
/// because some layouts type digits only with Shift. Layouts without Latin letters still work:
/// egui reports the Latin key at that position as the logical key.
pub fn consume_chord(input: &mut egui::InputState, chord: &KeyboardShortcut) -> bool {
    if is_digit(chord.logical_key) {
        return consume_physical_shortcut(input, chord);
    }
    let before = input.events.len();
    input.events.retain(|event| {
        !matches!(event, egui::Event::Key { key, modifiers, pressed: true, .. }
            if *key == chord.logical_key && modifiers.matches_exact(chord.modifiers))
    });
    input.events.len() < before
}

pub fn is_digit(key: Key) -> bool {
    matches!(
        key,
        Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
    )
}

/// Display exactly the platform modifiers used to consume the configured chord.
pub fn shortcut_label(ctx: &egui::Context, text: &str) -> String {
    shortcut_for_os(text, ctx.os()).map(|chord| ctx.format_shortcut(&chord)).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_parse() {
        let s = shortcut("cmd+d").unwrap();
        assert_eq!(s.logical_key, Key::D);
        let voice = shortcut("cmd+shift+space").unwrap();
        assert_eq!(voice.logical_key, Key::Space);
        assert!(voice.modifiers.shift);
        assert_eq!(shortcut("cmd+Escape").unwrap().logical_key, Key::Escape);
        assert!(shortcut("cmd+notakey").is_none());
        let cmd = shortcut("cmd+d").unwrap().modifiers;
        assert!(is_app_chord(&cmd));
        if cfg!(target_os = "macos") {
            assert!(cmd.mac_cmd && !cmd.ctrl && !cmd.shift);
        } else {
            assert!(cmd.ctrl && cmd.shift && !cmd.mac_cmd);
            assert!(!is_app_chord(&Modifiers::CTRL), "plain Ctrl must reach the terminal");
            assert_eq!(shortcut("cmd+shift+space"), shortcut("ctrl+shift+space"));
        }
    }

    #[test]
    fn digit_chords_match_the_physical_key_when_shift_types_a_symbol() {
        let chord = KeyboardShortcut::new(shortcut("cmd+a").unwrap().modifiers, Key::Num1);
        let press = |key, physical_key, modifiers| egui::Event::Key {
            key,
            physical_key: Some(physical_key),
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut input = egui::InputState::default();
        // Shift+1 arrives as `!` on most layouts; the physical key is still 1.
        input.events = vec![press(Key::Exclamationmark, Key::Num1, chord.modifiers)];
        assert!(consume_physical_shortcut(&mut input, &chord));
        assert!(input.events.is_empty());
        input.events = vec![press(Key::Num1, Key::Num1, chord.modifiers)];
        assert!(consume_physical_shortcut(&mut input, &chord), "an unshifted digit still matches");
        input.events = vec![egui::Event::Key {
            key: Key::Num1,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: chord.modifiers,
        }];
        assert!(consume_physical_shortcut(&mut input, &chord), "fall back when no physical key is supplied");
        for other in [
            press(Key::Num2, Key::Num2, chord.modifiers),
            press(Key::Num1, Key::Num2, chord.modifiers),
            press(Key::Num1, Key::Num1, Modifiers::CTRL),
            press(Key::Num1, Key::Num1, chord.modifiers | Modifiers::ALT),
        ] {
            input.events = vec![other];
            assert!(!consume_physical_shortcut(&mut input, &chord));
            assert_eq!(input.events.len(), 1, "other digits and plain Ctrl stay for the terminal");
        }
    }
}
