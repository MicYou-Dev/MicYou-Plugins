//! macOS capture backend — a session-level `CGEventTap` on a dedicated
//! CFRunLoop thread.
//!
//! * The tap sees every keyboard flag change, key down/up and mouse
//!   side-button event system-wide, which is exactly what press **and**
//!   release bindings (PTT/PTM) need — the host's `register_hotkey` only
//!   delivers presses.
//! * An **active** tap (`kCGEventTapOptionDefault`) additionally lets us
//!   swallow the main key of a combo while its modifiers are held (no stray
//!   keystrokes in the foreground app). Modifier-only combos are never
//!   swallowed — that would break ordinary shortcuts system-wide. Creating
//!   any tap requires the *MicYou app* to hold Accessibility permission
//!   (System Settings → Privacy & Security → Accessibility); without it we
//!   try a listen-only tap (full PTT support, no swallowing) and otherwise
//!   report `unavailable` (the core then arms the host `register_hotkey`
//!   fallback for the toggle bindings) and trigger the system permission
//!   prompt once.
//! * macOS may disable a tap that takes too long (`tapDisabledByTimeout`);
//!   the callback re-enables it immediately. All per-event work is a couple
//!   of atomic loads.
//!
//! FFI: raw `extern "C"` against the CoreGraphics / CoreFoundation /
//! ApplicationServices frameworks — no third-party crate pins us to a
//! specific binding generation, and the surface used here is C-ABI-frozen
//! since 10.1.

use super::{read_settings, report_status, KeyState, DETECTOR};
use crate::actions::Slot;
use crate::combo::{
    MainKey, ANY_ALT, ANY_CTRL, ANY_SHIFT, ANY_WIN, EX_LALT, EX_LCTRL, EX_LSHIFT, EX_LWIN,
    EX_RALT, EX_RCTRL, EX_RSHIFT, EX_RWIN,
};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

// ─────────────────────────── raw FFI surface ───────────────────────────

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFStringRef = *const c_void;
type CFTypeRef = *const c_void;
type CFIndex = isize;
type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

/// Opaque callback-struct globals (we only ever pass their addresses).
#[repr(C)]
struct CFDictionaryCallBacks {
    _opaque: [usize; 6],
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFRunLoopRef);
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: CFIndex,
        key_callbacks: *const CFDictionaryCallBacks,
        value_callbacks: *const CFDictionaryCallBacks,
    ) -> *mut c_void;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
    static kCFBooleanTrue: *const c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

// ─────────────────────────── CG constants ───────────────────────────

const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;

const EV_LEFT_MOUSE_DOWN: u32 = 1;
const EV_LEFT_MOUSE_UP: u32 = 2;
const EV_RIGHT_MOUSE_DOWN: u32 = 3;
const EV_RIGHT_MOUSE_UP: u32 = 4;
const EV_KEY_DOWN: u32 = 10;
const EV_KEY_UP: u32 = 11;
const EV_FLAGS_CHANGED: u32 = 12;
const EV_OTHER_MOUSE_DOWN: u32 = 25;
const EV_OTHER_MOUSE_UP: u32 = 26;
const EV_TAP_DISABLED_BY_TIMEOUT: u32 = u32::MAX - 1;
const EV_TAP_DISABLED_BY_USER_INPUT: u32 = u32::MAX;

const FIELD_KEYBOARD_KEYCODE: u32 = 9; // kCGKeyboardEventKeycode
const FIELD_MOUSE_BUTTON_NUMBER: u32 = 3; // kCGMouseEventButtonNumber

// CGEventFlags device-dependent bits (IOKit/hidsystem/IOLLEvent.h).
const NX_DEVICE_LCTL: u64 = 0x0000_0001;
const NX_DEVICE_LSHIFT: u64 = 0x0000_0002;
const NX_DEVICE_RSHIFT: u64 = 0x0000_0004;
const NX_DEVICLE_LCMD: u64 = 0x0000_0008;
const NX_DEVICE_RCMD: u64 = 0x0000_0010;
const NX_DEVICE_LALT: u64 = 0x0000_0020;
const NX_DEVICE_RALT: u64 = 0x0000_0040;
// Device-independent masks (IOLLEvent.h: NX_CONTROLMASK etc.). NOTE:
// NX_DEVICERCTLbit is 0x2000 — do not confuse it with NX_CONTROLMASK.
const NX_CONTROL: u64 = 0x0004_0000;
const NX_SHIFT: u64 = 0x0002_0000;
const NX_ALTERNATE: u64 = 0x0008_0000;
const NX_COMMAND: u64 = 0x0010_0000;

