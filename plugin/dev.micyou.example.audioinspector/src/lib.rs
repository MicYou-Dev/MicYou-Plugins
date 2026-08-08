//! # 音频状态监视器（MicYou 标准示例插件）
//!
//! 一个演示「标准写法」的插件，覆盖以下 Host API：
//! - `set_interval` 定时刷新（每 2 秒查询一次宿主状态）
//! - `audio_state` 音频流状态 / `connected_devices` 已连接设备
//! - `set_config` / `get_config` 状态持久化（面板轮询读取）
//! - `set_panel_icon` 设置设置侧边栏图标
//! - `log` 日志
//!
//! 面板通过 postMessage 桥轮询 `get_config`，把宿主状态实时显示出来
//! 面板文案用 `locale` API 跟随软件语言（中/英）

use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::Mutex;

// ---------- ABI 类型（与 include/micyou_plugin_abi.h 一致） ----------

#[repr(u32)]
#[derive(Clone, Copy, PartialEq)]
pub enum mpl_result_t {
    MPL_OK = 0,
    MPL_ERR_NOT_IMPLEMENTED = 1,
    MPL_ERR_INVALID_ARG = 2,
    MPL_ERR_RUNTIME = 3,
    MPL_ERR_BUFFER_TOO_SMALL = 4,
    MPL_ERR_PERMISSION = 5,
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub enum mpl_log_level_t {
    MPL_LOG_ERROR = 0,
    MPL_LOG_WARN = 1,
    MPL_LOG_INFO = 2,
    MPL_LOG_DEBUG = 3,
    MPL_LOG_TRACE = 4,
}

/// 宿主函数表：字段只能追加在 ctx 之后（append-only ABI）
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mpl_host_api_t {
    pub log: unsafe extern "C" fn(*mut c_void, mpl_log_level_t, *const c_char),
    pub get_config: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    pub set_config: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub emit_event: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub send_message: unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, u32) -> mpl_result_t,
    pub audio_state: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub connected_devices: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub ctx: *mut c_void,
    pub play_sound: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub plugin_dir: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub register_hotkey: unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64) -> mpl_result_t,
    pub open_window: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub fs_read: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    pub fs_write: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub set_timeout: unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t,
    pub clear_timeout: unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t,
    pub http_request: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, *const c_char, *mut u64) -> mpl_result_t,
    pub set_interval: unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t,
    pub clear_interval: unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t,
    pub open_url: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub notify: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub locale: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub host_info: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub clipboard_read: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub clipboard_write: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub set_panel_icon: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
}

/// ctx 是裸指针：静态里存宿主函数表需要 Send（宿主保证在线程间安全）
unsafe impl Send for mpl_host_api_t {}

#[repr(C)]
pub struct mpl_plugin_info_t {
    pub abi_version: u32,
    pub api_version: u32,
    pub id: *const c_char,
    pub version: *const c_char,
}

// ---------- 插件状态（静态，进程内唯一实例） ----------

static HOST: Mutex<Option<mpl_host_api_t>> = Mutex::new(None);
/// 面板查询用的 JSON 状态字符串（interval 每 2 秒更新）
static STATE_JSON: Mutex<String> = Mutex::new(String::new());
static INTERVAL_ID: AtomicU64 = AtomicU64::new(0);
static LOCALE: Mutex<String> = Mutex::new(String::new());
static LAST_MODE: AtomicI64 = AtomicI64::new(-1);

const INTERVAL_MS: u64 = 2000;

fn log_info(msg: &str) {
    if let Ok(host) = HOST.lock() {
        if let Some(h) = *host {
            if h.log as usize != 0 {
                let c = CString::new(msg).unwrap_or_default();
                unsafe { (h.log)(h.ctx, mpl_log_level_t::MPL_LOG_INFO, c.as_ptr()) };
            }
        }
    }
}

