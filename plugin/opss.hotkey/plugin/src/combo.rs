//! Platform-neutral hotkey combination parsing.
//!
//! The combo vocabulary mirrors the host's global-hotkey parser *plus* the
//! left/right-specific modifier tokens and mouse side buttons introduced by
//! the Voice-Typing plugin, so combo strings are portable across MicYou
//! plugins and expressive enough for IME-style modifier-only bindings
//! (`lctrl+lshift`) which the host's `register_hotkey` cannot represent.
//!
//! Parsing produces a neutral [`Combo`]; each platform capture backend maps
//! it onto native key codes (Win32 VK / CGKeyCode / evdev) at evaluation
//! time. The same safety rules as Voice-Typing apply:
//! * a bare letter/digit/ordinary key as main key is rejected — it would
//!   hijack normal typing system-wide; bare main keys are limited to F1–F24
//!   and the mouse side buttons;
//! * modifier-only combos need at least two modifiers (a lone `lctrl` would
//!   fire on every Ctrl+C).

use std::fmt;

// ─────────────────────────── modifiers ───────────────────────────

/// Exact (left/right distinguished) modifier bits, stored in [`Combo::exact`].
pub const EX_LCTRL: u8 = 1 << 0;
pub const EX_RCTRL: u8 = 1 << 1;
pub const EX_LALT: u8 = 1 << 2;
pub const EX_RALT: u8 = 1 << 3;
pub const EX_LSHIFT: u8 = 1 << 4;
pub const EX_RSHIFT: u8 = 1 << 5;
pub const EX_LWIN: u8 = 1 << 6;
pub const EX_RWIN: u8 = 1 << 7;

/// Side-agnostic modifier bits, stored in [`Combo::any`].
pub const ANY_CTRL: u8 = 1 << 0;
pub const ANY_ALT: u8 = 1 << 1;
pub const ANY_SHIFT: u8 = 1 << 2;
pub const ANY_WIN: u8 = 1 << 3;

// ─────────────────────────── main key ───────────────────────────

/// The non-modifier key of a combo. Keyboard keys are physical-position
/// based on every backend (Win32 VK letters, macOS ANSI keycodes and evdev
/// KEY_* all follow the US-QWERTY physical layout), so a parsed combo means
/// the same physical keys everywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MainKey {
    Letter(u8), // b'a'..=b'z'
    Digit(u8),  // b'0'..=b'9'
    Func(u8),   // 1..=24 → F1..F24
    Mouse4,     // side button "back"  (Win VK_XBUTTON1 / BTN_SIDE / CG button 3)
    Mouse5,     // side button "forward" (Win VK_XBUTTON2 / BTN_EXTRA / CG button 4)
    Space,
    Enter,
    Tab,
    Esc,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Minus,
    Equal,
    BracketLeft,
    BracketRight,
    Backslash,
    Semicolon,
    Quote,
    Comma,
    Period,
    Slash,
    Backquote,
    Pause,
}

impl MainKey {
    /// Canonical config token for this key (used for display + panel round-trips).
    pub fn token(self) -> &'static str {
        // Letters/digits need owned strings; they are rendered by Display instead.
        match self {
            MainKey::Mouse4 => "mouse4",
            MainKey::Mouse5 => "mouse5",
            MainKey::Space => "space",
            MainKey::Enter => "enter",
            MainKey::Tab => "tab",
            MainKey::Esc => "esc",
            MainKey::Backspace => "backspace",
            MainKey::Delete => "delete",
            MainKey::Insert => "insert",
            MainKey::Home => "home",
            MainKey::End => "end",
            MainKey::PageUp => "pageup",
            MainKey::PageDown => "pagedown",
            MainKey::Up => "up",
            MainKey::Down => "down",
            MainKey::Left => "left",
            MainKey::Right => "right",
            MainKey::Minus => "minus",
            MainKey::Equal => "equal",
            MainKey::BracketLeft => "bracketleft",
            MainKey::BracketRight => "bracketright",
            MainKey::Backslash => "backslash",
            MainKey::Semicolon => "semicolon",
            MainKey::Quote => "quote",
            MainKey::Comma => "comma",
            MainKey::Period => "period",
            MainKey::Slash => "slash",
            MainKey::Backquote => "backquote",
            MainKey::Pause => "pause",
            MainKey::Letter(_) | MainKey::Digit(_) | MainKey::Func(_) => "",
        }
    }

    pub fn is_mouse(self) -> bool {
        matches!(self, MainKey::Mouse4 | MainKey::Mouse5)
    }
}

