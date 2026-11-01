//! MicYou native plugin — `opss.hotkey` (快捷键控制 Hotkey Control)
//!
//! System-wide hotkeys that control MicYou's mute / monitoring state:
//! toggle bindings **and** hold bindings (push-to-talk / push-to-mute), on
//! Windows, macOS and Linux (X11 *and* Wayland). The host's own
//! `register_hotkey` API cannot serve this: it is press-only (no release
//! events → no PTT), X11-only on Linux, and cannot express modifier-only or
//! mouse-button combos. Like the Voice-Typing plugin on Windows, we capture
//! input ourselves — one native backend per platform — but here the capture
//! layer is fully cross-platform.
//!
//! Thread discipline (api-reference.md「调用约定与并发安全约束」):
//!
//! | thread | touches | Host API |
//! | --- | --- | --- |
//! | host-dispatched (init / deinit / handle_event / handle_message incl. the 20 ms interval pump) | Core state machine + engine | ✅ only here |
//! | capture backend thread(s) (hooks / event tap / evdev poll) | neutral `CaptureEvent` channel | ❌ never |
//!
//! The capture threads push `(Slot, Edge)` events into a crossbeam channel;
//! the pump — running on a host-dispatched timer tick — drains it, feeds the
//! [`actions::Engine`], and executes the resulting `set_muted` /
//! `set_monitoring` commands. Worst-case added latency: one tick (20 ms),
//! far below PTT perceptibility, and the contract is honored to the letter.
//!
//! Degraded mode: when no native backend is available (macOS without
//! Accessibility permission, Linux without `/dev/input` access, hook install
//! failure) the core registers the two *toggle* bindings through the host's
//! `register_hotkey` (press-only, X11/Windows/macOS) and reports the hold
//! bindings as unsupported via the panel status.
//!
//! Not a DSP plugin: `micyou_plugin_process` is intentionally NOT exported.

#![allow(non_camel_case_types)]

mod abi;
mod actions;
mod capture;
mod combo;

use abi::{mpl_host_api_t, mpl_plugin_info_t, mpl_result_t};
use actions::{Command, Edge, Engine, Slot, SLOT_COUNT};
use capture::{CaptureEvent, CaptureHandle, CaptureSettings};
use combo::Combo;
use crossbeam_channel::{unbounded, Receiver};
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PLUGIN_ID: &[u8] = b"opss.hotkey\0";
const PLUGIN_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// Interval-tick payload tag (filters foreign timers routed to our id).
const TICK_PAYLOAD: &str = "hotkey-pump";
/// Pump cadence: worst-case added latency for a hotkey press.
const PUMP_MS: u64 = 20;
/// Panel descriptor id (plugin.json ui.panels[0].id).
const PANEL_ID: &str = "settings";
/// `status` config mirror write throttle (change-gated).
const STATUS_WRITE: Duration = Duration::from_millis(400);
/// Force a status mirror refresh at least this often.
const STATUS_FORCE: Duration = Duration::from_secs(5);

/// Config keys (flat + configSchema-friendly; see plugin.json).
const CFG_ENABLED: &str = "enabled";
const CFG_NOTIFY: &str = "notifyChanges";
const CFG_HOTKEY: [&str; SLOT_COUNT] = ["muteHotkey", "monitorHotkey", "pttHotkey", "ptmHotkey"];

/// Factory defaults when a config key was never written.
const DEFAULT_HOTKEY: [&str; SLOT_COUNT] = ["ctrl+alt+m", "ctrl+alt+e", "", ""];

// ─────────────────────────── core state ───────────────────────────

/// Per-binding parse result, mirrored into the panel status.
#[derive(Clone, Default)]
struct BindingState {
    raw: String,
    effective: Option<Combo>,
    error: Option<String>,
}

struct Core {
    enabled: bool,
    notify_changes: bool,
    bindings: [BindingState; SLOT_COUNT],
    engine: Engine,
    rx: Receiver<CaptureEvent>,
    backend: Option<CaptureHandle>,
    backend_engine: String,
    backend_detail: String,
    /// Host-fallback registrations: (hotkey id → slot). Only armed while no
    /// native backend is capturing.
    fallback: Vec<(u64, Slot)>,
    /// Every shortcut ever registered with the host (no unregister API
    /// exists): ids are reused on re-arm instead of re-registering.
    fallback_ids: std::collections::HashMap<String, u64>,
    fallback_active: bool,
    interval_id: u64,
    last_status_json: String,
    last_status_write: Instant,
}

