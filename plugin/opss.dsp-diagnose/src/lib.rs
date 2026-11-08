//! MicYou native plugin entry points.
//!
//! The public ABI is the frozen C interface from `micyou_plugin_abi.h`; every
//! export is wrapped in `catch_unwind` because unwinding across FFI is UB.
//! Audio (`micyou_plugin_process*`) touches atomics and a bounded ring only.

mod abi;
mod config;
mod engine;
mod ring;
mod writer;

use abi::{cstr, Host, HostApi, LogLevel, MplResult};
use config::Config;
use engine::Engine;
use serde_json::Value;
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

const REPO_URL: &str = "https://github.com/OrientCOMPASS/DSP-Diagnose";
const VERSION_BYTES: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

static ENGINE: OnceLock<Engine> = OnceLock::new();
static WRITER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
static LAST_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static INITED: AtomicBool = AtomicBool::new(false);
static INTERVAL_ID: AtomicU64 = AtomicU64::new(0);
static TIMEOUT_ID: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
pub struct PluginInfo {
    abi_version: u32,
    api_version: u32,
    id: *const c_char,
    version: *const c_char,
}

// The pointers point at immutable NUL-terminated byte literals.
unsafe impl Send for PluginInfo {}
unsafe impl Sync for PluginInfo {}

static INFO: PluginInfo = PluginInfo {
    abi_version: abi::MPL_ABI_VERSION,
    api_version: abi::MPL_API_VERSION,
    id: b"opss.dsp-diagnose\0".as_ptr() as *const c_char,
    version: VERSION_BYTES.as_ptr() as *const c_char,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn remember_config(cfg: &Config) {
    *lock(&LAST_CONFIG) = Some(cfg.clone());
}

fn reload_config(engine: &Engine) {
    let cfg = Config::load(engine.host());
    let nodes_changed = engine.apply_config(&cfg);
    remember_config(&cfg);
    engine.log(format!("configuration applied ({} node(s))", cfg.node_count));
    engine.request_chain_repair(nodes_changed);
}

/// Called once per second by the host timer.
fn on_tick(engine: &Engine) {
    let cfg = Config::load(engine.host());
    let changed = {
        let mut last = lock(&LAST_CONFIG);
        if last.as_ref() == Some(&cfg) {
            false
        } else {
            *last = Some(cfg.clone());
            true
        }
    };
    if changed {
        let nodes_changed = engine.apply_config(&cfg);
        if nodes_changed {
            engine.log(format!("node count changed to {}", cfg.node_count));
            engine.request_chain_repair(true);
        } else {
            engine.request_chain_repair(false);
        }
    }
    engine.publish_status(false);
}

/// Host timers deliver `interval:tick` / `timer:expired` with a JSON body of
/// `{"interval"|"timer": id, "payload": "<the string passed to set_*>"}`.
fn timer_payload(payload: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    value
        .get("payload")
        .and_then(|inner| inner.as_str())
        .map(str::to_owned)
}

fn dispatch_message(engine: &Engine, topic: &str, payload: &[u8]) {
    // Unwrap host timer envelopes so the match below sees the logical topic.
    let unwrapped = match topic {
        "interval:tick" | "timer:expired" => match timer_payload(payload) {
            Some(inner) => inner,
            None => return,
        },
        other => other.to_owned(),
    };
    match unwrapped.as_str() {
        "status" => on_tick(engine),
        "chain:apply" => engine.request_chain_repair(true),
        "ui:start" | "ui:record:start" => engine.start(),
        "ui:stop" | "ui:record:stop" => engine.stop_recording(),
        "ui:toggle" => {
            if engine.is_recording() {
                engine.stop_recording();
            } else {
                engine.start();
            }
        }
        "ui:apply" => reload_config(engine),
        "ui:refresh" => engine.publish_status(true),
        "ui:chain" | "ui:chain:apply" => engine.request_chain_repair(true),
        "ui:chain:verify" => {
            let _ = engine.verify_chain();
        }
        "ui:open_repo" => {
            if !engine.host().open_url(REPO_URL) {
                engine.log_once("could not open the project page");
            }
        }
        "ui:log" => {
            if let Ok(text) = std::str::from_utf8(payload) {
                if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) {
                    let level = object.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                    let message = object
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if !message.is_empty() {
                        let level = match level {
                            "error" => LogLevel::Error,
                            "warn" => LogLevel::Warn,
                            "debug" => LogLevel::Debug,
                            "trace" => LogLevel::Trace,
                            _ => LogLevel::Info,
                        };
                        engine.host().log(level, message);
                    }
                }
            }
        }
        other => {
            if let Some(action) = other.strip_prefix("ui:") {
                engine.log(format!("unhandled panel action: {action}"));
            }
        }
    }
}

fn panic_message(_error: Box<dyn std::any::Any + Send>) -> &'static str {
    "plugin panicked"
}

// ── Exported ABI ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn micyou_plugin_info() -> *const PluginInfo {
    &INFO
}

