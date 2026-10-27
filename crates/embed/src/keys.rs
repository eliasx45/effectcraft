//! AppKit key events → egui events, as egui-winit makes them from winit's (EMB1).
//!
//! - `key`: the logical key, from `charactersIgnoringModifiers` (the active layout, Shift kept):
//!   on AZERTY the key labelled Z is `Key::Z`, so ⌘Z is Undo where the person sees Z; a character
//!   egui has no key for (`&`, `é` on AZERTY's number row) falls back to the physical key, as
//!   egui-winit does, so ⌘1 still works there.
//! - `physical_key`: the key's position (macOS virtual key code, US-ANSI names).
//! - ⌘X / ⌘C / ⌘V become `Cut` / `Copy` / `Paste(text)` instead of a key event, as egui-winit.
//! - typed text (`characters`) without ⌘ or ⌃ becomes `Text`.

/// One AppKit `keyDown:` / `keyUp:`.
#[derive(Clone, Copy, Debug)]
pub struct MacKey<'a> {
    /// `NSEvent.keyCode` (a `kVK_*` virtual key code).
    pub keycode: u16,
    /// `NSEvent.charactersIgnoringModifiers`.
    pub chars_ignoring_modifiers: &'a str,
    /// `NSEvent.characters` (what the key types).
    pub text: &'a str,
    pub pressed: bool,
    pub modifiers: egui::Modifiers,
}

/// `NSEvent.modifierFlags` → egui modifiers (⌘ is `command` on macOS).
pub fn mac_modifiers(flags: u64) -> egui::Modifiers {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    let cmd = flags & COMMAND != 0;
    egui::Modifiers { alt: flags & OPTION != 0, ctrl: flags & CONTROL != 0, shift: flags & SHIFT != 0, mac_cmd: cmd, command: cmd }
}

/// The egui events for `key`; `clipboard` is read only for a paste.
pub(crate) fn events(key: &MacKey<'_>, clipboard: impl FnOnce() -> Option<String>) -> Vec<egui::Event> {
    let mut out = Vec::new();
    let physical = physical_key(key.keycode);
    let logical = special_key(key.keycode).or_else(|| logical_key(key.chars_ignoring_modifiers));
    if let Some(active) = logical.or(physical) {
        let m = key.modifiers;
        if key.pressed && (active == egui::Key::Cut || (m.command && active == egui::Key::X)) {
            out.push(egui::Event::Cut);
            return out;
        }
        if key.pressed && (active == egui::Key::Copy || (m.command && active == egui::Key::C)) {
            out.push(egui::Event::Copy);
            return out;
        }
        if key.pressed && (active == egui::Key::Paste || (m.command && active == egui::Key::V)) {
            if let Some(text) = clipboard().map(|t| t.replace("\r\n", "\n")).filter(|t| !t.is_empty()) {
                out.push(egui::Event::Paste(text));
            }
            return out;
        }
        out.push(egui::Event::Key { key: active, physical_key: physical, pressed: key.pressed, repeat: false, modifiers: m });
    }
    let command = key.modifiers.ctrl || key.modifiers.command;
    if key.pressed && !command && !key.text.is_empty() && key.text.chars().all(is_printable) {
        out.push(egui::Event::Text(key.text.to_string()));
    }
    out
}

/// What egui-winit counts as typed text: no control characters and nothing from the private use
/// area (AppKit reports arrows and function keys as U+F700…).
fn is_printable(c: char) -> bool {
    let private_use = ('\u{e000}'..='\u{f8ff}').contains(&c) || ('\u{f0000}'..='\u{ffffd}').contains(&c) || ('\u{100000}'..='\u{10fffd}').contains(&c);
    !private_use && !c.is_ascii_control()
}

/// The logical key from the layout's character: a single character egui names (letters either
/// case, digits, punctuation).
fn logical_key(chars: &str) -> Option<egui::Key> {
    let mut it = chars.chars();
    let c = it.next()?;
    if it.next().is_some() {
        return None;
    }
    egui::Key::from_name(c.encode_utf8(&mut [0; 4]))
}

/// Keys whose meaning doesn't depend on the layout (arrows, Return, Escape, F-keys…).
fn special_key(code: u16) -> Option<egui::Key> {
    use egui::Key as K;
    Some(match code {
        0x24 | 0x4C => K::Enter,
        0x30 => K::Tab,
        0x31 => K::Space,
        0x33 => K::Backspace,
        0x35 => K::Escape,
        0x72 => K::Insert,
        0x73 => K::Home,
        0x74 => K::PageUp,
        0x75 => K::Delete,
        0x77 => K::End,
        0x79 => K::PageDown,
        0x7B => K::ArrowLeft,
        0x7C => K::ArrowRight,
        0x7D => K::ArrowDown,
        0x7E => K::ArrowUp,
        0x7A => K::F1,
        0x78 => K::F2,
        0x63 => K::F3,
        0x76 => K::F4,
        0x60 => K::F5,
        0x61 => K::F6,
        0x62 => K::F7,
        0x64 => K::F8,
        0x65 => K::F9,
        0x6D => K::F10,
        0x67 => K::F11,
        0x6F => K::F12,
        0x69 => K::F13,
        0x6B => K::F14,
        0x71 => K::F15,
        0x6A => K::F16,
        0x40 => K::F17,
        0x4F => K::F18,
        0x50 => K::F19,
        0x5A => K::F20,
        // Keypad keys are the same on every layout.
        0x52 => K::Num0,
        0x53 => K::Num1,
        0x54 => K::Num2,
        0x55 => K::Num3,
        0x56 => K::Num4,
        0x57 => K::Num5,
        0x58 => K::Num6,
        0x59 => K::Num7,
        0x5B => K::Num8,
        0x5C => K::Num9,
        0x41 => K::Period,
        0x45 => K::Plus,
        0x4E => K::Minus,
        0x4B => K::Slash,
        0x51 => K::Equals,
        _ => return None,
    })
}