static CORE: Mutex<Option<Core>> = Mutex::new(None);

/// Keep the lock helper tiny and poisoning-tolerant (a panicked pump must
/// not brick the plugin for the rest of the host session).
fn with_core<R>(f: impl FnOnce(&mut Core) -> R) -> Option<R> {
    let mut slot = CORE.lock().ok()?;
    slot.as_mut().map(f)
}

// ─────────────────────────── config ───────────────────────────

/// Read one config value as raw JSON text (`get_config` returns serialized
/// JSON, so strings arrive quoted).
fn cfg_json(key: &str) -> Option<String> {
    abi::get_config(key)
}

fn cfg_string(key: &str, default: &str) -> String {
    cfg_json(key)
        .and_then(|s| serde_json::from_str::<String>(&s).ok())
        .unwrap_or_else(|| default.to_string())
}

fn cfg_bool(key: &str, default: bool) -> bool {
    cfg_json(key)
        .and_then(|s| serde_json::from_str::<bool>(&s).ok())
        .unwrap_or(default)
}

impl Core {
    fn new(rx: Receiver<CaptureEvent>) -> Self {
        Self {
            enabled: true,
            notify_changes: false,
            bindings: std::array::from_fn(|_| BindingState::default()),
            engine: Engine::default(),
            rx,
            backend: None,
            backend_engine: "starting".into(),
            backend_detail: String::new(),
            fallback: Vec::new(),
            fallback_ids: std::collections::HashMap::new(),
            fallback_active: false,
            interval_id: 0,
            last_status_json: String::new(),
            last_status_write: Instant::now() - STATUS_WRITE,
        }
    }

    /// (Re-)read config: master switch, the four binding strings, notify
    /// flag. Invalid combos keep their slot inert and record a precise error
    /// (surfaced in the panel + log).
    fn load_config(&mut self) {
        self.enabled = cfg_bool(CFG_ENABLED, true);
        self.notify_changes = cfg_bool(CFG_NOTIFY, false);
        for (i, key) in CFG_HOTKEY.iter().enumerate() {
            let raw = cfg_string(key, DEFAULT_HOTKEY[i]);
            let trimmed = raw.trim();
            let (effective, error) = if trimmed.is_empty() {
                (None, None)
            } else {
                match combo::parse_combo(trimmed) {
                    Ok(c) => (Some(c), None),
                    Err(e) => {
                        abi::log_warn(&format!("无效快捷键 [{trimmed}] ({key}): {e}"));
                        (None, Some(e))
                    }
                }
            };
            self.bindings[i] = BindingState {
                raw: trimmed.to_string(),
                effective,
                error,
            };
        }
        for w in self.capture_settings().duplicate_warnings() {
            abi::log_warn(&w);
        }
    }

    fn capture_settings(&self) -> CaptureSettings {
        CaptureSettings::new(
            self.enabled,
            std::array::from_fn(|i| self.bindings[i].effective),
        )
    }

    /// Push the current bindings into the capture backend (live reload) and
    /// report platform-unsupported main keys.
    fn apply_capture_settings(&mut self) {
        let settings = self.capture_settings();
        let unsupported = settings.unsupported_on(capture::platform::main_key_supported);
        for (slot, c) in unsupported {
            let msg = format!(
                "绑定 {}=[{c}] 的主键在本平台不存在，已忽略 (main key unavailable on this platform)",
                slot.name()
            );
            abi::log_warn(&msg);
            self.bindings[slot.index()].error = Some(msg);
        }
        capture::update_settings(settings);
    }

    fn notify_text(&self, cmd: Command) -> Option<(&'static str, &'static str)> {
        if !self.notify_changes {
            return None;
        }
        Some(match cmd {
            Command::SetMuted(true) => ("MicYou Hotkey", "已静音 Muted"),
            Command::SetMuted(false) => ("MicYou Hotkey", "已取消静音 Unmuted"),
            Command::SetMonitoring(true) => ("MicYou Hotkey", "耳返监听已开启 Monitoring on"),
            Command::SetMonitoring(false) => ("MicYou Hotkey", "耳返监听已关闭 Monitoring off"),
        })
    }

