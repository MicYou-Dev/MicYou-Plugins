//! Linux capture backend — raw evdev (`/dev/input/event*`).
//!
//! Why evdev instead of X11 grabs: the host's `register_hotkey` (and any
//! XGrabKey-based approach) only works on X11 — Wayland compositors do not
//! forward global key grabs. The kernel input layer, in contrast, is
//! session-agnostic: one code path serves **X11 and Wayland alike**, and it
//! delivers press *and* release events, which PTT/PTM require.
//!
//! Permissions: reading `/dev/input/event*` needs either
//! * an active local logind session (udev `uaccess` ACLs grant the session
//!   user read access — the normal desktop case, works out of the box), or
//! * membership in the `input` group (SSH sessions, unusual setups).
//! When no device is readable the backend reports `unavailable` with
//! guidance; the core then arms the host `register_hotkey` fallback for the
//! toggle bindings (X11 only).
//!
//! Limitation: a raw evdev tap is **observe-only** — the hotkey keystroke is
//! also delivered to the focused application (swallowing would require an
//! exclusive EVIOCGRAB + uinput re-injection pipeline, which is far too
//! invasive for a mic-control plugin). The README recommends side buttons /
//! F-keys / modifier-only combos on Linux.
//!
//! Implementation notes:
//! * key state lives in a lock-free `[AtomicU64; 12]` bitset (KEY_CNT=768);
//! * hot-plug: the poll loop wakes every 2 s and rescans `/dev/input` for
//!   new devices / drops unplugged ones;
//! * `libc::poll` is used directly (instead of evdev's poll helper) to keep
//!   the crate-API surface we depend on minimal.

use super::{report_status, KeyState, DETECTOR};
use crate::combo::{
    MainKey, ANY_ALT, ANY_CTRL, ANY_SHIFT, ANY_WIN, EX_LALT, EX_LCTRL, EX_LSHIFT, EX_LWIN,
    EX_RALT, EX_RCTRL, EX_RSHIFT, EX_RWIN,
};
use evdev::{Device, EventType};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

// ─────────────────────────── input-event-codes (stable uapi ABI) ───────────────────────────

const KEY_ESC: u16 = 1;
const KEY_GRAVE: u16 = 41;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const KEY_SPACE: u16 = 57;
#[allow(dead_code)] // pinned in tests
const KEY_CAPSLOCK: u16 = 58;
const KEY_PAUSE: u16 = 119;
const KEY_INSERT: u16 = 110;
const KEY_HOME: u16 = 102;
const KEY_PAGEUP: u16 = 104;
const KEY_DELETE: u16 = 111;
const KEY_END: u16 = 107;
const KEY_PAGEDOWN: u16 = 109;
const KEY_UP: u16 = 103;
const KEY_DOWN: u16 = 108;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_MINUS: u16 = 12;
const KEY_EQUAL: u16 = 13;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_BACKSLASH: u16 = 43;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_SLASH: u16 = 53;
const BTN_SIDE: u16 = 275; // mouse "back" side button  = mouse4
const BTN_EXTRA: u16 = 276; // mouse "forward" side button = mouse5

/// KEY_* codes for a-z: KEY_A=30 in QWERTY physical order (a,s,d,f,g,h,j,k,l,
/// z,x,c,v,b,n,m) — see linux/input-event-codes.h.
const fn letter_code(c: u8) -> Option<u16> {
    Some(match c {
        b'a' => 30, b's' => 31, b'd' => 32, b'f' => 33, b'g' => 34,
        b'h' => 35, b'j' => 36, b'k' => 37, b'l' => 38,
        b'z' => 44, b'x' => 45, b'c' => 46, b'v' => 47, b'b' => 48,
        b'n' => 49, b'm' => 50,
        b'q' => 16, b'w' => 17, b'e' => 18, b'r' => 19, b't' => 20,
        b'y' => 21, b'u' => 22, b'i' => 23, b'o' => 24, b'p' => 25,
        _ => return None,
    })
}

/// KEY_1..KEY_9 = 2..10, KEY_0 = 11.
const fn digit_code(d: u8) -> Option<u16> {
    Some(match d {
        b'1'..=b'9' => (d - b'1') as u16 + 2,
        b'0' => 11,
        _ => return None,
    })
}

/// KEY_F1..KEY_F10 = 59..68; F11=87, F12=88; KEY_F13..KEY_F24 = 183..194.
const fn func_code(n: u8) -> Option<u16> {
    Some(match n {
        1..=10 => 58 + n as u16,
        11 => 87,
        12 => 88,
        13..=24 => 170 + n as u16,
        _ => return None,
    })
}