// Apple side-button numbers (0=left, 1=right, 2=center, 3=back, 4=forward).
const MOUSE_BTN_BACK: i64 = 3;
const MOUSE_BTN_FORWARD: i64 = 4;

// ─────────────────────────── neutral key → CGKeyCode ───────────────────────────

/// Main key → macOS ANSI virtual keycode (physical-position based).
/// `None` for keys Apple layouts cannot represent (F21–F24, Pause) — such
/// combos are inert on macOS and reported through the panel status.
pub fn keycode_of_main(key: MainKey) -> Option<u16> {
    Some(match key {
        MainKey::Letter(c) => match c {
            b'a' => 0x00, b's' => 0x01, b'd' => 0x02, b'f' => 0x03, b'h' => 0x04,
            b'g' => 0x05, b'z' => 0x06, b'x' => 0x07, b'c' => 0x08, b'v' => 0x09,
            b'b' => 0x0B, b'q' => 0x0C, b'w' => 0x0D, b'e' => 0x0E, b'r' => 0x0F,
            b'y' => 0x10, b't' => 0x11, b'o' => 0x1F, b'u' => 0x20, b'i' => 0x22,
            b'p' => 0x23, b'l' => 0x25, b'j' => 0x26, b'k' => 0x28, b'n' => 0x2D,
            b'm' => 0x2E,
            _ => return None,
        },
        MainKey::Digit(d) => match d {
            b'1' => 0x12, b'2' => 0x13, b'3' => 0x14, b'4' => 0x15, b'6' => 0x16,
            b'5' => 0x17, b'9' => 0x19, b'7' => 0x1A, b'8' => 0x1C, b'0' => 0x1D,
            _ => return None,
        },
        MainKey::Func(n) => match n {
            1 => 0x7A, 2 => 0x78, 3 => 0x63, 4 => 0x76, 5 => 0x60, 6 => 0x61,
            7 => 0x62, 8 => 0x64, 9 => 0x65, 10 => 0x6D, 11 => 0x67, 12 => 0x6F,
            13 => 0x69, 14 => 0x6B, 15 => 0x71, 16 => 0x6A, 17 => 0x40, 18 => 0x4F,
            19 => 0x50, 20 => 0x5A,
            _ => return None, // F21–F24 have no Apple keycodes
        },
        // Side buttons travel as otherMouse events, not keycodes.
        MainKey::Mouse4 | MainKey::Mouse5 => return None,
        MainKey::Space => 0x31,
        MainKey::Tab => 0x30,
        MainKey::Enter => 0x24,
        MainKey::Backspace => 0x33,
        MainKey::Esc => 0x35,
        MainKey::Delete => 0x75, // forward delete
        MainKey::Insert => 0x72, // Help key acts as Insert on extended layouts
        MainKey::Home => 0x73,
        MainKey::End => 0x77,
        MainKey::PageUp => 0x74,
        MainKey::PageDown => 0x79,
        MainKey::Up => 0x7E,
        MainKey::Down => 0x7D,
        MainKey::Left => 0x7B,
        MainKey::Right => 0x7C,
        MainKey::Minus => 0x1B,
        MainKey::Equal => 0x18,
        MainKey::BracketLeft => 0x21,
        MainKey::BracketRight => 0x1E,
        MainKey::Backslash => 0x2A,
        MainKey::Semicolon => 0x29,
        MainKey::Quote => 0x27,
        MainKey::Comma => 0x2B,
        MainKey::Period => 0x2F,
        MainKey::Slash => 0x2C,
        MainKey::Backquote => 0x32,
        MainKey::Pause => return None, // no Apple keycode
    })
}

pub fn main_key_supported(key: MainKey) -> bool {
    matches!(key, MainKey::Mouse4 | MainKey::Mouse5) || keycode_of_main(key).is_some()
}

// ─────────────────────────── shared state ───────────────────────────

