//! Cross-platform input-capture layer.
//!
//! One capture backend per desktop platform reports neutral
//! [`CaptureEvent`]s into a crossbeam channel; the plugin core drains that
//! channel **on host-dispatched threads only** (the interval-tick pump) and
//! turns edges into Host API calls. Capture threads never touch the host
//! table (api-reference.md §调用约定).
//!
//! | platform | backend | press **and** release | swallows hotkey |
//! | --- | --- | --- | --- |
//! | Windows | `WH_KEYBOARD_LL` + `WH_MOUSE_LL` hooks | ✅ | ✅ (main key, while its modifiers are held) |
//! | macOS | `CGEventTap` (session tap, run-loop thread) | ✅ | ✅ (same policy; needs Accessibility permission) |
//! | Linux | evdev (`/dev/input/event*`, X11 **and** Wayland) | ✅ | ❌ (raw tap is observe-only) |
//!
//! When a native backend is unavailable (macOS without Accessibility
//! permission, Linux without `/dev/input` access, hook install failure) the
//! core degrades to the host's `register_hotkey` for the two *toggle*
//! bindings; the hold-style PTT/PTM bindings need release events and are
//! reported unsupported in that mode.
//!
//! Modifier-only combos (`lctrl+lshift`, IME-style) are supported by every
//! native backend but can never go through the host fallback (global-hotkey
//! requires a non-modifier main key).

pub mod platform;

#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

use crate::actions::{Edge, Slot, SLOT_COUNT};
use crate::combo::{Combo, MainKey};
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

/// Live snapshot read by backend threads on every input event.
#[derive(Clone, Default)]
pub struct CaptureSettings {
    pub enabled: bool,
    /// Combo per binding slot (indexed by [`Slot::index`]); `None` = unbound.
    pub combos: [Option<Combo>; SLOT_COUNT],
}

impl CaptureSettings {
    pub fn new(enabled: bool, combos: [Option<Combo>; SLOT_COUNT]) -> Self {
        Self { enabled, combos }
    }

    /// Any active combo needs mouse-button capture?
    #[allow(dead_code)] // used by the Windows backend only
    pub fn needs_mouse(&self) -> bool {
        self.enabled && self.combos.iter().flatten().any(|c| c.needs_mouse())
    }

    /// Duplicate-combo warnings (two slots bound to the same combo fire both
    /// actions on one press — legal but almost always a mistake).
    pub fn duplicate_warnings(&self) -> Vec<String> {
        let mut warns = Vec::new();
        for a in 0..SLOT_COUNT {
            for b in (a + 1)..SLOT_COUNT {
                if let (Some(ca), Some(cb)) = (self.combos[a], self.combos[b]) {
                    if ca == cb {
                        warns.push(format!(
                            "绑定冲突：{ca} 同时绑定到 {} 与 {}，一次按键会触发两个动作",
                            Slot::ALL[a].name(),
                            Slot::ALL[b].name()
                        ));
                    }
                }
            }
        }
        warns
    }

    /// Combos whose main key this platform cannot represent (e.g. F24 or
    /// `insert` on macOS) — they are accepted by the parser but inert here.
    pub fn unsupported_on<F: Fn(MainKey) -> bool>(&self, main_supported: F) -> Vec<(Slot, Combo)> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        for slot in Slot::ALL {
            if let Some(c) = self.combos[slot.index()] {
                if let Some(m) = c.main {
                    if !main_supported(m) {
                        out.push((slot, c));
                    }
                }
            }
        }
        out
    }
}

/// Neutral event produced by capture backends.
#[derive(Clone, Debug)]
pub enum CaptureEvent {
    /// A binding's combo transitioned (press = fully held, release = let go).
    Edge { slot: Slot, edge: Edge },
    /// Backend state change for the panel status mirror / logs. `engine` is a
    /// stable id: `windows-hooks`, `macos-tap`, `macos-tap-listen`,
    /// `linux-evdev`, `unavailable`, `starting`.
    Status { engine: String, detail: String },
}

// ─────────────────────────── shared statics ───────────────────────────

static SETTINGS: RwLock<CaptureSettings> = RwLock::new(CaptureSettings {
    enabled: false,
    combos: [None; SLOT_COUNT],
});
static TX: Mutex<Option<Sender<CaptureEvent>>> = Mutex::new(None);