/// Neutral main key → evdev code. Every key the parser accepts exists in the
/// Linux input ABI, so this is total.
pub fn evdev_code_of_main(key: MainKey) -> Option<u16> {
    Some(match key {
        MainKey::Letter(c) => letter_code(c)?,
        MainKey::Digit(d) => digit_code(d)?,
        MainKey::Func(n) => func_code(n)?,
        MainKey::Mouse4 => BTN_SIDE,
        MainKey::Mouse5 => BTN_EXTRA,
        MainKey::Space => KEY_SPACE,
        MainKey::Tab => KEY_TAB,
        MainKey::Enter => KEY_ENTER,
        MainKey::Backspace => KEY_BACKSPACE,
        MainKey::Esc => KEY_ESC,
        MainKey::Pause => KEY_PAUSE,
        MainKey::Insert => KEY_INSERT,
        MainKey::Delete => KEY_DELETE,
        MainKey::Home => KEY_HOME,
        MainKey::End => KEY_END,
        MainKey::PageUp => KEY_PAGEUP,
        MainKey::PageDown => KEY_PAGEDOWN,
        MainKey::Up => KEY_UP,
        MainKey::Down => KEY_DOWN,
        MainKey::Left => KEY_LEFT,
        MainKey::Right => KEY_RIGHT,
        MainKey::Minus => KEY_MINUS,
        MainKey::Equal => KEY_EQUAL,
        MainKey::BracketLeft => KEY_LEFTBRACE,
        MainKey::BracketRight => KEY_RIGHTBRACE,
        MainKey::Backslash => KEY_BACKSLASH,
        MainKey::Semicolon => KEY_SEMICOLON,
        MainKey::Quote => KEY_APOSTROPHE,
        MainKey::Comma => KEY_COMMA,
        MainKey::Period => KEY_DOT,
        MainKey::Slash => KEY_SLASH,
        MainKey::Backquote => KEY_GRAVE,
    })
}

pub fn main_key_supported(key: MainKey) -> bool {
    evdev_code_of_main(key).is_some()
}

// ─────────────────────────── key state ───────────────────────────

/// KEY_CNT = 0x2ff+1 = 768 codes → 12 words.
const KEY_WORDS: usize = 12;
static KEYBITS: [AtomicU64; KEY_WORDS] = [const { AtomicU64::new(0) }; KEY_WORDS];

fn set_keybit(code: u16, down: bool) {
    let code = code as usize;
    if code >= KEY_WORDS * 64 {
        return;
    }
    let (word, bit) = (code / 64, code % 64);
    if down {
        KEYBITS[word].fetch_or(1u64 << bit, Ordering::SeqCst);
    } else {
        KEYBITS[word].fetch_and(!(1u64 << bit), Ordering::SeqCst);
    }
}

fn keybit(code: u16) -> bool {
    let code = code as usize;
    if code >= KEY_WORDS * 64 {
        return false;
    }
    KEYBITS[code / 64].load(Ordering::SeqCst) & (1u64 << (code % 64)) != 0
}

struct EvdevState;

impl KeyState for EvdevState {
    fn exact_mod(&self, bit: u8) -> bool {
        keybit(match bit {
            EX_LCTRL => KEY_LEFTCTRL,
            EX_RCTRL => KEY_RIGHTCTRL,
            EX_LALT => KEY_LEFTALT,
            EX_RALT => KEY_RIGHTALT,
            EX_LSHIFT => KEY_LEFTSHIFT,
            EX_RSHIFT => KEY_RIGHTSHIFT,
            EX_LWIN => KEY_LEFTMETA,
            EX_RWIN => KEY_RIGHTMETA,
            _ => return false,
        })
    }
    fn any_mod(&self, bit: u8) -> bool {
        match bit {
            ANY_CTRL => keybit(KEY_LEFTCTRL) || keybit(KEY_RIGHTCTRL),
            ANY_ALT => keybit(KEY_LEFTALT) || keybit(KEY_RIGHTALT),
            ANY_SHIFT => keybit(KEY_LEFTSHIFT) || keybit(KEY_RIGHTSHIFT),
            ANY_WIN => keybit(KEY_LEFTMETA) || keybit(KEY_RIGHTMETA),
            _ => false,
        }
    }
    fn main(&self, key: MainKey) -> Option<bool> {
        evdev_code_of_main(key).map(keybit)
    }
}

// ─────────────────────────── capture loop ───────────────────────────

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Short poll timeout keeps `stop()` snappy; hot-plug rescans are throttled
/// to every eighth timeout (~2 s).
const POLL_TIMEOUT_MS: i32 = 250;
const RESCAN_EVERY: u32 = 8;