    /// Execute one engine command against the Host API (pump thread only).
    fn exec(&mut self, cmd: Command) {
        let ok = match cmd {
            Command::SetMuted(m) => abi::set_muted(m),
            Command::SetMonitoring(m) => abi::set_monitoring(m),
        };
        if ok {
            if let Some((title, body)) = self.notify_text(cmd) {
                abi::notify(title, body);
            }
            abi::log_debug(&format!("host command applied: {cmd:?}"));
        } else {
            abi::log_warn(&format!("host rejected command {cmd:?}"));
            // Re-sync the engine from the authoritative host state.
            if let Some(m) = abi::get_muted() {
                self.engine.sync_muted(m);
            }
            if let Some(m) = abi::get_monitoring() {
                self.engine.sync_monitoring(m);
            }
        }
    }

    // ── pump (host-dispatched threads only) ──

    /// Drain capture events and execute whatever the engine decides.
    fn pump(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(CaptureEvent::Edge { slot, edge }) => {
                    for cmd in self.engine.on_edge(slot, edge) {
                        self.exec(cmd);
                    }
                    // Any edge can flip states the panel mirrors.
                    self.write_status(false);
                }
                Ok(CaptureEvent::Status { engine, detail }) => {
                    let changed = self.backend_engine != engine || self.backend_detail != detail;
                    self.backend_engine = engine;
                    self.backend_detail = detail;
                    if changed {
                        abi::log_info(&format!(
                            "capture backend: {} — {}",
                            self.backend_engine, self.backend_detail
                        ));
                        self.reconcile_fallback();
                        self.write_status(true);
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Native backend gone → arm host hotkeys for the toggle slots; native
    /// backend alive → disarm (messages from stale registrations are
    /// ignored). The host offers no unregister API, so once-registered
    /// shortcut ids live in `fallback_ids` forever and are *reused* on
    /// re-arm — re-registering the same shortcut would be rejected by the
    /// host's global-hotkey layer as a duplicate.
    fn reconcile_fallback(&mut self) {
        let native_ok = !matches!(
            self.backend_engine.as_str(),
            "unavailable" | "starting" | "stopped"
        );
        if native_ok {
            if self.fallback_active {
                abi::log_info("native capture back online — host-fallback hotkeys disarmed");
            }
            self.fallback_active = false;
            self.fallback.clear();
            return;
        }
        if self.backend_engine == "starting" || self.fallback_active {
            return; // verdict pending / already armed
        }
        self.fallback_active = true;
        if !self.enabled {
            return;
        }
        let mut armed = 0;
        for slot in [Slot::MuteToggle, Slot::MonitorToggle] {
            let Some(combo) = self.bindings[slot.index()].effective else {
                continue;
            };
            let Some(shortcut) = combo.to_host_string() else {
                abi::log_warn(&format!(
                    "fallback 无法注册 [{combo}]（宿主快捷键不支持修饰键组合/鼠标侧键）"
                ));
                continue;
            };
            let id = if let Some(&id) = self.fallback_ids.get(&shortcut) {
                id // still registered host-side — reuse
            } else {
                let id = abi::register_hotkey(&shortcut);
                if id == 0 {
                    abi::log_warn(&format!("register_hotkey({shortcut}) 被宿主拒绝"));
                    continue;
                }
                self.fallback_ids.insert(shortcut.clone(), id);
                id
            };
            self.fallback.push((id, slot));
            armed += 1;
        }
        if armed > 0 {
            abi::log_info(&format!(
                "host-fallback armed for {armed} toggle binding(s); PTT/PTM 在降级模式下不可用"
            ));
        }
    }

    /// Live config reload (host broadcast `config:changed` / panel saves).
    fn reload(&mut self) {
        // A hold whose binding just changed can never see its release: the
        // shared detector reset drops the edge, so restore the mic here.
        for cmd in self.engine.shutdown_restore() {
            self.exec(cmd);
        }
        self.load_config();
        self.apply_capture_settings();
        // Re-evaluate the fallback with the new bindings (disarm + rearm
        // reuses host-side registrations via `fallback_ids`).
        if self.fallback_active || self.backend_engine == "unavailable" {
            self.fallback_active = false;
            self.fallback.clear();
            self.reconcile_fallback();
        }
        abi::log_info(&format!(
            "settings applied: enabled={} bindings=[{}]",
            self.enabled,
            Slot::ALL
                .iter()
                .map(|s| self.bindings[s.index()]
                    .effective
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".into()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        self.write_status(true);
    }

    /// Panel "test" action: drive the same code path as a real press.
    fn simulate(&mut self, slot: Slot) {
        for cmd in self.engine.on_edge(slot, Edge::Press) {
            self.exec(cmd);
        }
        // Hold bindings: close the session right away so the test can never
        // leave the mic in the held state.
        if slot.is_hold() {
            for cmd in self.engine.on_edge(slot, Edge::Release) {
                self.exec(cmd);
            }
        }
        self.write_status(true);
    }

    // ── status mirror (panel reads it via get_config) ──

    fn status_json(&self) -> String {
        let mut bindings = serde_json::Map::new();
        for s in Slot::ALL {
            let b = &self.bindings[s.index()];
            bindings.insert(
                s.name().to_string(),
                serde_json::json!({
                    "raw": b.raw,
                    "effective": b.effective.map(|c| c.to_string()).unwrap_or_default(),
                    "error": b.error,
                    "active": match s {
                        Slot::PushToTalk => self.engine.ptt_active(),
                        Slot::PushToMute => self.engine.ptm_active(),
                        _ => false,
                    },
                }),
            );
        }
        let native = !matches!(
            self.backend_engine.as_str(),
            "unavailable" | "starting" | "stopped"
        );
        serde_json::json!({
            "engine": if self.enabled { self.backend_engine.as_str() } else { "disabled" },
            "detail": self.backend_detail,
            "enabled": self.enabled,
            "muted": self.engine.muted(),
            "monitoring": self.engine.monitoring(),
            "pttActive": self.engine.ptt_active(),
            "ptmActive": self.engine.ptm_active(),
            // Release events (PTT/PTM) only exist on a native backend.
            "holdSupported": native,
            // Whether the backend can suppress the hotkey keystroke.
            "swallowSupported": native
                && !self.backend_engine.contains("listen")
                && !cfg!(target_os = "linux"),
            "fallbackActive": self.fallback_active,
            "version": env!("CARGO_PKG_VERSION"),
            "bindings": bindings,
        })
        .to_string()
    }

    fn write_status(&mut self, force: bool) {
        let json = self.status_json();
        let now = Instant::now();
        let due = now - self.last_status_write >= STATUS_WRITE;
        let stale = now - self.last_status_write >= STATUS_FORCE;
        if (force && due) || (!force && (json != self.last_status_json && due || stale)) {
            self.last_status_json = json.clone();
            self.last_status_write = now;
            if !abi::set_config("status", &json) {
                abi::log_warn("set_config(status) failed");
            }
        }
    }

    fn shutdown(&mut self) {
        // 1. Stop capture threads and JOIN them — mandatory before the host
        //    may unload this library.
        if let Some(handle) = self.backend.take() {
            handle.shutdown();
        }
        // 2. Never leave the mic hot (PTT) or dead (PTM) after unload.
        for cmd in self.engine.shutdown_restore() {
            self.exec(cmd);
        }
        // 3. Disarm timers.
        abi::clear_interval(self.interval_id);
        self.interval_id = 0;
        self.fallback.clear();
        self.fallback_active = false;
        self.backend_engine = "stopped".into();
    }
}

/// Every FFI entry point runs through this guard: panics must never cross
/// the C ABI boundary (UB); they degrade to `MPL_ERR_RUNTIME`.
fn guard<F: FnOnce() -> mpl_result_t>(f: F) -> mpl_result_t {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            // Best-effort log without the host lock (it may be poisoned).
            eprintln!("[micyou-hotkey] panic contained at FFI boundary");
            mpl_result_t::MPL_ERR_RUNTIME
        }
    }
}

// ─────────────────────────── FFI entry points ───────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn micyou_plugin_info() -> *const mpl_plugin_info_t {
    static INFO: mpl_plugin_info_t = mpl_plugin_info_t {
        abi_version: abi::MPL_ABI_VERSION,
        api_version: abi::MPL_API_VERSION,
        id: PLUGIN_ID.as_ptr() as *const c_char,
        version: PLUGIN_VERSION.as_ptr() as *const c_char,
    };
    &INFO
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn micyou_plugin_init(host: *const mpl_host_api_t) -> mpl_result_t {
    guard(|| {
        if host.is_null() {
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        // The host pointer is only valid during init — copy the table by value.
        let table = unsafe { *host };
        abi::store_host(table);

        // Re-init safety: a previous instance must be fully torn down first.
        if let Some(mut old) = CORE.lock().ok().and_then(|mut s| s.take()) {
            old.shutdown();
        }

        let (tx, rx) = unbounded();
        let mut core = Core::new(rx);

        if let Some(info) = abi::host_info() {
            abi::log_info(&format!("initializing against host {info}"));
        }
        if abi::plugin_dir().is_none() {
            abi::log_warn("plugin_dir unavailable (cosmetic only)");
        }

        core.load_config();

        // Authoritative initial control-plane state.
        core.engine = Engine::new(
            abi::get_muted().unwrap_or(false),
            abi::get_monitoring().unwrap_or(false),
        );

        abi::set_panel_icon(PANEL_ID, "⌨️");

        // Start the native capture backend; availability verdicts arrive as
        // Status events and drive the host-fallback decision.
        let settings = core.capture_settings();
        core.backend = Some(capture::start(settings, tx));

        // The pump: a fast interval tick drains capture events on a
        // host-dispatched thread (threading contract).
        core.interval_id = abi::set_interval(PUMP_MS, TICK_PAYLOAD);
        if core.interval_id == 0 {
            abi::log_error("set_interval unavailable — capture events cannot be pumped");
            // Keep running: fallback hotkeys and panel tests still work
            // because they execute inside handle_message directly.
        }

        abi::log_info(&format!(
            "Hotkey Control v{} ready — enabled={} bindings=[{}]",
            env!("CARGO_PKG_VERSION"),
            core.enabled,
            Slot::ALL
                .iter()
                .map(|s| core.bindings[s.index()]
                    .effective
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".into()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        core.apply_capture_settings();
        core.write_status(true);

        match CORE.lock() {
            Ok(mut slot) => *slot = Some(core),
            Err(_) => return mpl_result_t::MPL_ERR_RUNTIME,
        }
        mpl_result_t::MPL_OK
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn micyou_plugin_deinit() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(mut core) = CORE.lock().ok().and_then(|mut s| s.take()) {
            core.shutdown();
        }
        abi::clear_host();
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn micyou_plugin_handle_event(
    event_type: *const c_char,
    json: *const c_char,
) -> mpl_result_t {
    guard(|| {
        let ty = unsafe { abi::cstr_or(event_type, "") }.to_string();
        let payload = unsafe { abi::cstr_or(json, "{}") }.to_string();
        let _ = with_core(|core| {
            // Piggyback: any host-dispatched callback also drains the queue,
            // shaving latency when ticks are sparse.
            core.pump();
            match ty.as_str() {
                "mute_changed" => {
                    #[derive(serde::Deserialize)]
                    struct M {
                        #[serde(default)]
                        muted: bool,
                    }
                    if let Ok(m) = serde_json::from_str::<M>(&payload) {
                        core.engine.sync_muted(m.muted);
                        core.write_status(true);
                    }
                }
                "monitoring_changed" => {
                    #[derive(serde::Deserialize)]
                    struct M {
                        #[serde(default)]
                        enabled: bool,
                    }
                    if let Ok(m) = serde_json::from_str::<M>(&payload) {
                        core.engine.sync_monitoring(m.enabled);
                        core.write_status(true);
                    }
                }
                _ => {}
            }
        });
        mpl_result_t::MPL_OK
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn micyou_plugin_handle_message(
    _source: *const c_char,
    topic: *const c_char,
    payload: *const u8,
    payload_len: u32,
) -> mpl_result_t {
    guard(|| {
        let topic = unsafe { abi::cstr_or(topic, "") }.to_string();
        let bytes: &[u8] = if payload.is_null() || payload_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(payload, payload_len as usize) }
        };

        match topic.as_str() {
            "interval:tick" => {
                #[derive(serde::Deserialize)]
                struct Tick {
                    #[serde(default)]
                    interval: u64,
                    #[serde(default)]
                    payload: String,
                }
                let Ok(t) = serde_json::from_slice::<Tick>(bytes) else {
                    return mpl_result_t::MPL_OK;
                };
                // Only OUR interval with OUR tag (stale timers from previous
                // plugin instances can still be routed here by plugin id).
                if t.payload != TICK_PAYLOAD {
                    return mpl_result_t::MPL_OK;
                }
                let _ = with_core(|core| {
                    if core.interval_id != 0 && t.interval != core.interval_id {
                        return; // stale timer id → ignore
                    }
                    core.pump();
                    core.write_status(false);
                });
            }
            "config:changed" => {
                let _ = with_core(|core| {
                    core.pump();
                    core.reload();
                });
            }
            other => {
                if let Some(id_str) = other.strip_prefix("hotkey:") {
                    // Host-fallback registration fired (press-only).
                    let id = id_str.parse::<u64>().unwrap_or(0);
                    let _ = with_core(|core| {
                        core.pump();
                        if !core.enabled {
                            return;
                        }
                        let Some(&(_, slot)) = core.fallback.iter().find(|(h, _)| *h == id) else {
                            return; // stale / foreign registration
                        };
                        for cmd in core.engine.on_edge(slot, Edge::Press) {
                            core.exec(cmd);
                        }
                        core.write_status(true);
                    });
                } else if let Some(action) = other.strip_prefix("ui:") {
                    let action = action.to_string();
                    let body = bytes.to_vec();
                    let _ = with_core(move |core| core.ui_action(&action, &body));
                }
                // Unknown topics (timer:expired, broadcasts…) ignored.
            }
        }
        mpl_result_t::MPL_OK
    })
}

impl Core {
    /// Panel bridge actions (`trigger` → topic `ui:<action>`).
    fn ui_action(&mut self, action: &str, payload: &[u8]) {
        match action {
            "test" => {
                #[derive(serde::Deserialize)]
                struct Args {
                    #[serde(default)]
                    slot: String,
                }
                let args: Args = serde_json::from_slice(payload).unwrap_or(Args {
                    slot: String::new(),
                });
                match Slot::parse(&args.slot) {
                    Some(slot) => {
                        self.pump();
                        self.simulate(slot);
                    }
                    None => abi::log_warn(&format!("ui:test unknown slot {:?}", args.slot)),
                }
            }
            "apply" | "reload" => {
                self.pump();
                self.reload();
            }
            "status" => {
                self.pump();
                self.write_status(true);
            }
            "log" => {
                // Panel console bridge → host plugin log.
                #[derive(serde::Deserialize)]
                struct LogArgs {
                    #[serde(default)]
                    level: String,
                    #[serde(default)]
                    message: String,
                }
                if let Ok(args) = serde_json::from_slice::<LogArgs>(payload) {
                    let level = match args.level.as_str() {
                        "error" => abi::LOG_ERROR,
                        "warn" => abi::LOG_WARN,
                        "debug" => abi::LOG_DEBUG,
                        _ => abi::LOG_INFO,
                    };
                    abi::log(level, &format!("[panel] {}", args.message));
                }
            }
            other => abi::log_debug(&format!("ignoring ui action {other}")),
        }
    }
}

// NOTE: `micyou_plugin_process` is intentionally absent — this plugin never
// joins the real-time DSP chain (kind = "utility").

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkeys_parse() {
        for (i, d) in DEFAULT_HOTKEY.iter().enumerate() {
            if d.is_empty() {
                continue;
            }
            assert!(
                combo::parse_combo(d).is_ok(),
                "default binding {i} [{d}] must parse"
            );
        }
    }

    #[test]
    fn defaults_do_not_collide() {
        let combos: Vec<_> = DEFAULT_HOTKEY
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| combo::parse_combo(s).unwrap())
            .collect();
        for a in 0..combos.len() {
            for b in (a + 1)..combos.len() {
                assert_ne!(combos[a], combos[b], "default bindings must differ");
            }
        }
    }
}
