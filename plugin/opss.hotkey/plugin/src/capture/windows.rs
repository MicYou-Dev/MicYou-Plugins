//! Windows capture backend — `WH_KEYBOARD_LL` (+ on-demand `WH_MOUSE_LL`).
//!
//! Port of the Voice-Typing hook engine, generalised from one combo to the
//! four binding slots and from Win32-specific combos to the neutral
//! [`crate::combo::Combo`]:
//!
//! * The hook callback polls `GetAsyncKeyState` on every input event and
//!   edge-triggers on each combo's down/up transition (immune to missed
//!   events and auto-repeat). `GetAsyncKeyState` also reports the mouse side
//!   buttons (VK_XBUTTON1/2), so cross-device combos like `ctrl+mouse4` stay
//!   consistent from both hooks.
//! * When a combo contains a main key, that key's own down/up events are
//!   swallowed (hook returns 1) while the combo's modifiers are held, so the
//!   foreground app never sees the hotkey (no stray keystrokes, no browser
//!   back/forward on mouse4/5).
//! * Modifier-only combos are NEVER swallowed — that would break ordinary
//!   shortcuts like Ctrl+C for the whole system.
//! * The mouse hook is only installed while an active combo actually uses a
//!   mouse button; `on_settings_updated` re-evaluates it on the pump thread
//!   via a WM_APP post — hooks must be installed/uninstalled from the thread
//!   whose message loop will dispatch them.
//! * Note (UIPI): if the foreground application runs elevated while MicYou
//!   does not, Windows hides its input from our hooks — the README tells
//!   users to run MicYou as administrator in that case.