impl fmt::Display for MainKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MainKey::Letter(c) => write!(f, "{}", c as char),
            MainKey::Digit(d) => write!(f, "{}", d as char),
            MainKey::Func(n) => write!(f, "f{n}"),
            other => f.write_str(other.token()),
        }
    }
}

// ─────────────────────────── combo ───────────────────────────

/// A parsed key combination. `main == None` means a modifier-only combo.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Combo {
    /// Exact left/right modifier bits (`EX_*`).
    pub exact: u8,
    /// Side-agnostic modifier bits (`ANY_*`).
    pub any: u8,
    /// Optional non-modifier main key.
    pub main: Option<MainKey>,
}

impl Combo {
    pub fn mod_count(&self) -> usize {
        self.exact.count_ones() as usize + self.any.count_ones() as usize
    }

    /// True when the main key is a mouse side button — backends attach their
    /// mouse capture path only while such a combo is active.
    #[allow(dead_code)] // used by the Windows backend only
    pub fn needs_mouse(&self) -> bool {
        self.main.is_some_and(|m| m.is_mouse())
    }

    /// Best-effort rendering for the host's `register_hotkey` (global-hotkey
    /// crate syntax: generic modifiers + lowercase main key). `None` when the
    /// combo cannot be expressed there (modifier-only combos, mouse buttons)
    /// — callers report those bindings as unsupported in fallback mode.
    /// Left/right-specific modifiers degrade to their generic form.
    pub fn to_host_string(&self) -> Option<String> {
        // global-hotkey requires a non-modifier main key.
        let main = self.main?;
        if main.is_mouse() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        if self.any & ANY_CTRL != 0 || self.exact & (EX_LCTRL | EX_RCTRL) != 0 {
            parts.push("ctrl".into());
        }
        if self.any & ANY_ALT != 0 || self.exact & (EX_LALT | EX_RALT) != 0 {
            parts.push("alt".into());
        }
        if self.any & ANY_SHIFT != 0 || self.exact & (EX_LSHIFT | EX_RSHIFT) != 0 {
            parts.push("shift".into());
        }
        if self.any & ANY_WIN != 0 || self.exact & (EX_LWIN | EX_RWIN) != 0 {
            parts.push("super".into());
        }
        parts.push(match main {
            MainKey::Letter(c) => (c as char).to_string(),
            MainKey::Digit(d) => (d as char).to_string(),
            _ => main.to_string(),
        });
        Some(parts.join("+"))
    }
}

impl fmt::Display for Combo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        for (bit, name) in [
            (EX_LCTRL, "lctrl"),
            (EX_RCTRL, "rctrl"),
            (EX_LALT, "lalt"),
            (EX_RALT, "ralt"),
            (EX_LSHIFT, "lshift"),
            (EX_RSHIFT, "rshift"),
            (EX_LWIN, "lwin"),
            (EX_RWIN, "rwin"),
        ] {
            if self.exact & bit != 0 {
                parts.push(name.into());
            }
        }
        for (bit, name) in [
            (ANY_CTRL, "ctrl"),
            (ANY_ALT, "alt"),
            (ANY_SHIFT, "shift"),
            (ANY_WIN, "win"),
        ] {
            if self.any & bit != 0 {
                parts.push(name.into());
            }
        }
        if let Some(m) = self.main {
            parts.push(m.to_string());
        }
        write!(f, "{}", parts.join("+"))
    }
}

// ─────────────────────────── parsing ───────────────────────────