struct Dev {
    path: PathBuf,
    device: Device,
}

/// Open every readable `/dev/input/event*` that reports EV_KEY. Returns the
/// opened devices plus the count of permission-denied opens (for guidance).
fn scan_devices(existing: &[PathBuf]) -> (Vec<Dev>, usize) {
    let mut out = Vec::new();
    let mut denied = 0usize;
    let Ok(rd) = std::fs::read_dir("/dev/input") else {
        return (out, denied);
    };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    paths.sort();
    for path in paths {
        if existing.contains(&path) {
            continue;
        }
        match Device::open(&path) {
            Ok(dev) => {
                if dev.supported_events().contains(EventType::KEY) {
                    // Seed the currently-held keys (e.g. a modifier already
                    // down when the plugin starts) so combos are consistent
                    // from the first event. Best effort.
                    if let Ok(state) = dev.get_key_state() {
                        for key in state.iter() {
                            set_keybit(key.code(), true);
                        }
                    }
                    out.push(Dev { path, device: dev });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied += 1,
            Err(_) => {} // busy / vanished — ignore
        }
    }
    (out, denied)
}

fn describe_state(devices: &[Dev]) -> String {
    if devices.is_empty() {
        "no readable /dev/input/event* device (X11 & Wayland both need evdev access)".to_string()
    } else {
        let names: Vec<String> = devices
            .iter()
            .filter_map(|d| d.device.name())
            .take(6)
            .map(|n| n.to_string())
            .collect();
        format!("capturing {} input device(s): {}", devices.len(), names.join(", "))
    }
}

fn report_availability(devices: &[Dev], denied: usize) {
    if !devices.is_empty() {
        report_status("linux-evdev", &describe_state(devices));
    } else if denied > 0 {
        report_status(
            "unavailable",
            &format!(
                "permission denied on {denied} /dev/input/event* node(s) — \
                 add your user to the `input` group (or log into a local \
                 graphical session so logind uaccess ACLs apply), then \
                 re-enable this plugin"
            ),
        );
    } else {
        report_status("unavailable", &describe_state(devices));
    }
}

fn capture_loop() {
    let mut devices: Vec<Dev> = Vec::new();
    let mut idle_rounds = 0u32;

    let (mut found, denied) = scan_devices(&[]);
    let mut denied_total = denied;
    devices.append(&mut found);
    report_availability(&devices, denied_total);

    while !STOP.load(Ordering::SeqCst) {
        if devices.is_empty() {
            // Nothing to poll — wait a bit, then hot-plug rescan (a later
            // permission fix or plugged-in keyboard should be picked up).
            std::thread::sleep(std::time::Duration::from_millis(250));
            let (mut found, denied) = scan_devices(&[]);
            if !found.is_empty() || denied != denied_total {
                denied_total = denied;
                devices.append(&mut found);
                report_availability(&devices, denied_total);
            }
            continue;
        }

        // poll(2) over every open device fd.
        let mut fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|d| libc::pollfd {
                fd: d.device.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: fds points at our own vec for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_TIMEOUT_MS) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            report_status("unavailable", &format!("poll failed: {e}"));
            break;
        }
        if n == 0 {
            // Timeout: throttled hot-plug rescan (new keyboards/mice).
            idle_rounds += 1;
            if idle_rounds % RESCAN_EVERY == 0 {
                let paths: Vec<PathBuf> = devices.iter().map(|d| d.path.clone()).collect();
                let (mut found, _denied) = scan_devices(&paths);
                if !found.is_empty() {
                    devices.append(&mut found);
                    report_availability(&devices, 0);
                }
            }
            continue;
        }
        idle_rounds = 0;

        let mut dirty = false;
        let mut dead: Vec<usize> = Vec::new();
        for (i, fd) in fds.iter().enumerate() {
            if fd.revents & libc::POLLIN == 0 {
                if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    dead.push(i);
                }
                continue;
            }
            // Clone the path for diagnostics: fetch_events() holds a mutable
            // borrow of the device across the whole match.
            let path = devices[i].path.clone();
            match devices[i].device.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        if ev.event_type() != EventType::KEY {
                            continue;
                        }
                        let v = ev.value();
                        if v == 2 {
                            continue; // auto-repeat: state unchanged
                        }
                        set_keybit(ev.code(), v == 1);
                        dirty = true;
                    }
                }
                Err(e) => {
                    // Device unplugged (ENODEV) or access revoked: drop it.
                    crate::abi::log_debug(&format!("evdev fetch failed on {}: {e}", path.display()));
                    dead.push(i);
                }
            }
        }
        if dirty {
            super::evaluate_all(&EvdevState);
        }
        if !dead.is_empty() {
            dead.sort_unstable();
            dead.dedup();
            for i in dead.into_iter().rev() {
                devices.remove(i);
            }
            report_availability(&devices, denied_total);
        }
    }

    // Leaving: force every latch low so a re-enable starts clean.
    DETECTOR.reset();
    RUNNING.store(false, Ordering::SeqCst);
}

