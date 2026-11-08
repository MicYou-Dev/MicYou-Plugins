//! MicYou native plugin ABI (v1 / Host API v2).
//!
//! Only the subset needed by this plugin is wrapped, but the host table keeps
//! the exact field order of `micyou_plugin_abi.h` so offsets stay correct.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

pub const MPL_ABI_VERSION: u32 = 1;
pub const MPL_API_VERSION: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MplResult {
    Ok = 0,
    NotImplemented = 1,
    InvalidArg = 2,
    Runtime = 3,
    BufferTooSmall = 4,
    Permission = 5,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

/// Mirror of `mpl_host_api_t`.
///
/// Fields before `ctx` are frozen; newer callbacks live after it. Every
/// callback is `Option<fn>` so a plugin can detect hosts that predate a field
/// (the representation of `Option<extern "C" fn>` is a nullable pointer).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApi {
    pub log: Option<unsafe extern "C" fn(*mut c_void, LogLevel, *const c_char)>,
    pub get_config: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> MplResult,
    >,
    pub set_config:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> MplResult>,
    pub emit_event:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> MplResult>,
    pub send_message:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, u32) -> MplResult>,
    pub audio_state:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub connected_devices:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub ctx: *mut c_void,
    pub play_sound: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> MplResult>,
    pub plugin_dir:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub register_hotkey:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64) -> MplResult>,
    pub open_window: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> MplResult>,
    pub fs_read: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> MplResult,
    >,
    pub fs_write:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> MplResult>,
    pub set_timeout:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> MplResult>,
    pub clear_timeout: Option<unsafe extern "C" fn(*mut c_void, u64) -> MplResult>,
    pub http_request: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *mut u64,
        ) -> MplResult,
    >,
    pub set_interval:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> MplResult>,
    pub clear_interval: Option<unsafe extern "C" fn(*mut c_void, u64) -> MplResult>,
    pub open_url: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> MplResult>,
    pub notify: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> MplResult>,
    pub locale: Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub host_info: Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub clipboard_read:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub clipboard_write: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> MplResult>,
    pub set_panel_icon:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> MplResult>,
    pub set_muted: Option<unsafe extern "C" fn(*mut c_void, u32) -> MplResult>,
    pub get_muted: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> MplResult>,
    pub set_monitoring: Option<unsafe extern "C" fn(*mut c_void, u32) -> MplResult>,
    pub get_monitoring: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> MplResult>,
    pub get_dsp_settings:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> MplResult>,
    pub set_dsp_settings: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> MplResult>,
}

// The host table is only dereferenced on the plugin dispatch thread (and the
// audio thread uses in-process state only). The raw `ctx` pointer is owned by
// the host for the whole plugin lifetime.
unsafe impl Send for HostApi {}
unsafe impl Sync for HostApi {}

/// Thin safe wrapper around the host callback table.
pub struct Host {
    api: HostApi,
}

impl Host {
    pub fn new(api: HostApi) -> Self {
        Self { api }
    }

    pub fn log(&self, level: LogLevel, message: &str) {
        if let (Some(f), Ok(c)) = (self.api.log, CString::new(message)) {
            unsafe { f(self.api.ctx, level, c.as_ptr()) };
        }
    }

