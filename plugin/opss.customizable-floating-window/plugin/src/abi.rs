//! MicYou native plugin ABI bindings (mirrors `micyou_plugin_abi.h`, ABI v1 / Host API v2).
//!
//! Layout rules honored here (see MicYou docs/plugins/api-reference.md):
//! * The 7 function pointers before `ctx` are frozen — declared non-optional.
//! * Everything appended after `ctx` exists in the same order as the header.
//!   We declare the table **up to `set_dsp_settings`** because this plugin
//!   ships `apiVersion: 2` in its manifest: a host only loads us when its
//!   `HOST_API_VERSION >= 2`, which guarantees every field up to
//!   `set_dsp_settings` is present. (Reading past the end of a shorter host
//!   table would be out-of-bounds — that is exactly why the manifest declares
//!   v2: we *use* the v2 control-plane fields `set_muted` / `get_muted` /
//!   `set_monitoring` / `get_monitoring`.)
//! * Extension slots are still typed `Option<fn>` and null-checked, so a host
//!   that leaves an individual slot empty degrades gracefully instead of
//!   jumping to a null pointer.
//!
//! Threading contract (api-reference.md「使用注意事项」):
//! * The host pointer handed to `micyou_plugin_init` is only valid during the
//!   call — the struct is **copied by value** into [`HOST`].
//! * Host callbacks are ONLY invoked from host-dispatched contexts: `init`,
//!   `deinit`, `handle_event`, `handle_message` (incl. `interval:tick`).
//!   Our own threads (stdin writer / stdout reader / stderr tail for the
//!   helper process) NEVER touch the host table.
//! * This plugin is not a DSP node: it does not export `micyou_plugin_process`
//!   and never runs on the real-time audio thread.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Mutex;

pub const MPL_ABI_VERSION: u32 = 1;
pub const MPL_API_VERSION: u32 = 2;

/// Result codes returned by every plugin entry point (`mpl_result_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum mpl_result_t {
    MPL_OK = 0,
    MPL_ERR_NOT_IMPLEMENTED = 1,
    MPL_ERR_INVALID_ARG = 2,
    MPL_ERR_RUNTIME = 3,
    MPL_ERR_BUFFER_TOO_SMALL = 4,
    MPL_ERR_PERMISSION = 5,
}

/// Log levels (`mpl_log_level_t`), passed as plain i32 across the boundary.
pub const LOG_ERROR: i32 = 0;
pub const LOG_WARN: i32 = 1;
pub const LOG_INFO: i32 = 2;
pub const LOG_DEBUG: i32 = 3;

/// Host callback table (`mpl_host_api_t`). Field order MUST match the C header
/// byte-for-byte; new host fields are only ever appended after `ctx`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mpl_host_api_t {
    pub log: unsafe extern "C" fn(*mut c_void, i32, *const c_char),
    pub get_config:
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    pub set_config: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub emit_event: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub send_message:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, u32) -> mpl_result_t,
    pub audio_state: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub connected_devices: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub ctx: *mut c_void,
    // ── Appended extensions (defensively null-checked before use) ──
    pub play_sound: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub plugin_dir:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub register_hotkey:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64) -> mpl_result_t>,
    pub open_window: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub fs_read: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    >,
    pub fs_write:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t>,
    pub set_timeout:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t>,
    pub clear_timeout: Option<unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t>,
    pub http_request: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *mut u64,
        ) -> mpl_result_t,
    >,
    pub set_interval:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t>,
    pub clear_interval: Option<unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t>,
    pub open_url: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub notify:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t>,
    pub locale:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub host_info:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub clipboard_read:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub clipboard_write: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub set_panel_icon:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t>,
    // ── API Version 2: control plane (guaranteed present at apiVersion 2) ──
    pub set_muted: Option<unsafe extern "C" fn(*mut c_void, u32) -> mpl_result_t>,
    pub get_muted: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> mpl_result_t>,
    pub set_monitoring: Option<unsafe extern "C" fn(*mut c_void, u32) -> mpl_result_t>,
    pub get_monitoring: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> mpl_result_t>,
    pub get_dsp_settings:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub set_dsp_settings:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
}

// The table is a plain-old-data function-pointer bundle; the host keeps the
// backing `NativeHostCtx` alive for the whole plugin lifetime (Arc drop-guard
// on the host side), so it is safe to store and to move across the threads the
// host itself dispatches us on. We still only ever *call* it from
// host-dispatched threads (see module docs).
unsafe impl Send for mpl_host_api_t {}
unsafe impl Sync for mpl_host_api_t {}