/// Latest CGEventFlags snapshot (updated on every tapped event).
static FLAGS: AtomicU64 = AtomicU64::new(0);
/// Physical keycodes 0..128 currently held (keyDown sets, keyUp clears).
static KEYBITS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Mouse side buttons held: bit0 = back (mouse4), bit1 = forward (mouse5).
static MOUSEBITS: AtomicU8 = AtomicU8::new(0);
/// Per-slot latch: a swallowed main-key press awaits its swallowed release.
static SWALLOWED: [AtomicBool; 4] = [const { AtomicBool::new(false) }; 4];

static TAP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static RUNLOOP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static LISTEN_ONLY: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn set_keybit(kc: u16, down: bool) {
    if kc >= 128 {
        return;
    }
    let (word, bit) = ((kc / 64) as usize, kc % 64);
    if down {
        KEYBITS[word].fetch_or(1 << bit, Ordering::SeqCst);
    } else {
        KEYBITS[word].fetch_and(!(1 << bit), Ordering::SeqCst);
    }
}

fn keybit(kc: u16) -> bool {
    if kc >= 128 {
        return false;
    }
    KEYBITS[(kc / 64) as usize].load(Ordering::SeqCst) & (1 << (kc % 64)) != 0
}

fn set_mousebit(btn: i64, down: bool) {
    let bit = match btn {
        MOUSE_BTN_BACK => 1u8,
        MOUSE_BTN_FORWARD => 2u8,
        _ => return,
    };
    if down {
        MOUSEBITS.fetch_or(bit, Ordering::SeqCst);
    } else {
        MOUSEBITS.fetch_and(!bit, Ordering::SeqCst);
    }
}

/// [`KeyState`] over the atomics above — read from the tap callback thread.
struct TapState;

impl KeyState for TapState {
    fn exact_mod(&self, bit: u8) -> bool {
        let f = FLAGS.load(Ordering::SeqCst);
        match bit {
            EX_LCTRL => f & NX_DEVICE_LCTL != 0,
            // IOLLEvent has no device bit for right Control: "ctrl held but
            // not left ctrl" is right ctrl.
            EX_RCTRL => f & NX_CONTROL != 0 && f & NX_DEVICE_LCTL == 0,
            EX_LALT => f & NX_DEVICE_LALT != 0,
            EX_RALT => f & NX_DEVICE_RALT != 0,
            EX_LSHIFT => f & NX_DEVICE_LSHIFT != 0,
            EX_RSHIFT => f & NX_DEVICE_RSHIFT != 0,
            EX_LWIN => f & NX_DEVICLE_LCMD != 0,
            EX_RWIN => f & NX_DEVICE_RCMD != 0,
            _ => false,
        }
    }
    fn any_mod(&self, bit: u8) -> bool {
        let f = FLAGS.load(Ordering::SeqCst);
        match bit {
            ANY_CTRL => f & NX_CONTROL != 0,
            ANY_ALT => f & NX_ALTERNATE != 0,
            ANY_SHIFT => f & NX_SHIFT != 0,
            ANY_WIN => f & NX_COMMAND != 0,
            _ => false,
        }
    }
    fn main(&self, key: MainKey) -> Option<bool> {
        match key {
            MainKey::Mouse4 => Some(MOUSEBITS.load(Ordering::SeqCst) & 1 != 0),
            MainKey::Mouse5 => Some(MOUSEBITS.load(Ordering::SeqCst) & 2 != 0),
            other => keycode_of_main(other).map(keybit),
        }
    }
}

/// Modifiers-only check (swallow gate for a main-key event).
fn mods_down(combo: &crate::combo::Combo) -> bool {
    let mod_only = crate::combo::Combo {
        main: None,
        ..*combo
    };
    super::combo_is_down(&mod_only, &TapState)
}

// ─────────────────────────── tap callback ───────────────────────────

/// Panics must never unwind into CoreGraphics — contain them and pass the
/// event through untouched (a swallowed key on panic could stick the
/// user's keyboard).
unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    _user_info: *mut c_void,
) -> CGEventRef {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle_tap(etype, event))) {
        Ok(Decision::Swallow) => null_mut(),
        Ok(Decision::Pass) | Err(_) => event,
    }
}

enum Decision {
    Pass,
    Swallow,
}