// ─────────────────────────── lifecycle ───────────────────────────

pub fn start() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    STOP.store(false, Ordering::SeqCst);
    report_status("starting", "opening /dev/input/event*");
    let handle = std::thread::Builder::new()
        .name("hotkey-evdev".into())
        .spawn(capture_loop)
        .expect("spawn evdev capture thread");
    if let Ok(mut slot) = THREAD.lock() {
        *slot = Some(handle);
    }
}

pub fn on_settings_updated() {
    // Nothing backend-specific: the shared layer reset the detector and the
    // loop re-reads settings on every event. Force one evaluation so a
    // still-held combo under the *new* settings presses immediately
    // (e.g. user was holding keys while editing the binding).
    super::evaluate_all(&EvdevState);
}

/// Signal the loop and join — must complete before the cdylib is unloaded.
pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
    if let Ok(mut slot) = THREAD.lock() {
        if let Some(h) = slot.take() {
            let _ = h.join();
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_code_table_spot_checks() {
        // input-event-codes.h pins — a wrong table silently breaks every
        // binding, so check the anchors.
        assert_eq!(KEY_ESC, 1);
        assert_eq!(KEY_CAPSLOCK, 58);
        assert_eq!(BTN_SIDE, 275);
        assert_eq!(BTN_EXTRA, 276);
        assert_eq!(letter_code(b'a'), Some(30));
        assert_eq!(letter_code(b'p'), Some(25));
        assert_eq!(letter_code(b'z'), Some(44));
        assert_eq!(letter_code(b'm'), Some(50));
        assert_eq!(digit_code(b'0'), Some(11));
        assert_eq!(digit_code(b'1'), Some(2));
        assert_eq!(digit_code(b'9'), Some(10));
        assert_eq!(func_code(1), Some(59));
        assert_eq!(func_code(10), Some(68));
        assert_eq!(func_code(11), Some(87));
        assert_eq!(func_code(12), Some(88));
        assert_eq!(func_code(13), Some(183));
        assert_eq!(func_code(24), Some(194));
        assert_eq!(func_code(25), None);
    }

    #[test]
    fn every_main_key_maps_to_a_code() {
        // The Linux input ABI covers the full parser vocabulary.
        for k in [
            MainKey::Letter(b'q'),
            MainKey::Digit(b'7'),
            MainKey::Func(24),
            MainKey::Mouse4,
            MainKey::Mouse5,
            MainKey::Space,
            MainKey::Enter,
            MainKey::Tab,
            MainKey::Esc,
            MainKey::Backspace,
            MainKey::Delete,
            MainKey::Insert,
            MainKey::Home,
            MainKey::End,
            MainKey::PageUp,
            MainKey::PageDown,
            MainKey::Up,
            MainKey::Down,
            MainKey::Left,
            MainKey::Right,
            MainKey::Minus,
            MainKey::Equal,
            MainKey::BracketLeft,
            MainKey::BracketRight,
            MainKey::Backslash,
            MainKey::Semicolon,
            MainKey::Quote,
            MainKey::Comma,
            MainKey::Period,
            MainKey::Slash,
            MainKey::Backquote,
            MainKey::Pause,
        ] {
            assert!(evdev_code_of_main(k).is_some(), "{k:?} must map");
            assert!(main_key_supported(k));
        }
    }

    #[test]
    fn keybits_roundtrip() {
        set_keybit(KEY_LEFTCTRL, true);
        assert!(keybit(KEY_LEFTCTRL));
        assert!(!keybit(KEY_RIGHTCTRL));
        set_keybit(KEY_LEFTCTRL, false);
        assert!(!keybit(KEY_LEFTCTRL));
        // Out-of-range codes must not panic.
        set_keybit(u16::MAX, true);
        assert!(!keybit(u16::MAX));
    }

    #[test]
    fn modifier_queries_use_distinct_codes() {
        set_keybit(KEY_LEFTSHIFT, true);
        let st = EvdevState;
        assert!(st.exact_mod(EX_LSHIFT));
        assert!(!st.exact_mod(EX_RSHIFT));
        assert!(st.any_mod(ANY_SHIFT));
        assert!(!st.any_mod(ANY_CTRL));
        set_keybit(KEY_LEFTSHIFT, false);
    }
}