/// Static plugin identity (`mpl_plugin_info_t`).
#[repr(C)]
pub struct mpl_plugin_info_t {
    pub abi_version: u32,
    pub api_version: u32,
    pub id: *const c_char,
    pub version: *const c_char,
}
unsafe impl Sync for mpl_plugin_info_t {}

/// By-value copy of the host table, stored during `init` (the pointer handed
/// to `init` must never be retained — api-reference.md §使用注意事项).
static HOST: Mutex<Option<mpl_host_api_t>> = Mutex::new(None);

pub fn store_host(host: mpl_host_api_t) {
    if let Ok(mut slot) = HOST.lock() {
        *slot = Some(host);
    }
}

pub fn clear_host() {
    if let Ok(mut slot) = HOST.lock() {
        *slot = None;
    }
}

/// Run `f` with the stored host table. Returns `None` before `init` / after
/// `deinit`, or when the (poisoned) lock is unavailable.
pub fn with_host<R>(f: impl FnOnce(&mpl_host_api_t) -> R) -> Option<R> {
    HOST.lock().ok().and_then(|slot| slot.as_ref().map(f))
}

// ── Thin typed wrappers (all follow the buffer contract) ───────────────────

pub fn log(level: i32, msg: &str) {
    with_host(|h| {
        if let Ok(c) = CString::new(msg) {
            unsafe { (h.log)(h.ctx, level, c.as_ptr()) };
        }
    });
}
pub fn log_info(msg: &str) {
    log(LOG_INFO, msg);
}
pub fn log_warn(msg: &str) {
    log(LOG_WARN, msg);
}
pub fn log_error(msg: &str) {
    log(LOG_ERROR, msg);
}

/// Read a host string via the out/out_size buffer contract, growing once when
/// the host reports `MPL_ERR_BUFFER_TOO_SMALL` (it then returns the required
/// size). Never panics; returns `None` on any failure.
///
/// IMPORTANT: takes the table **by value copy** — the `HOST` mutex is not
/// reentrant, so callback invocations must happen with the lock released.
fn read_host_string(
    table: &mpl_host_api_t,
    f: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let mut size = buf.len() as u32;
    let mut code = unsafe { f(table.ctx, buf.as_mut_ptr() as *mut c_char, &mut size) };
    if code == mpl_result_t::MPL_ERR_BUFFER_TOO_SMALL {
        // Host told us the required size; grow (with a sane cap) and retry.
        let need = (size as usize).min(4 * 1024 * 1024);
        buf.resize(need + 1, 0);
        size = buf.len() as u32;
        code = unsafe { f(table.ctx, buf.as_mut_ptr() as *mut c_char, &mut size) };
    }
    if code == mpl_result_t::MPL_OK {
        let len = (size as usize).min(buf.len().saturating_sub(1));
        Some(String::from_utf8_lossy(&buf[..len]).into_owned())
    } else {
        None
    }
}

/// Copy the stored host table (it is `Copy`), releasing the lock immediately.
fn table_copy() -> Option<mpl_host_api_t> {
    with_host(|h| *h)
}

/// `get_config(key)` -> raw JSON text of the value, or `None` when the key is
/// unset. (The host serializes the stored JSON value, so a string config
/// comes back quoted — callers parse with serde_json.)
pub fn get_config(key: &str) -> Option<String> {
    with_host(|h| {
        let k = CString::new(key).ok()?;
        let mut buf = vec![0u8; 16384];
        let mut size = buf.len() as u32;
        let mut code =
            unsafe { (h.get_config)(h.ctx, k.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut size) };
        if code == mpl_result_t::MPL_ERR_BUFFER_TOO_SMALL {
            let need = (size as usize).min(1024 * 1024);
            buf.resize(need + 1, 0);
            size = buf.len() as u32;
            code = unsafe {
                (h.get_config)(h.ctx, k.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut size)
            };
        }
        match code {
            mpl_result_t::MPL_OK => {
                let len = (size as usize).min(buf.len().saturating_sub(1));
                Some(String::from_utf8_lossy(&buf[..len]).into_owned())
            }
            // Key does not exist: the host writes *out_size = 0 and returns OK,
            // but be lenient about NOT_IMPLEMENTED as well.
            _ => None,
        }
    })
    .flatten()
}

/// `set_config(key, json_value)` — `json_value` must be valid JSON text.
pub fn set_config(key: &str, json_value: &str) -> bool {
    with_host(|h| {
        let (Ok(k), Ok(v)) = (CString::new(key), CString::new(json_value)) else {
            return false;
        };
        (unsafe { (h.set_config)(h.ctx, k.as_ptr(), v.as_ptr()) }) == mpl_result_t::MPL_OK
    })
    .unwrap_or(false)
}