#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_init(host: *const HostApi) -> MplResult {
    if host.is_null() {
        return MplResult::InvalidArg;
    }
    let api = unsafe { *host };
    let engine = ENGINE.get_or_init(|| Engine::new(Host::new(api)));

    if INITED.swap(true, Ordering::AcqRel) {
        return MplResult::Ok;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let cfg = Config::load(engine.host());
        engine.apply_config(&cfg);
        remember_config(&cfg);

        // Re-enabling the plugin in the same process reuses the engine, so
        // clear the shutdown flag before the writer thread starts.
        engine.stop.store(false, Ordering::Release);

        match writer::spawn(engine) {
            Ok(handle) => *lock(&WRITER) = Some(handle),
            Err(error) => engine.log_once(format!("cannot start writer thread: {error}")),
        }

        if cfg.auto_start {
            engine.start();
        }

        let interval = engine.host().set_interval(1_000, "status");
        INTERVAL_ID.store(interval, Ordering::Release);
        // The DSP registry registers this plugin only *after* init returns, so
        // reconciling the chain synchronously would see our node as stale.
        let timeout = engine.host().set_timeout(2_000, "chain:apply");
        TIMEOUT_ID.store(timeout, Ordering::Release);

        engine.log(format!(
            "DSP Diagnose {} ready ({} node(s))",
            env!("CARGO_PKG_VERSION"),
            cfg.node_count
        ));
        MplResult::Ok
    }));

    match result {
        Ok(code) => code,
        Err(error) => {
            INITED.store(false, Ordering::Release);
            engine.host().error(panic_message(error));
            MplResult::Runtime
        }
    }
}

#[no_mangle]
pub extern "C" fn micyou_plugin_deinit() {
    if !INITED.swap(false, Ordering::AcqRel) {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(engine) = ENGINE.get() else {
            return;
        };
        engine
            .host()
            .clear_interval(INTERVAL_ID.swap(0, Ordering::AcqRel));
        engine
            .host()
            .clear_timeout(TIMEOUT_ID.swap(0, Ordering::AcqRel));
        engine.shutdown();

        if let Some(handle) = lock(&WRITER).take() {
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
        engine.log("DSP Diagnose stopped");
    }));
}

#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_process(
    data: *mut f32,
    samples: u32,
    channels: u32,
    _queued_ms: f64,
    bypass: *mut u32,
) -> MplResult {
    if data.is_null() || bypass.is_null() {
        return MplResult::InvalidArg;
    }
    unsafe { *bypass = 0 };
    let Some(engine) = ENGINE.get() else {
        return MplResult::NotImplemented;
    };
    if !INITED.load(Ordering::Acquire) {
        return MplResult::Ok;
    }
    let slice = unsafe { std::slice::from_raw_parts(data, samples as usize) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        engine.process(slice, channels, None);
    }));
    MplResult::Ok
}

/// Optional extension export: the patched host calls this for every
/// `Plugin:<id>` occurrence and passes the 0-based occurrence index.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_process_node(
    data: *mut f32,
    samples: u32,
    channels: u32,
    _queued_ms: f64,
    node_index: u32,
    bypass: *mut u32,
) -> MplResult {
    if data.is_null() || bypass.is_null() {
        return MplResult::InvalidArg;
    }
    unsafe { *bypass = 0 };
    let Some(engine) = ENGINE.get() else {
        return MplResult::NotImplemented;
    };
    if !INITED.load(Ordering::Acquire) {
        return MplResult::Ok;
    }
    let slice = unsafe { std::slice::from_raw_parts(data, samples as usize) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        engine.process(slice, channels, Some(node_index as usize));
    }));
    MplResult::Ok
}

#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_handle_event(
    event_type: *const c_char,
    _json: *const c_char,
) -> MplResult {
    let Some(engine) = ENGINE.get() else {
        return MplResult::NotImplemented;
    };
    if !INITED.load(Ordering::Acquire) {
        return MplResult::Ok;
    }
    let Some(name) = (unsafe { cstr(event_type) }) else {
        return MplResult::InvalidArg;
    };

    let result = catch_unwind(AssertUnwindSafe(|| match name.as_str() {
        "dsp_settings_changed" => engine.on_dsp_settings_changed(),
        "state_changed" | "device_connected" | "device_disconnected" => {
            engine.publish_status(true)
        }
        _ => {}
    }));
    match result {
        Ok(()) => MplResult::Ok,
        Err(error) => {
            engine.host().error(panic_message(error));
            MplResult::Runtime
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_handle_message(
    _source: *const c_char,
    topic: *const c_char,
    payload: *const u8,
    payload_len: u32,
) -> MplResult {
    let Some(engine) = ENGINE.get() else {
        return MplResult::NotImplemented;
    };
    if !INITED.load(Ordering::Acquire) {
        return MplResult::Ok;
    }
    let Some(topic) = (unsafe { cstr(topic) }) else {
        return MplResult::InvalidArg;
    };
    let payload = if payload.is_null() || payload_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(payload, payload_len as usize) }
    };

    let result = catch_unwind(AssertUnwindSafe(|| dispatch_message(engine, &topic, payload)));
    match result {
        Ok(()) => MplResult::Ok,
        Err(error) => {
            engine.host().error(panic_message(error));
            MplResult::Runtime
        }
    }
}