/// The key at this position on a US-ANSI keyboard (`kVK_ANSI_*`), else the layout-independent key.
fn physical_key(code: u16) -> Option<egui::Key> {
    use egui::Key as K;
    Some(match code {
        0x00 => K::A,
        0x01 => K::S,
        0x02 => K::D,
        0x03 => K::F,
        0x04 => K::H,
        0x05 => K::G,
        0x06 => K::Z,
        0x07 => K::X,
        0x08 => K::C,
        0x09 => K::V,
        0x0B => K::B,
        0x0C => K::Q,
        0x0D => K::W,
        0x0E => K::E,
        0x0F => K::R,
        0x10 => K::Y,
        0x11 => K::T,
        0x12 => K::Num1,
        0x13 => K::Num2,
        0x14 => K::Num3,
        0x15 => K::Num4,
        0x16 => K::Num6,
        0x17 => K::Num5,
        0x18 => K::Equals,
        0x19 => K::Num9,
        0x1A => K::Num7,
        0x1B => K::Minus,
        0x1C => K::Num8,
        0x1D => K::Num0,
        0x1E => K::CloseBracket,
        0x1F => K::O,
        0x20 => K::U,
        0x21 => K::OpenBracket,
        0x22 => K::I,
        0x23 => K::P,
        0x25 => K::L,
        0x26 => K::J,
        0x27 => K::Quote,
        0x28 => K::K,
        0x29 => K::Semicolon,
        0x2A => K::Backslash,
        0x2B => K::Comma,
        0x2C => K::Slash,
        0x2D => K::N,
        0x2E => K::M,
        0x2F => K::Period,
        0x32 => K::Backtick,
        other => return special_key(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(keycode: u16, chars: &'static str, text: &'static str, modifiers: egui::Modifiers) -> MacKey<'static> {
        MacKey { keycode, chars_ignoring_modifiers: chars, text, pressed: true, modifiers }
    }

    #[test]
    fn azerty_letters_are_their_own_letters_and_keep_their_position() {
        // AZERTY: the key at QWERTY's W (kVK_ANSI_W) types "z"; ⌘ on it is ⌘Z (Undo).
        let cmd = mac_modifiers(1 << 20);
        let ev = events(&key(0x0D, "z", "z", cmd), || None);
        assert_eq!(ev, vec![egui::Event::Key { key: egui::Key::Z, physical_key: Some(egui::Key::W), pressed: true, repeat: false, modifiers: cmd }]);
        // AZERTY "a" sits at QWERTY's Q: plain, it types the text too.
        let ev = events(&key(0x0C, "a", "a", egui::Modifiers::NONE), || None);
        assert_eq!(ev.len(), 2);
        assert!(matches!(&ev[0], egui::Event::Key { key: egui::Key::A, physical_key: Some(egui::Key::Q), .. }));
        assert_eq!(ev[1], egui::Event::Text("a".into()));
    }

    #[test]
    fn azerty_number_row_falls_back_to_the_position() {
        // "&" on AZERTY's 1 key: egui has no key for it, so ⌘& is ⌘1 (as egui-winit).
        let cmd = mac_modifiers(1 << 20);
        let ev = events(&key(0x12, "&", "&", cmd), || None);
        assert!(matches!(&ev[0], egui::Event::Key { key: egui::Key::Num1, .. }));
        assert_eq!(ev.len(), 1, "no text with ⌘");
    }

    #[test]
    fn space_and_arrows_are_keys_without_private_use_text() {
        let ev = events(&key(0x31, " ", " ", egui::Modifiers::NONE), || None);
        assert!(matches!(&ev[0], egui::Event::Key { key: egui::Key::Space, .. }));
        let ev = events(&key(0x7B, "\u{f702}", "\u{f702}", egui::Modifiers::NONE), || None);
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], egui::Event::Key { key: egui::Key::ArrowLeft, .. }));
    }

    #[test]
    fn clipboard_chords_become_copy_cut_paste() {
        let cmd = mac_modifiers(1 << 20);
        assert_eq!(events(&key(0x08, "c", "c", cmd), || None), vec![egui::Event::Copy]);
        assert_eq!(events(&key(0x07, "x", "x", cmd), || None), vec![egui::Event::Cut]);
        assert_eq!(events(&key(0x09, "v", "v", cmd), || Some("a\r\nb".into())), vec![egui::Event::Paste("a\nb".into())]);
    }

    #[test]
    fn modifier_flags() {
        let m = mac_modifiers((1 << 17) | (1 << 19));
        assert!(m.shift && m.alt && !m.command && !m.ctrl);
    }
}