use super::{combo_is_down, read_settings, report_status, KeyState, DETECTOR};
use crate::actions::Slot;
use crate::combo::{
    MainKey, ANY_ALT, ANY_CTRL, ANY_SHIFT, ANY_WIN, EX_LALT, EX_LCTRL, EX_LSHIFT, EX_LWIN,
    EX_RALT, EX_RCTRL, EX_RSHIFT, EX_RWIN,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Private WM_APP message: "re-evaluate whether WH_MOUSE_LL is needed".
const WM_APP_REHOOK: u32 = WM_APP + 1;

// Virtual-key codes not (all) exported as constants by the windows crate.
const VK_XBUTTON1: u32 = 0x05;
const VK_XBUTTON2: u32 = 0x06;

static HOOK_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
static HOOK_TID: AtomicU32 = AtomicU32::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Per-slot latch: a swallowed main-key press is pending its (also swallowed)
/// release. Kept until the main key's up event so down/up stay paired for the
/// foreground app even if the modifiers are released first.
static SWALLOWED: [AtomicBool; 4] = [const { AtomicBool::new(false) }; 4];

// ─────────────────────────── neutral combo → VK ───────────────────────────

/// Main key → Win32 virtual-key code (US-layout VK semantics; physical
/// position based, matching macOS keycodes and evdev codes).
pub fn vk_of_main(key: MainKey) -> Option<u32> {
    Some(match key {
        MainKey::Letter(c) => 0x41 + u32::from(c - b'a'),
        MainKey::Digit(d) => 0x30 + u32::from(d - b'0'),
        MainKey::Func(n) => 0x70 + u32::from(n - 1), // VK_F1..VK_F24
        MainKey::Mouse4 => VK_XBUTTON1,
        MainKey::Mouse5 => VK_XBUTTON2,
        MainKey::Space => 0x20,
        MainKey::Tab => 0x09,
        MainKey::Enter => 0x0D,
        MainKey::Backspace => 0x08,
        MainKey::Esc => 0x1B,
        MainKey::Pause => 0x13,
        MainKey::Insert => 0x2D,
        MainKey::Delete => 0x2E,
        MainKey::Home => 0x24,
        MainKey::End => 0x23,
        MainKey::PageUp => 0x21,
        MainKey::PageDown => 0x22,
        MainKey::Up => 0x26,
        MainKey::Down => 0x28,
        MainKey::Left => 0x25,
        MainKey::Right => 0x27,
        MainKey::Minus => 0xBD,
        MainKey::Equal => 0xBB,
        MainKey::BracketLeft => 0xDB,
        MainKey::BracketRight => 0xDD,
        MainKey::Backslash => 0xDC,
        MainKey::Semicolon => 0xBA,
        MainKey::Quote => 0xDE,
        MainKey::Comma => 0xBC,
        MainKey::Period => 0xBE,
        MainKey::Slash => 0xBF,
        MainKey::Backquote => 0xC0,
    })
}

fn exact_mod_vk(bit: u8) -> u16 {
    match bit {
        EX_LCTRL => VK_LCONTROL.0,
        EX_RCTRL => VK_RCONTROL.0,
        EX_LALT => VK_LMENU.0,
        EX_RALT => VK_RMENU.0,
        EX_LSHIFT => VK_LSHIFT.0,
        EX_RSHIFT => VK_RSHIFT.0,
        EX_LWIN => VK_LWIN.0,
        EX_RWIN => VK_RWIN.0,
        _ => 0,
    }
}

/// `GetAsyncKeyState`-backed [`KeyState`] — stateless, safe from any thread.
struct AsyncState;

impl KeyState for AsyncState {
    fn exact_mod(&self, bit: u8) -> bool {
        key_down(exact_mod_vk(bit))
    }
    fn any_mod(&self, bit: u8) -> bool {
        match bit {
            ANY_CTRL => key_down(VK_LCONTROL.0) || key_down(VK_RCONTROL.0),
            ANY_ALT => key_down(VK_LMENU.0) || key_down(VK_RMENU.0),
            ANY_SHIFT => key_down(VK_LSHIFT.0) || key_down(VK_RSHIFT.0),
            ANY_WIN => key_down(VK_LWIN.0) || key_down(VK_RWIN.0),
            _ => false,
        }
    }
    fn main(&self, key: MainKey) -> Option<bool> {
        vk_of_main(key).map(|vk| key_down(vk as u16))
    }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState is a pure state query, callable from any
    // thread at any time.
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// Are just the modifiers of `combo` currently held? (Swallow gate for the
/// main key's own down event.)
fn mods_down(combo: &crate::combo::Combo) -> bool {
    let st = AsyncState;
    use crate::combo::*;
    for bit in [
        EX_LCTRL, EX_RCTRL, EX_LALT, EX_RALT, EX_LSHIFT, EX_RSHIFT, EX_LWIN, EX_RWIN,
    ] {
        if combo.exact & bit != 0 && !st.exact_mod(bit) {
            return false;
        }
    }
    for bit in [ANY_CTRL, ANY_ALT, ANY_SHIFT, ANY_WIN] {
        if combo.any & bit != 0 && !st.any_mod(bit) {
            return false;
        }
    }
    true
}

pub fn main_key_supported(key: MainKey) -> bool {
    vk_of_main(key).is_some()
}

// ─────────────────────────── hooks ───────────────────────────

unsafe extern "system" fn keyboard_hook(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code >= 0 {
        let settings = read_settings();
        if settings.enabled {
            let kb = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };
            let vk = kb.vkCode;
            let msg = w_param.0 as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            let mut swallow = false;

            for slot in Slot::ALL {
                let Some(combo) = settings.combos[slot.index()] else {
                    DETECTOR.update(slot, false);
                    continue;
                };
                let main_vk = combo.main.and_then(vk_of_main).unwrap_or(0);
                let main_event = main_vk != 0 && vk == main_vk && !main_vk_is_mouse(main_vk);
                if main_event && is_down && mods_down(&combo) {
                    // Combo engaged via its main key: swallow the keystroke.
                    SWALLOWED[slot.index()].store(true, Ordering::SeqCst);
                    DETECTOR.update(slot, true);
                    swallow = true;
                    continue;
                }
                if main_event && is_up && SWALLOWED[slot.index()].swap(false, Ordering::SeqCst) {
                    // Paired release of a swallowed press — swallow it too.
                    DETECTOR.update(slot, false);
                    swallow = true;
                    continue;
                }
                // Polling path: modifier-only combos, main-key combos whose
                // modifiers were released first, and general convergence.
                DETECTOR.update(slot, combo_is_down(&combo, &AsyncState));
            }

            if swallow {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

fn main_vk_is_mouse(vk: u32) -> bool {
    vk == VK_XBUTTON1 || vk == VK_XBUTTON2
}

/// Mouse side-button hook — installed only while an active combo uses
/// mouse4/mouse5. Mirrors the keyboard hook's swallow semantics so the
/// foreground app never sees the binding (no stray back/forward navigation).
unsafe extern "system" fn mouse_hook(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code >= 0 {
        let settings = read_settings();
        let msg = w_param.0 as u32;
        if settings.enabled
            && settings.needs_mouse()
            && (msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP)
        {
            let ms = unsafe { &*(l_param.0 as *const MSLLHOOKSTRUCT) };
            // HIWORD(mouseData) carries XBUTTON1 (1) / XBUTTON2 (2).
            let vk = match (ms.mouseData >> 16) & 0xFFFF {
                1 => VK_XBUTTON1,
                2 => VK_XBUTTON2,
                _ => 0,
            };
            let is_down = msg == WM_XBUTTONDOWN;
            let mut swallow = false;
            if vk != 0 {
                for slot in Slot::ALL {
                    let Some(combo) = settings.combos[slot.index()] else { continue };
                    let Some(main) = combo.main else { continue };
                    if vk_of_main(main) != Some(vk) {
                        continue;
                    }
                    if is_down {
                        if mods_down(&combo) {
                            SWALLOWED[slot.index()].store(true, Ordering::SeqCst);
                            DETECTOR.update(slot, true);
                            swallow = true;
                        }
                    } else if SWALLOWED[slot.index()].swap(false, Ordering::SeqCst) {
                        DETECTOR.update(slot, false);
                        swallow = true;
                    }
                }
            }
            if swallow {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

/// Install WH_MOUSE_LL from the pump (hook) thread; None + status on failure.
unsafe fn install_mouse_hook() -> Option<HHOOK> {
    match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) } {
        Ok(h) => Some(h),
        Err(e) => {
            report_status(
                "windows-hooks",
                &format!("WH_MOUSE_LL install failed ({e}); mouse bindings inactive"),
            );
            None
        }
    }
}

// ─────────────────────────── lifecycle ───────────────────────────

pub fn start() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return; // already capturing
    }
    report_status("starting", "installing low-level input hooks");
    let handle = std::thread::spawn(|| unsafe {
        // Register the thread id and force message-queue creation *before*
        // installing the hooks, so a later PostThreadMessageW (shutdown /
        // rehook) can always reach this thread.
        HOOK_TID.store(GetCurrentThreadId(), Ordering::SeqCst);
        let mut warm = MSG::default();
        let _ = PeekMessageW(&mut warm, None, 0, 0, PM_NOREMOVE);

        let kb_hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
            Ok(h) => h,
            Err(e) => {
                report_status("unavailable", &format!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {e}"));
                HOOK_TID.store(0, Ordering::SeqCst);
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }
        };
        // Mouse hook only while an active combo uses a side button — a plain
        // keyboard setup pays zero per-mouse-event overhead.
        let mut ms_hook = if read_settings().needs_mouse() {
            install_mouse_hook()
        } else {
            None
        };
        report_status("windows-hooks", "low-level keyboard/mouse hooks installed");

        // The hook thread must pump messages, otherwise the system stops
        // delivering low-level hook callbacks (and eventually times the hook
        // out silently). WM_QUIT (posted by `stop()`) ends the loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_APP_REHOOK {
                // Settings changed: reconcile the mouse hook on this thread.
                if read_settings().needs_mouse() {
                    if ms_hook.is_none() {
                        ms_hook = install_mouse_hook();
                    }
                } else if let Some(h) = ms_hook.take() {
                    let _ = UnhookWindowsHookEx(h);
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(kb_hook);
        if let Some(h) = ms_hook {
            let _ = UnhookWindowsHookEx(h);
        }
        HOOK_TID.store(0, Ordering::SeqCst);
        RUNNING.store(false, Ordering::SeqCst);
    });

    if let Ok(mut slot) = HOOK_THREAD.lock() {
        *slot = Some(handle);
    }
}

pub fn on_settings_updated() {
    for s in SWALLOWED.iter() {
        s.store(false, Ordering::SeqCst);
    }
    // Ask the pump thread to reconcile the mouse hook with the new combos
    // (hooks must be installed/uninstalled on the thread whose message loop
    // dispatches them). Best effort — if the thread isn't up yet, it reads
    // the current settings when it starts and installs accordingly.
    let tid = HOOK_TID.load(Ordering::SeqCst);
    if tid != 0 {
        let _ = unsafe { PostThreadMessageW(tid, WM_APP_REHOOK, WPARAM(0), LPARAM(0)) };
    }
}

/// Stop the hook engine and join its thread. Called from `deinit` — the
/// library may be unloaded right after, and a low-level hook left behind in
/// an unloaded DLL would fault inside the system-wide input chain.
pub fn stop() {
    for s in SWALLOWED.iter() {
        s.store(false, Ordering::SeqCst);
    }
    // Wait (bounded) for the hook thread to publish its thread id, then post
    // WM_QUIT. The retry covers two races: the thread not yet scheduled right
    // after start, and PostThreadMessageW arriving before the queue exists
    // (the thread forces queue creation before installing the hook, so this
    // normally succeeds on the first attempt).
    for _ in 0..200 {
        let tid = HOOK_TID.load(Ordering::SeqCst);
        if tid != 0 {
            let ok = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok();
            if ok {
                break;
            }
        }
        if !RUNNING.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if let Ok(mut slot) = HOOK_THREAD.lock() {
        if let Some(h) = slot.take() {
            let _ = h.join();
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
}