    pub fn info(&self, message: &str) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: &str) {
        self.log(LogLevel::Warn, message);
    }

    pub fn error(&self, message: &str) {
        self.log(LogLevel::Error, message);
    }

    pub fn get_config(&self, key: &str) -> Option<String> {
        let f = self.api.get_config?;
        let key = CString::new(key).ok()?;
        read_string(|out, size| unsafe { f(self.api.ctx, key.as_ptr(), out, size) })
    }

    pub fn set_config(&self, key: &str, json_value: &str) -> bool {
        let (Some(f), Ok(k), Ok(v)) = (
            self.api.set_config,
            CString::new(key),
            CString::new(json_value),
        ) else {
            return false;
        };
        unsafe { f(self.api.ctx, k.as_ptr(), v.as_ptr()) == MplResult::Ok }
    }

    pub fn get_dsp_settings(&self) -> Option<String> {
        let f = self.api.get_dsp_settings?;
        read_string(|out, size| unsafe { f(self.api.ctx, out, size) })
    }

    pub fn set_dsp_settings(&self, json_value: &str) -> bool {
        let (Some(f), Ok(v)) = (self.api.set_dsp_settings, CString::new(json_value)) else {
            return false;
        };
        unsafe { f(self.api.ctx, v.as_ptr()) == MplResult::Ok }
    }

    pub fn set_timeout(&self, ms: u64, payload: &str) -> u64 {
        let (Some(f), Ok(p)) = (self.api.set_timeout, CString::new(payload)) else {
            return 0;
        };
        let mut id = 0u64;
        unsafe { f(self.api.ctx, ms, p.as_ptr(), &mut id) };
        id
    }

    pub fn set_interval(&self, ms: u64, payload: &str) -> u64 {
        let (Some(f), Ok(p)) = (self.api.set_interval, CString::new(payload)) else {
            return 0;
        };
        let mut id = 0u64;
        unsafe { f(self.api.ctx, ms, p.as_ptr(), &mut id) };
        id
    }

    pub fn clear_timeout(&self, id: u64) -> bool {
        let Some(f) = self.api.clear_timeout else {
            return false;
        };
        unsafe { f(self.api.ctx, id) == MplResult::Ok }
    }

    pub fn clear_interval(&self, id: u64) -> bool {
        let Some(f) = self.api.clear_interval else {
            return false;
        };
        unsafe { f(self.api.ctx, id) == MplResult::Ok }
    }

    pub fn open_url(&self, url: &str) -> bool {
        let (Some(f), Ok(u)) = (self.api.open_url, CString::new(url)) else {
            return false;
        };
        unsafe { f(self.api.ctx, u.as_ptr()) == MplResult::Ok }
    }

    pub fn notify(&self, title: &str, body: &str) {
        let (Some(f), Ok(t), Ok(b)) = (
            self.api.notify,
            CString::new(title),
            CString::new(body),
        ) else {
            return;
        };
        unsafe { f(self.api.ctx, t.as_ptr(), b.as_ptr()) };
    }

    pub fn set_panel_icon(&self, panel_id: &str, icon: &str) {
        let (Some(f), Ok(p), Ok(i)) = (
            self.api.set_panel_icon,
            CString::new(panel_id),
            CString::new(icon),
        ) else {
            return;
        };
        unsafe { f(self.api.ctx, p.as_ptr(), i.as_ptr()) };
    }
}

/// Host-capability check without calling into the host.
pub fn has_fn(f: Option<unsafe extern "C" fn()>) -> bool {
    f.is_some()
}

/// Implements the `out` / `out_size` buffer contract.
fn read_string<F>(mut call: F) -> Option<String>
where
    F: FnMut(*mut c_char, *mut u32) -> MplResult,
{
    let mut buf = vec![0u8; 8192];
    let mut size = buf.len() as u32;
    let mut code = call(buf.as_mut_ptr() as *mut c_char, &mut size);

    if code == MplResult::BufferTooSmall {
        let required = size as usize;
        if required == 0 || required > 16 * 1024 * 1024 {
            return None;
        }
        buf.resize(required + 1, 0);
        size = buf.len() as u32;
        code = call(buf.as_mut_ptr() as *mut c_char, &mut size);
    }

    if code != MplResult::Ok {
        return None;
    }
    let len = (size as usize).min(buf.len());
    let bytes = &buf[..len];
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec()).ok()
}

/// Read a NUL-terminated C string, tolerating NULL.
pub unsafe fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

pub fn null_ptr<T>() -> *const T {
    ptr::null()
}