pub fn read_settings() -> CaptureSettings {
    SETTINGS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Swap in a new snapshot (called from the core on host-dispatched threads
/// after config changes) and reset every edge latch — latches refer to the
/// old combos, and a held PTT whose combo just changed would otherwise never
/// see its release.
pub fn update_settings(new: CaptureSettings) {
    if let Ok(mut s) = SETTINGS.write() {
        *s = new;
    }
    DETECTOR.reset();
    platform::on_settings_updated();
}

/// Send an event to the core (no-op after shutdown).
pub fn emit(ev: CaptureEvent) {
    if let Ok(slot) = TX.lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(ev);
        }
    }
}

pub fn report_status(engine: &str, detail: &str) {
    emit(CaptureEvent::Status {
        engine: engine.to_string(),
        detail: detail.to_string(),
    });
}

// ─────────────────────────── edge detection ───────────────────────────

/// Shared per-slot press/release latch. Backends evaluate "is this slot's
/// combo currently fully held?" on every input event and call [`update`];
/// the detector edge-triggers so auto-repeat and redundant polls are inert.
pub struct EdgeDetector {
    held: [AtomicBool; SLOT_COUNT],
}

impl EdgeDetector {
    pub const fn new() -> Self {
        Self {
            held: [const { AtomicBool::new(false) }; SLOT_COUNT],
        }
    }

    pub fn update(&self, slot: Slot, down: bool) {
        let latch = &self.held[slot.index()];
        if down {
            if !latch.swap(true, Ordering::SeqCst) {
                emit(CaptureEvent::Edge {
                    slot,
                    edge: Edge::Press,
                });
            }
        } else if latch.swap(false, Ordering::SeqCst) {
            emit(CaptureEvent::Edge {
                slot,
                edge: Edge::Release,
            });
        }
    }

    pub fn reset(&self) {
        // Forgetting the latch without emitting: settings changes end holds
        // at the core level (the pump releases PTT/PTM latches on reload),
        // so a synthetic Release here could double-fire.
        for l in self.held.iter() {
            l.store(false, Ordering::SeqCst);
        }
    }
}

pub static DETECTOR: EdgeDetector = EdgeDetector::new();

// ─────────────────────────── key-state abstraction ───────────────────────────

/// Per-platform view of "which keys are currently down", used by the shared
/// [`combo_is_down`] evaluation.
pub trait KeyState {
    /// One exact-modifier bit (`EX_*`) held?
    fn exact_mod(&self, bit: u8) -> bool;
    /// One side-agnostic modifier bit (`ANY_*`) held?
    fn any_mod(&self, bit: u8) -> bool;
    /// Main key held? `None` when this platform cannot represent the key
    /// (the combo then never fires — the core reports it in the status).
    fn main(&self, key: MainKey) -> Option<bool>;
}

/// Evaluate a combo against a platform key-state snapshot.
pub fn combo_is_down(combo: &Combo, st: &impl KeyState) -> bool {
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
    match combo.main {
        None => true,
        Some(m) => st.main(m).unwrap_or(false),
    }
}

/// Evaluate every bound slot against `st` and push the resulting edges.
#[allow(dead_code)] // used by the macOS/Linux backends; Windows polls inline
pub fn evaluate_all(st: &impl KeyState) {
    let settings = read_settings();
    if !settings.enabled {
        // Master switch off: force every latch low so a later re-enable
        // starts from a clean slate (release events are intentionally not
        // emitted — the core already restored hold latches on reload).
        DETECTOR.reset();
        return;
    }
    for slot in Slot::ALL {
        let Some(combo) = settings.combos[slot.index()] else {
            DETECTOR.update(slot, false);
            continue;
        };
        DETECTOR.update(slot, combo_is_down(&combo, st));
    }
}

// ─────────────────────────── lifecycle (platform dispatch) ───────────────────────────

/// Opaque backend handle. `shutdown()` stops capture threads and joins them —
/// it MUST complete before the host unloads the library (a live system hook /
/// event tap pointing into an unmapped cdylib would fault the whole session).
pub struct CaptureHandle {
    _private: (),
}

impl CaptureHandle {
    pub fn shutdown(self) {
        platform::stop();
        if let Ok(mut slot) = TX.lock() {
            *slot = None;
        }
        DETECTOR.reset();
        if let Ok(mut s) = SETTINGS.write() {
            *s = CaptureSettings::default();
        }
    }
}

/// Start the platform capture backend. Returns once the backend thread is
/// spawned; the authoritative availability verdict arrives asynchronously as
/// a [`CaptureEvent::Status`] (the pump reacts by arming the host fallback).
pub fn start(settings: CaptureSettings, tx: Sender<CaptureEvent>) -> CaptureHandle {
    if let Ok(mut slot) = TX.lock() {
        *slot = Some(tx);
    }
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }
    DETECTOR.reset();
    platform::start();
    CaptureHandle { _private: () }
}