fn handle_tap(etype: u32, event: CGEventRef) -> Decision {
    // macOS disabled our tap (we were too slow, or the user toggled it off):
    // re-enable and move on.
    if etype == EV_TAP_DISABLED_BY_TIMEOUT || etype == EV_TAP_DISABLED_BY_USER_INPUT {
        let tap = TAP.load(Ordering::SeqCst);
        if !tap.is_null() {
            unsafe { CGEventTapEnable(tap, true) };
        }
        return Decision::Pass;
    }

    let flags = unsafe { CGEventGetFlags(event) };
    FLAGS.store(flags, Ordering::SeqCst);

    match etype {
        EV_KEY_DOWN | EV_KEY_UP => {
            let kc = unsafe { CGEventGetIntegerValueField(event, FIELD_KEYBOARD_KEYCODE) } as u16;
            set_keybit(kc, etype == EV_KEY_DOWN);
            handle_main_event(kc, None, etype == EV_KEY_DOWN)
        }
        EV_FLAGS_CHANGED => {
            // Modifier transition: poll every slot (modifier-only combos
            // press/release here; main-key combos converge when a modifier
            // is released first). Never swallow — swallowing a modifier
            // would break every shortcut system-wide.
            let settings = read_settings();
            if settings.enabled {
                for slot in Slot::ALL {
                    match settings.combos[slot.index()] {
                        Some(combo) => {
                            DETECTOR.update(slot, super::combo_is_down(&combo, &TapState))
                        }
                        None => DETECTOR.update(slot, false),
                    }
                }
            }
            Decision::Pass
        }
        EV_OTHER_MOUSE_DOWN | EV_OTHER_MOUSE_UP => {
            let btn = unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_BUTTON_NUMBER) };
            if btn != MOUSE_BTN_BACK && btn != MOUSE_BTN_FORWARD {
                return Decision::Pass;
            }
            set_mousebit(btn, etype == EV_OTHER_MOUSE_DOWN);
            handle_main_event(0, Some(btn), etype == EV_OTHER_MOUSE_DOWN)
        }
        // Left/right/middle button + movement events only refresh FLAGS.
        EV_LEFT_MOUSE_DOWN | EV_LEFT_MOUSE_UP | EV_RIGHT_MOUSE_DOWN | EV_RIGHT_MOUSE_UP => {
            Decision::Pass
        }
        _ => Decision::Pass,
    }
}

/// Shared press-edge handling for events on a combo's *main key* (keyboard
/// `kc` or mouse side button `btn`), mirroring the Windows hook semantics:
/// swallow the event pair while the combo's modifiers are held, and poll all
/// other slots for convergence.
fn handle_main_event(kc: u16, btn: Option<i64>, is_down: bool) -> Decision {
    let settings = read_settings();
    let mut swallow = false;
    if !settings.enabled {
        DETECTOR.reset();
        return Decision::Pass;
    }
    for slot in Slot::ALL {
        let Some(combo) = settings.combos[slot.index()] else {
            DETECTOR.update(slot, false);
            continue;
        };
        let matches_event = match (combo.main, btn) {
            (Some(MainKey::Mouse4), Some(b)) => b == MOUSE_BTN_BACK,
            (Some(MainKey::Mouse5), Some(b)) => b == MOUSE_BTN_FORWARD,
            (Some(m), None) => keycode_of_main(m) == Some(kc),
            _ => false,
        };
        if matches_event && is_down && mods_down(&combo) {
            SWALLOWED[slot.index()].store(true, Ordering::SeqCst);
            DETECTOR.update(slot, true);
            swallow = true;
            continue;
        }
        if matches_event && !is_down && SWALLOWED[slot.index()].swap(false, Ordering::SeqCst) {
            DETECTOR.update(slot, false);
            swallow = true;
            continue;
        }
        DETECTOR.update(slot, super::combo_is_down(&combo, &TapState));
    }
    // A listen-only tap cannot suppress events; the hotkey keystroke also
    // reaches the foreground app in that mode (documented limitation).
    if swallow && !LISTEN_ONLY.load(Ordering::SeqCst) {
        Decision::Swallow
    } else {
        Decision::Pass
    }
}

// ─────────────────────────── lifecycle ───────────────────────────