/// Parse a combo string like `lctrl+lshift`, `ctrl+alt+m`, `f13` or
/// `ctrl+mouse5`. Errors carry precise Chinese messages (shown verbatim in
/// the plugin log and panel), identical in spirit to Voice-Typing's parser.
pub fn parse_combo(text: &str) -> Result<Combo, String> {
    let mut combo = Combo::default();
    let mut main: Option<MainKey> = None;
    let mut tokens = 0usize;

    for raw in text.split('+') {
        let tok = raw.trim().to_ascii_lowercase();
        if tok.is_empty() {
            continue;
        }
        tokens += 1;
        match tok.as_str() {
            "ctrl" | "control" => combo.any |= ANY_CTRL,
            "lctrl" | "leftctrl" => combo.exact |= EX_LCTRL,
            "rctrl" | "rightctrl" => combo.exact |= EX_RCTRL,
            "alt" | "option" => combo.any |= ANY_ALT,
            "lalt" | "leftalt" | "loption" => combo.exact |= EX_LALT,
            "ralt" | "rightalt" | "roption" => combo.exact |= EX_RALT,
            "shift" => combo.any |= ANY_SHIFT,
            "lshift" | "leftshift" => combo.exact |= EX_LSHIFT,
            "rshift" | "rightshift" => combo.exact |= EX_RSHIFT,
            "win" | "super" | "meta" | "cmd" | "command" => combo.any |= ANY_WIN,
            "lwin" | "leftwin" | "lcmd" | "lcommand" | "lmeta" => combo.exact |= EX_LWIN,
            "rwin" | "rightwin" | "rcmd" | "rcommand" | "rmeta" => combo.exact |= EX_RWIN,
            other => {
                if main.is_some() {
                    return Err(format!("快捷键只能有一个主键（多余 token: {other}）"));
                }
                main = Some(parse_main_key(other)?);
            }
        }
    }

    if tokens == 0 {
        return Err("快捷键不能为空".into());
    }

    if let Some(m) = main {
        combo.main = Some(m);
        if combo.mod_count() == 0 {
            // Bare main key: only F1–F24 and the mouse side buttons are safe
            // (neither produces text input; everything else would hijack
            // normal typing / shortcuts system-wide).
            let is_fkey = matches!(m, MainKey::Func(_));
            if !is_fkey && !m.is_mouse() {
                return Err(
                    "字母/数字/普通键作主键时必须搭配至少一个修饰键（ctrl/alt/shift/win）；无修饰键时仅支持 F1-F24 与鼠标侧键（mouse4/mouse5）"
                        .into(),
                );
            }
        }
    } else if combo.mod_count() < 2 {
        return Err("纯修饰键组合至少需要两个修饰键（如 lctrl+lshift），避免误触".into());
    }

    Ok(combo)
}