/// `audio_state` snapshot (capability: audio.state). Parsed fields mirror
/// `AudioStateSnapshot` (camelCase JSON).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioState {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub channels: u32,
    #[serde(default)]
    pub input_level: f32,
    #[serde(default)]
    pub processed_level: f32,
    #[serde(default)]
    pub queued_ms: f64,
    #[serde(default)]
    pub muted: bool,
}

pub fn audio_state() -> Option<AudioState> {
    let table = table_copy()?;
    let raw = read_host_string(&table, table.audio_state)?;
    serde_json::from_str::<AudioState>(&raw).ok()
}

/// Absolute path of the plugin install directory (no capability required).
pub fn plugin_dir() -> Option<String> {
    let table = table_copy()?;
    read_host_string(&table, table.plugin_dir?)
}

/// Host identity JSON (`{"name":..,"version":..,"apiVersion":..}`), for logs.
pub fn host_info() -> Option<String> {
    let table = table_copy()?;
    read_host_string(&table, table.host_info?)
}

/// `set_interval(ms, payload)` -> timer id (0 = unavailable/failed).
/// Ticks arrive as `handle_message(topic = "interval:tick",
/// payload = {"interval":<id>,"payload":"<payload>"})`.
pub fn set_interval(ms: u64, payload: &str) -> u64 {
    with_host(|h| {
        let Some(f) = h.set_interval else { return 0 };
        let Ok(p) = CString::new(payload) else { return 0 };
        let mut id: u64 = 0;
        if unsafe { f(h.ctx, ms, p.as_ptr(), &mut id) } == mpl_result_t::MPL_OK {
            id
        } else {
            0
        }
    })
    .unwrap_or(0)
}

pub fn clear_interval(id: u64) {
    with_host(|h| {
        if id != 0 {
            if let Some(f) = h.clear_interval {
                unsafe { f(h.ctx, id) };
            }
        }
    });
}

/// `notify(title, body)` — no capability required.
pub fn notify(title: &str, body: &str) -> bool {
    with_host(|h| {
        let Some(f) = h.notify else { return false };
        let (Ok(t), Ok(b)) = (CString::new(title), CString::new(body)) else {
            return false;
        };
        (unsafe { f(h.ctx, t.as_ptr(), b.as_ptr()) }) == mpl_result_t::MPL_OK
    })
    .unwrap_or(false)
}

/// `set_panel_icon(panel_id, icon)` — best-effort cosmetics.
pub fn set_panel_icon(panel_id: &str, icon: &str) {
    with_host(|h| {
        let Some(f) = h.set_panel_icon else { return };
        if let (Ok(p), Ok(i)) = (CString::new(panel_id), CString::new(icon)) {
            unsafe { f(h.ctx, p.as_ptr(), i.as_ptr()) };
        }
    });
}

// ── API v2 control plane ───────────────────────────────────────────────────

/// `get_muted()` (capability: control.observe).
pub fn get_muted() -> Option<bool> {
    with_host(|h| {
        let f = h.get_muted?;
        let mut out: u32 = 0;
        (unsafe { f(h.ctx, &mut out) } == mpl_result_t::MPL_OK).then_some(out != 0)
    })
    .flatten()
}

/// `set_muted(muted)` (capability: control.intercept).
pub fn set_muted(muted: bool) -> bool {
    with_host(|h| {
        let Some(f) = h.set_muted else { return false };
        (unsafe { f(h.ctx, muted as u32) }) == mpl_result_t::MPL_OK
    })
    .unwrap_or(false)
}

/// `get_monitoring()` (capability: control.observe).
pub fn get_monitoring() -> Option<bool> {
    with_host(|h| {
        let f = h.get_monitoring?;
        let mut out: u32 = 0;
        (unsafe { f(h.ctx, &mut out) } == mpl_result_t::MPL_OK).then_some(out != 0)
    })
    .flatten()
}

/// `set_monitoring(enabled)` (capability: control.intercept).
pub fn set_monitoring(enabled: bool) -> bool {
    with_host(|h| {
        let Some(f) = h.set_monitoring else { return false };
        (unsafe { f(h.ctx, enabled as u32) }) == mpl_result_t::MPL_OK
    })
    .unwrap_or(false)
}

/// Read a NUL-terminated C string argument defensively (never panics).
pub unsafe fn cstr_or<'a>(p: *const c_char, fallback: &'a str) -> &'a str {
    if p.is_null() {
        return fallback;
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or(fallback)
}