fn event_mask() -> u64 {
    (1u64 << EV_KEY_DOWN)
        | (1u64 << EV_KEY_UP)
        | (1u64 << EV_FLAGS_CHANGED)
        | (1u64 << EV_OTHER_MOUSE_DOWN)
        | (1u64 << EV_OTHER_MOUSE_UP)
}

/// Ask macOS to show the "grant Accessibility access" prompt (once per
/// failed start). Best-effort: any FFI hiccup degrades to the plain
/// AXIsProcessTrusted query.
fn prompt_accessibility() {
    let trusted = unsafe {
        let key: CFStringRef = kAXTrustedCheckOptionPrompt;
        let value = kCFBooleanTrue;
        let keys = [key as *const c_void];
        let values = [value as *const c_void];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if dict.is_null() {
            AXIsProcessTrusted()
        } else {
            let trusted = AXIsProcessTrustedWithOptions(dict);
            CFRelease(dict);
            trusted
        }
    };
    if !trusted {
        crate::abi::log_warn(
            "Accessibility permission missing — open System Settings → Privacy & Security → Accessibility and enable MicYou, then re-enable this plugin",
        );
    }
}

pub fn start() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    report_status("starting", "creating CGEventTap");
    let handle = std::thread::spawn(|| {
        unsafe {
            // Active tap first (can swallow); listen-only as a degraded
            // second chance; both require Accessibility trust, so a double
            // failure means "no permission".
            let mut tap = CGEventTapCreate(
                K_CG_SESSION_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_DEFAULT,
                event_mask(),
                tap_callback,
                null_mut(),
            );
            let mut listen = false;
            if tap.is_null() {
                tap = CGEventTapCreate(
                    K_CG_SESSION_EVENT_TAP,
                    K_CG_HEAD_INSERT_EVENT_TAP,
                    K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                    event_mask(),
                    tap_callback,
                    null_mut(),
                );
                listen = true;
            }
            if tap.is_null() {
                prompt_accessibility();
                report_status(
                    "unavailable",
                    "CGEventTap unavailable — grant Accessibility permission to MicYou (System Settings → Privacy & Security → Accessibility), then restart MicYou",
                );
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            LISTEN_ONLY.store(listen, Ordering::SeqCst);
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            if source.is_null() {
                CFRelease(tap);
                report_status("unavailable", "CFMachPortCreateRunLoopSource failed");
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            let rl = CFRunLoopGetCurrent();
            CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            TAP.store(tap, Ordering::SeqCst);
            RUNLOOP.store(rl, Ordering::SeqCst);
            report_status(
                if listen { "macos-tap-listen" } else { "macos-tap" },
                if listen {
                    "listen-only event tap (hotkeys work; keystrokes are not swallowed)"
                } else {
                    "active event tap installed"
                },
            );

            CFRunLoopRun();

            // stop() disabled the tap and stopped the run loop.
            CGEventTapEnable(tap, false);
            CFRelease(source);
            CFRelease(tap);
            TAP.store(null_mut(), Ordering::SeqCst);
            RUNLOOP.store(null_mut(), Ordering::SeqCst);
            RUNNING.store(false, Ordering::SeqCst);
        }
    });
    if let Ok(mut slot) = THREAD.lock() {
        *slot = Some(handle);
    }
}

pub fn on_settings_updated() {
    for s in SWALLOWED.iter() {
        s.store(false, Ordering::SeqCst);
    }
}

/// Disable the tap, stop the run loop and **join** the thread — mandatory
/// before the cdylib is unloaded (a live tap callback into unmapped memory
/// would crash the whole session).
pub fn stop() {
    for s in SWALLOWED.iter() {
        s.store(false, Ordering::SeqCst);
    }
    // Bounded wait for the thread to publish its run loop (start races).
    for _ in 0..200 {
        let rl = RUNLOOP.load(Ordering::SeqCst);
        if !rl.is_null() {
            unsafe {
                let tap = TAP.load(Ordering::SeqCst);
                if !tap.is_null() {
                    CGEventTapEnable(tap, false);
                }
                CFRunLoopStop(rl);
            }
            break;
        }
        if !RUNNING.load(Ordering::SeqCst) {
            break; // thread already gave up (no permission)
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if let Ok(mut slot) = THREAD.lock() {
        if let Some(h) = slot.take() {
            let _ = h.join();
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
}