fn parse_main_key(tok: &str) -> Result<MainKey, String> {
    let b = tok.as_bytes();
    if b.len() == 1 {
        return match b[0] {
            b'a'..=b'z' => Ok(MainKey::Letter(b[0])),
            b'0'..=b'9' => Ok(MainKey::Digit(b[0])),
            _ => Err(format!("无法识别的键名: {tok}")),
        };
    }
    if let Some(n) = tok.strip_prefix('f') {
        if let Ok(n) = n.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Ok(MainKey::Func(n));
            }
        }
    }
    Ok(match tok {
        // 鼠标侧键（浏览器「后退/前进」键）——可作为主键，允许单独使用
        "mouse4" | "xbutton1" | "x1" | "back" => MainKey::Mouse4,
        "mouse5" | "xbutton2" | "x2" | "forward" => MainKey::Mouse5,
        "space" => MainKey::Space,
        "tab" => MainKey::Tab,
        "enter" | "return" => MainKey::Enter,
        "backspace" => MainKey::Backspace,
        "escape" | "esc" => MainKey::Esc,
        "pause" => MainKey::Pause,
        "insert" => MainKey::Insert,
        "delete" | "del" => MainKey::Delete,
        "home" => MainKey::Home,
        "end" => MainKey::End,
        "pageup" => MainKey::PageUp,
        "pagedown" => MainKey::PageDown,
        "up" => MainKey::Up,
        "down" => MainKey::Down,
        "left" => MainKey::Left,
        "right" => MainKey::Right,
        "minus" | "dash" => MainKey::Minus,
        "equal" | "equals" => MainKey::Equal,
        "bracketleft" | "leftbracket" => MainKey::BracketLeft,
        "bracketright" | "rightbracket" => MainKey::BracketRight,
        "backslash" => MainKey::Backslash,
        "semicolon" => MainKey::Semicolon,
        "quote" | "apostrophe" => MainKey::Quote,
        "comma" => MainKey::Comma,
        "period" | "dot" => MainKey::Period,
        "slash" => MainKey::Slash,
        "backquote" | "grave" => MainKey::Backquote,
        _ => return Err(format!("无法识别的键名: {tok}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_modifier_only_combo() {
        let c = parse_combo("lctrl+lshift").unwrap();
        assert!(c.main.is_none());
        assert_eq!(c.mod_count(), 2);
        assert_eq!(c.exact, EX_LCTRL | EX_LSHIFT);
        assert_eq!(c.to_string(), "lctrl+lshift");
    }

    #[test]
    fn parses_mixed_mods_and_main_key() {
        let c = parse_combo("Ctrl + Alt + M").unwrap();
        assert_eq!(c.any, ANY_CTRL | ANY_ALT);
        assert_eq!(c.main, Some(MainKey::Letter(b'm')));
        assert_eq!(c.to_string(), "ctrl+alt+m");
        let c2 = parse_combo("alt+space").unwrap();
        assert_eq!(c2.main, Some(MainKey::Space));
        let c3 = parse_combo("lalt+rwin+a").unwrap();
        assert_eq!(c3.main, Some(MainKey::Letter(b'a')));
        assert_eq!(c3.to_string(), "lalt+rwin+a");
    }

    #[test]
    fn bare_fkey_allowed_bare_letter_rejected() {
        assert!(parse_combo("f13").is_ok());
        assert!(parse_combo("a").is_err());
        assert!(parse_combo("space").is_err());
        assert!(parse_combo("ctrl+a").is_ok());
    }

    #[test]
    fn single_modifier_only_combo_rejected() {
        assert!(parse_combo("lctrl").is_err());
        assert!(parse_combo("shift").is_err());
        assert!(parse_combo("lctrl+rctrl").is_ok());
        assert!(parse_combo("ctrl+shift").is_ok());
    }

    #[test]
    fn invalid_inputs_rejected() {
        assert!(parse_combo("").is_err());
        assert!(parse_combo("  +  ").is_err());
        assert!(parse_combo("ctrl+nope").is_err());
        assert!(parse_combo("ctrl+a+b").is_err());
        assert!(parse_combo("mouse4+mouse5").is_err()); // two main keys
        assert!(parse_combo("wheel").is_err());
        assert!(parse_combo("f25").is_err());
    }

    #[test]
    fn mouse_side_buttons_supported() {
        let m4 = parse_combo("mouse4").unwrap();
        assert_eq!(m4.main, Some(MainKey::Mouse4));
        assert!(m4.needs_mouse());
        assert_eq!(m4.to_string(), "mouse4");
        assert_eq!(parse_combo("xbutton1").unwrap(), m4);
        assert_eq!(parse_combo("back").unwrap(), m4);
        let m5 = parse_combo("mouse5").unwrap();
        assert_eq!(parse_combo("forward").unwrap(), m5);
        let c = parse_combo("ctrl+mouse5").unwrap();
        assert_eq!(c.any, ANY_CTRL);
        assert!(c.needs_mouse() && c.main == Some(MainKey::Mouse5));
        assert!(!parse_combo("lctrl+lshift").unwrap().needs_mouse());
    }

    #[test]
    fn duplicate_tokens_are_idempotent() {
        assert_eq!(parse_combo("lctrl+lctrl+lshift").unwrap().mod_count(), 2);
        assert_eq!(parse_combo("ctrl+ctrl+m").unwrap().mod_count(), 1);
    }

    #[test]
    fn display_roundtrips_through_parser() {
        for src in [
            "lctrl+lshift", "ctrl+alt+m", "f13", "mouse4", "ctrl+mouse5",
            "ctrl+shift+f8", "alt+space", "lalt+rwin+a", "shift+pageup",
            "ctrl+alt+delete",
        ] {
            let c = parse_combo(src).unwrap();
            let rendered = c.to_string();
            assert_eq!(parse_combo(&rendered).unwrap(), c, "roundtrip {src} → {rendered}");
        }
    }

    #[test]
    fn host_string_rendering() {
        assert_eq!(
            parse_combo("ctrl+alt+m").unwrap().to_host_string().as_deref(),
            Some("ctrl+alt+m")
        );
        assert_eq!(
            parse_combo("Ctrl + Shift + F8").unwrap().to_host_string().as_deref(),
            Some("ctrl+shift+f8")
        );
        // Left/right modifiers degrade to generic ones for the host parser.
        assert_eq!(
            parse_combo("lctrl+lalt+x").unwrap().to_host_string().as_deref(),
            Some("ctrl+alt+x")
        );
        // Modifier-only combos and mouse buttons cannot be expressed.
        assert_eq!(parse_combo("lctrl+lshift").unwrap().to_host_string(), None);
        assert_eq!(parse_combo("mouse4").unwrap().to_host_string(), None);
        assert_eq!(parse_combo("ctrl+mouse5").unwrap().to_host_string(), None);
        assert_eq!(
            parse_combo("win+d").unwrap().to_host_string().as_deref(),
            Some("super+d")
        );
        assert_eq!(
            parse_combo("alt+esc").unwrap().to_host_string().as_deref(),
            Some("alt+esc")
        );
        assert_eq!(
            parse_combo("ctrl+space").unwrap().to_host_string().as_deref(),
            Some("ctrl+space")
        );
    }
}
