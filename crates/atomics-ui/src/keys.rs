//! Keyboard chords: a `[keys]` binding as an egui shortcut, taking it from the input before a terminal sees
//! it, and its label in the platform's own words.

use atomics_core::config::{Chord, KeyConfig};
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

/// The complete configured binding table, shared by diagnostics and shortcut consumers.
pub fn bindings(keys: &KeyConfig) -> [(&'static str, &str); 16] {
    [
        ("toggle_diff", &keys.toggle_diff),
        ("next_attention", &keys.next_attention),
        ("focus_steer", &keys.focus_steer),
        ("new_session", &keys.new_session),
        ("refresh_diff", &keys.refresh_diff),
        ("close_session", &keys.close_session),
        ("quick_open", &keys.quick_open),
        ("toggle_editor", &keys.toggle_editor),
        ("toggle_plan", &keys.toggle_plan),
        ("toggle_chief", &keys.toggle_chief),
        ("toggle_tickets", &keys.toggle_tickets),
        ("plan_with_chief", &keys.plan_with_chief),
        ("voice", &keys.voice),
        ("github", &keys.github),
        ("back", &keys.back),
        ("forward", &keys.forward),
    ]
}

pub fn invalid_bindings(keys: &KeyConfig, os: egui::os::OperatingSystem) -> Vec<&'static str> {
    bindings(keys)
        .into_iter()
        .filter_map(|(name, text)| (!text.trim().is_empty() && shortcut_for_os(text, os).is_none()).then_some(name))
        .collect()
}

/// Every binding and fixed chord that a new shortcut must leave alone.
pub fn taken_shortcuts<'a>(
    bindings: impl IntoIterator<Item = (&'a str, &'a str)>,
    except: &str,
    os: egui::os::OperatingSystem,
) -> Vec<(String, KeyboardShortcut)> {
    let mut taken: Vec<_> = bindings
        .into_iter()
        .filter(|(name, _)| *name != except)
        .filter_map(|(name, text)| Some((format!("[keys] {name}"), shortcut_for_os(text, os)?)))
        .collect();
    for (index, key) in
        [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9]
            .into_iter()
            .enumerate()
    {
        taken.push((
            format!("the agent chord for agent {}", index + 1),
            KeyboardShortcut::new(crate::window_keys::agent_modifiers(os), key),
        ));
        taken.push((
            format!("the ticket filter chord for chip {}", index + 1),
            KeyboardShortcut::new(Modifiers::ALT, key),
        ));
    }
    for (key, modifiers, name) in [
        (Key::ArrowUp, Modifiers::SHIFT, "the previous changed file chord"),
        (Key::ArrowDown, Modifiers::SHIFT, "the next changed file chord"),
        (Key::PageUp, Modifiers::NONE, "the previous hunk chord"),
        (Key::PageDown, Modifiers::NONE, "the next hunk chord"),
        (Key::Escape, Modifiers::NONE, "the close window chord"),
    ] {
        taken.push((name.into(), KeyboardShortcut::new(modifiers, key)));
    }
    taken
}

pub fn shortcut_owner<'a>(taken: &'a [(String, KeyboardShortcut)], chord: &KeyboardShortcut) -> Option<&'a str> {
    taken.iter().find(|(_, other)| other == chord).map(|(name, _)| name.as_str())
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
    fn reserved_and_taken_shortcuts_share_the_client_os_and_every_binding() {
        for os in [egui::os::OperatingSystem::Nix, egui::os::OperatingSystem::Mac] {
            let keys = KeyConfig::default();
            let taken = taken_shortcuts(bindings(&keys), "github", os);
            for n in 1..=9 {
                let chord = shortcut_for_os(&format!("alt+{n}"), os).unwrap();
                assert!(shortcut_owner(&taken, &chord).unwrap().contains("ticket filter"));
                let chord = shortcut_for_os(&format!("primary+{n}"), os).unwrap();
                assert!(shortcut_owner(&taken, &chord).unwrap().contains("agent chord"));
            }
            let plan = shortcut_for_os(&keys.plan_with_chief, os).unwrap();
            assert_eq!(shortcut_owner(&taken, &plan), Some("[keys] plan_with_chief"));
        }
    }

    #[test]
    fn every_unparsable_configured_binding_is_reported_and_blank_disables() {
        let mut values = serde_json::to_value(KeyConfig::default()).unwrap();
        for value in values.as_object_mut().unwrap().values_mut() {
            *value = "cmd+d+e".into();
        }
        let keys: KeyConfig = serde_json::from_value(values).unwrap();
        assert_eq!(invalid_bindings(&keys, egui::os::OperatingSystem::Nix).len(), 16);
        let keys = KeyConfig { quick_open: String::new(), github: String::new(), ..KeyConfig::default() };
        assert!(invalid_bindings(&keys, egui::os::OperatingSystem::Nix).is_empty());
    }

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