/// 调用宿主的字符串输出型 API（audio_state / connected_devices / locale）
fn host_string(api: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t) -> String {
    let host = match *HOST.lock().unwrap() {
        Some(h) => h,
        None => return String::new(),
    };
    if api as usize == 0 {
        return String::new();
    }
    // 先问大小，再取内容（out/out_size 契约）
    let mut size: u32 = 0;
    let code = unsafe { api(host.ctx, std::ptr::null_mut(), &mut size) };
    if code != mpl_result_t::MPL_ERR_BUFFER_TOO_SMALL || size == 0 {
        return String::new();
    }
    let mut buf = vec![0u8; size as usize + 1];
    let mut actual = size;
    let code = unsafe { api(host.ctx, buf.as_mut_ptr() as *mut c_char, &mut actual) };
    if code != mpl_result_t::MPL_OK {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// 刷新状态：audio_state + connected_devices → JSON → set_config("state")
fn refresh_state() {
    let host = match *HOST.lock().unwrap() {
        Some(h) => h,
        None => return,
    };
    let audio = host_string(host.audio_state);
    let devices = host_string(host.connected_devices);
    let state = format!(
        r#"{{"audio":{audio},"devices":{devices},"ts":{}}}"#,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    );
    if let Ok(mut s) = STATE_JSON.lock() {
        *s = state.clone();
    }
    // 持久化到配置，面板 get_config 轮询读取
    if host.set_config as usize != 0 {
        let key = CString::new("state").expect("nul-free");
        let val = CString::new(state).expect("nul-free");
        unsafe {
            (host.set_config)(host.ctx, key.as_ptr(), val.as_ptr());
        }
    }
}

/// 面板点击「刷新」时收到 handle_message
fn on_message(payload: &[u8]) {
    let text = String::from_utf8_lossy(payload);
    if text.contains("refresh") {
        refresh_state();
        log_info("audio-inspector: manual refresh");
    }
}

// ---------- 导出入口（catch_unwind 包裹，防跨 FFI UB） ----------

fn guard<F: FnOnce() -> mpl_result_t>(f: F) -> mpl_result_t {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => mpl_result_t::MPL_ERR_RUNTIME,
    }
}

#[no_mangle]
pub extern "C" fn micyou_plugin_info() -> mpl_plugin_info_t {
    mpl_plugin_info_t {
        abi_version: 1,
        api_version: 1,
        id: c"dev.micyou.example.audioinspector".as_ptr(),
        version: c"1.0.0".as_ptr(),
    }
}

#[no_mangle]
pub extern "C" fn micyou_plugin_init(host: *const mpl_host_api_t) -> mpl_result_t {
    guard(|| {
        if host.is_null() {
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        unsafe {
            *HOST.lock().unwrap() = Some(*host);

            // 侧边栏图标
            if (*host).set_panel_icon as usize != 0 {
                let pid = CString::new("inspector").expect("nul-free");
                let icon = CString::new("📊").expect("nul-free");
                ((*host).set_panel_icon)((*host).ctx, pid.as_ptr(), icon.as_ptr());
            }

            // 记住宿主语言（面板打开时读 get_config 里保存的 locale？不——
            // 面板直接调 locale API，这里只用于日志）
            let lang = host_string((*host).locale);
            *LOCALE.lock().unwrap() = lang;

            // 每 2 秒刷新一次状态
            if (*host).set_interval as usize != 0 {
                let payload = CString::new("").expect("nul-free");
                let mut id: u64 = 0;
                let code =
                    ((*host).set_interval)((*host).ctx, INTERVAL_MS, payload.as_ptr(), &mut id);
                if code == mpl_result_t::MPL_OK && id != 0 {
                    INTERVAL_ID.store(id, std::sync::atomic::Ordering::Relaxed);
                }
            }

            refresh_state();
            log_info("audio-inspector initialized");
        }
        mpl_result_t::MPL_OK
    })
}

#[no_mangle]
pub extern "C" fn micyou_plugin_deinit() -> mpl_result_t {
    guard(|| {
        let host = match *HOST.lock().unwrap() {
            Some(h) => h,
            None => return mpl_result_t::MPL_OK,
        };
        let id = INTERVAL_ID.load(std::sync::atomic::Ordering::Relaxed);
        if id != 0 && host.clear_interval as usize != 0 {
            unsafe {
                (host.clear_interval)(host.ctx, id);
            }
        }
        log_info("audio-inspector deinit");
        mpl_result_t::MPL_OK
    })
}

/// 面板按钮动作（ui:refresh 等）→ handle_message
#[no_mangle]
pub extern "C" fn micyou_plugin_handle_message(
    _source: *const c_char,
    _topic: *const c_char,
    payload: *const u8,
    payload_len: u32,
) -> mpl_result_t {
    guard(|| {
        if payload.is_null() || payload_len == 0 {
            return mpl_result_t::MPL_OK;
        }
        let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len as usize) };
        on_message(bytes);
        mpl_result_t::MPL_OK
    })
}
