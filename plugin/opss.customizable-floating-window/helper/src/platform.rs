//! Platform window plumbing that Slint does not model.
//!
//! # Windows taskbar — root cause & mechanism (round-9 fix)
//! Evidence chain:
//!   * slint-ui/slint discussion #3266: a user set `WS_EX_TOOLWINDOW` via
//!     `SetWindowLongPtr` **after show** and it did NOT remove the taskbar
//!     button; maintainers confirmed no out-of-the-box Slint API.
//!   * winit 0.30 source (`platform_impl/windows`): winit's own
//!     `set_skip_taskbar()` does **not** touch extended styles — it calls
//!     `ITaskbarList::DeleteTab(hwnd)` (COM). Separately, winit's
//!     `WindowFlags::apply_diff()` recomputes `GWL_STYLE`/`GWL_EXSTYLE` from
//!     its internal flags whenever any flag diffs (e.g. VISIBLE at show),
//!     adding `WS_EX_APPWINDOW` (ON_TASKBAR) — so externally-set exstyle bits
//!     are wiped at show, and the shell creates the taskbar button at show.
//! Hence the only robust levers, both used here (idempotent, re-assertable):
//!   1. `ITaskbarList::DeleteTab(hwnd)` — removes the existing button any time
//!      after show (winit's sanctioned mechanism; immune to style rewrites);
//!   2. `WS_EX_TOOLWINDOW` set / `WS_EX_APPWINDOW` cleared — stops the shell
//!      from re-adding the button (explorer restart) and drops Alt-Tab entry.
//! Deliberately NOT used (proved harmful/pointless in rounds 3–4):
//!   * `ShowWindow(SW_HIDE/SW_SHOW)` cycles (desync winit visibility state and
//!     the DWM transparent surface → grey partial repaints);
//!   * caption/style stripping (winit recreates styles from `no-frame` at
//!     creation; later strips get wiped by apply_diff).
//!
//! # X11 / macOS / Wayland
//! X11: `_NET_WM_STATE_SKIP_TASKBAR/+SKIP_PAGER` (WM-managed, persists) plus
//! `_MOTIF_WM_HINTS` decorations=0 for CSD shells. macOS: process-level
//! `NSApplicationActivationPolicyAccessory` (no Dock / Cmd-Tab). Wayland:
//! compositor policy → no-op.

use slint::Window;

// ─────────────────────────── dispatch ───────────────────────────

/// Apply the platform's taskbar/alt-tab/dock exclusion. Idempotent; safe to
/// call repeatedly (the UI driver re-asserts every ~30 ticks on Windows).
pub fn apply_skip_taskbar(window: &Window) -> &'static str {
    #[cfg(windows)]
    return windows_skip_taskbar(window);
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    return linux_skip_taskbar(window);
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        macos_set_accessory_policy();
        "macos-accessory-policy"
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = window;
        "unsupported-platform"
    }
}

/// Primary screen size in physical pixels (None → keep WM placement).
pub fn screen_size() -> Option<(i32, i32)> {
    #[cfg(windows)]
    {
        unsafe {
            let w = win::GetSystemMetrics(win::SM_CXSCREEN);
            let h = win::GetSystemMetrics(win::SM_CYSCREEN);
            (w > 0 && h > 0).then_some((w, h))
        }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe {
            let id = CGMainDisplayID();
            let w = CGDisplayPixelsWide(id) as i32;
            let h = CGDisplayPixelsHigh(id) as i32;
            (w > 0 && h > 0).then_some((w, h))
        }
    }
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        if is_wayland() {
            return None;
        }
        x11_screen_size()
    }
}

/// Whether absolute window positioning / global cursor work (false: Wayland).
pub fn position_api_usable() -> bool {
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        !is_wayland()
    }
    #[cfg(not(all(target_os = "linux", not(target_os = "android"))))]
    {
        true
    }
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none()
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

// ─────────────────────────── Windows ───────────────────────────

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    pub type HWND = *mut c_void;
    pub const GWL_EXSTYLE: i32 = -20;
    pub const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    pub const WS_EX_APPWINDOW: isize = 0x0004_0000;
    pub const SM_CXSCREEN: i32 = 0;
    pub const SM_CYSCREEN: i32 = 1;

    #[repr(C)]
    pub struct Guid {
        pub d1: u32,
        pub d2: u16,
        pub d3: u16,
        pub d4: [u8; 8],
    }
    /// ITaskbarList vtable: IUnknown (3) + ITaskbarList (5), COM ABI order.
    #[repr(C)]
    pub struct ITaskbarListVtbl {
        pub query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        pub release: unsafe extern "system" fn(*mut c_void) -> u32,
        pub hr_init: unsafe extern "system" fn(*mut c_void) -> i32,
        pub add_tab: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
        pub delete_tab: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
        pub mark_fullscreen: unsafe extern "system" fn(*mut c_void, HWND, i32) -> i32,
        pub set_active_alt: unsafe extern "system" fn(*mut c_void, HWND, i32) -> i32,
    }
    #[repr(C)]
    pub struct ITaskbarList {
        pub vtbl: *const ITaskbarListVtbl,
    }
    pub const CLSID_TASKBAR_LIST: Guid = Guid {
        d1: 0x56FD_F344,
        d2: 0xFD6D,
        d3: 0x11D0,
        d4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
    };
    pub const IID_ITASKBAR_LIST: Guid = Guid {
        d1: 0x56FD_F342,
        d2: 0xFD6D,
        d3: 0x11D0,
        d4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
    };
    pub const CLSCTX_ALL: u32 = 0x17;
    pub const COINIT_APARTMENTTHREADED: u32 = 0x2;
    pub const S_OK: i32 = 0;

    #[link(name = "user32")]
    extern "system" {
        pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
        pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, new_long: isize) -> isize;
        pub fn GetSystemMetrics(index: i32) -> i32;
    }
    #[link(name = "ole32")]
    extern "system" {
        pub fn CoInitializeEx(pv: *mut c_void, coinit: u32) -> i32;
        pub fn CoCreateInstance(
            rclsid: *const Guid,
            outer: *mut c_void,
            ctx: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
    }
}

/// Process-lifetime cached ITaskbarList (always the same UI thread → the
/// apartment the interface was created in).
#[cfg(windows)]
static TASKBAR_LIST: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Cached overlay HWND once acquired (re-assert path).
#[cfg(windows)]
static OVERLAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// `ITaskbarList::DeleteTab(hwnd)` — the exact mechanism winit's own
/// `set_skip_taskbar` uses. Returns false when COM/CoCreate fails.
#[cfg(windows)]
fn taskbar_delete_tab(hwnd: win::HWND) -> bool {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    unsafe {
        let mut p = TASKBAR_LIST.load(Ordering::Relaxed) as *mut win::ITaskbarList;
        if p.is_null() {
            // COM init is ref-counted; S_FALSE (already initialized) is fine.
            win::CoInitializeEx(std::ptr::null_mut(), win::COINIT_APARTMENTTHREADED);
            let mut raw: *mut c_void = std::ptr::null_mut();
            let hr = win::CoCreateInstance(
                &win::CLSID_TASKBAR_LIST,
                std::ptr::null_mut(),
                win::CLSCTX_ALL,
                &win::IID_ITASKBAR_LIST,
                &mut raw,
            );
            if hr != win::S_OK || raw.is_null() {
                return false;
            }
            p = raw as *mut win::ITaskbarList;
            let vt = &*(*p).vtbl;
            if (vt.hr_init)(p as *mut c_void) != win::S_OK {
                return false;
            }
            TASKBAR_LIST.store(p as isize, Ordering::Relaxed);
        }
        let vt = &*(*p).vtbl;
        (vt.delete_tab)(p as *mut c_void, hwnd) == win::S_OK
    }
}

/// Idempotent Windows exclusion: DeleteTab + TOOLWINDOW/¬APPWINDOW.
#[cfg(windows)]
fn windows_skip_taskbar(window: &Window) -> &'static str {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use std::sync::atomic::Ordering;
    let mut hwnd = OVERLAY_HWND.load(Ordering::Relaxed) as win::HWND;
    if hwnd.is_null() {
        let wh = window.window_handle();
        if let Ok(handle) = wh.window_handle() {
            if let RawWindowHandle::Win32(w) = handle.as_raw() {
                hwnd = w.hwnd.get() as win::HWND;
                if !hwnd.is_null() {
                    OVERLAY_HWND.store(hwnd as isize, Ordering::Relaxed);
                }
            }
        }
    }
    if hwnd.is_null() {
        return "no-handle";
    }
    let ok = taskbar_delete_tab(hwnd);
    unsafe {
        let ex = win::GetWindowLongPtrW(hwnd, win::GWL_EXSTYLE);
        win::SetWindowLongPtrW(
            hwnd,
            win::GWL_EXSTYLE,
            (ex | win::WS_EX_TOOLWINDOW) & !win::WS_EX_APPWINDOW,
        );
    }
    if ok {
        "taskbar: ITaskbarList::DeleteTab + WS_EX_TOOLWINDOW"
    } else {
        "taskbar: DeleteTab unavailable (CoCreateInstance failed); exstyle only"
    }
}

// ─────────────────────────── Linux / X11 ───────────────────────────

#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn x11_screen_size() -> Option<(i32, i32)> {
    let xlib = x11_dl::xlib::Xlib::open().ok()?;
    unsafe {
        let disp = (xlib.XOpenDisplay)(std::ptr::null());
        if disp.is_null() {
            return None;
        }
        let screen = (xlib.XDefaultScreen)(disp);
        let w = (xlib.XDisplayWidth)(disp, screen);
        let h = (xlib.XDisplayHeight)(disp, screen);
        (xlib.XCloseDisplay)(disp);
        (w > 0 && h > 0).then_some((w, h))
    }
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn linux_skip_taskbar(window: &Window) -> &'static str {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    if is_wayland() {
        return "wayland-noop";
    }
    let wh = window.window_handle();
    let Ok(handle) = wh.window_handle() else {
        return "no-handle";
    };
    let RawWindowHandle::Xlib(h) = handle.as_raw() else {
        return "not-x11";
    };
    let win_id = h.window as x11_dl::xlib::Window;
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
        return "no-xlib";
    };
    unsafe {
        let disp = (xlib.XOpenDisplay)(std::ptr::null());
        if disp.is_null() {
            return "no-display";
        }
        let state_atom = intern(&xlib, disp, b"_NET_WM_STATE\0");
        let skip_taskbar = intern(&xlib, disp, b"_NET_WM_STATE_SKIP_TASKBAR\0");
        let skip_pager = intern(&xlib, disp, b"_NET_WM_STATE_SKIP_PAGER\0");
        if state_atom == 0 || skip_taskbar == 0 || skip_pager == 0 {
            (xlib.XCloseDisplay)(disp);
            return "atom-error";
        }
        const XA_ATOM: x11_dl::xlib::Atom = 4;
        let mut actual_type: x11_dl::xlib::Atom = 0;
        let mut actual_format: std::os::raw::c_int = 0;
        let mut nitems: std::os::raw::c_ulong = 0;
        let mut bytes_after: std::os::raw::c_ulong = 0;
        let mut prop: *mut std::os::raw::c_uchar = std::ptr::null_mut();
        (xlib.XGetWindowProperty)(
            disp,
            win_id,
            state_atom,
            0,
            64,
            0,
            XA_ATOM,
            &mut actual_type,
            &mut actual_format,
            &mut nitems,
            &mut bytes_after,
            &mut prop,
        );
        let mut atoms: Vec<x11_dl::xlib::Atom> = Vec::new();
        if !prop.is_null() {
            if actual_type == XA_ATOM && actual_format == 32 && nitems > 0 {
                atoms.extend_from_slice(std::slice::from_raw_parts(
                    prop as *const x11_dl::xlib::Atom,
                    nitems as usize,
                ));
            }
            (xlib.XFree)(prop as *mut std::os::raw::c_void);
        }
        for a in [skip_taskbar, skip_pager] {
            if !atoms.contains(&a) {
                atoms.push(a);
            }
        }
        (xlib.XChangeProperty)(
            disp,
            win_id,
            state_atom,
            XA_ATOM,
            32,
            0,
            atoms.as_ptr() as *const std::os::raw::c_uchar,
            atoms.len() as std::os::raw::c_int,
        );
        let motif = intern(&xlib, disp, b"_MOTIF_WM_HINTS\0");
        if motif != 0 {
            #[repr(C)]
            struct MotifHints {
                flags: u64,
                functions: u64,
                decorations: u64,
                input_mode: i64,
                status: u64,
            }
            let hints = MotifHints {
                flags: 1 << 1,
                functions: 0,
                decorations: 0,
                input_mode: 0,
                status: 0,
            };
            (xlib.XChangeProperty)(
                disp,
                win_id,
                motif,
                motif,
                32,
                0,
                &hints as *const MotifHints as *const std::os::raw::c_uchar,
                5,
            );
        }
        (xlib.XFlush)(disp);
        (xlib.XCloseDisplay)(disp);
    }
    "skip-taskbar-x11"
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
unsafe fn intern(
    xlib: &x11_dl::xlib::Xlib,
    disp: *mut x11_dl::xlib::Display,
    name: &'static [u8],
) -> x11_dl::xlib::Atom {
    unsafe {
        (xlib.XInternAtom)(
            disp,
            name.as_ptr() as *const std::os::raw::c_char,
            0,
        )
    }
}

// ─────────────────────────── macOS ─────────────────────────────

#[cfg(target_os = "macos")]
use std::ffi::{c_char, c_void};

#[cfg(target_os = "macos")]
type MsgSend0 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
#[cfg(target_os = "macos")]
type MsgSendI = unsafe extern "C" fn(*mut c_void, *mut c_void, isize);

#[cfg(target_os = "macos")]
#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
}

/// macOS: hide from Dock / Cmd-Tab via process-level Accessory policy.
#[cfg(target_os = "macos")]
pub fn macos_set_accessory_policy() {
    use std::ffi::CString;
    const ACCESSORY: isize = 1;
    unsafe {
        let Ok(cls_name) = CString::new("NSApplication") else { return };
        let Ok(sel_shared) = CString::new("sharedApplication") else { return };
        let Ok(sel_policy) = CString::new("setActivationPolicy:") else { return };
        let cls = objc_getClass(cls_name.as_ptr());
        if cls.is_null() {
            return;
        }
        let sel_shared = sel_registerName(sel_shared.as_ptr());
        let sel_policy = sel_registerName(sel_policy.as_ptr());
        if sel_shared.is_null() || sel_policy.is_null() {
            return;
        }
        let msg0: MsgSend0 = std::mem::transmute(objc_msgSend as *const c_void);
        let app = msg0(cls, sel_shared);
        if app.is_null() {
            return;
        }
        let msg1: MsgSendI = std::mem::transmute(objc_msgSend as *const c_void);
        msg1(app, sel_policy, ACCESSORY);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn macos_set_accessory_policy() {}

// ─────────────────────────── global cursor ───────────────────────────

/// Global cursor position in **physical pixels** for drag tracking.
/// None where unavailable (Wayland): WindowMoveArea themes remain the
/// recommended pattern there.
#[cfg(windows)]
pub fn global_cursor(_scale: f32) -> Option<(f64, f64)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(lp: *mut Point) -> i32;
    }
    unsafe {
        let mut p = Point { x: 0, y: 0 };
        if GetCursorPos(&mut p) != 0 {
            Some((p.x as f64, p.y as f64))
        } else {
            None
        }
    }
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn global_cursor(_scale: f32) -> Option<(f64, f64)> {
    use x11_dl::xlib;
    if is_wayland() {
        return None;
    }
    thread_local! {
        static DISP: std::cell::Cell<Option<*mut xlib::Display>> = const { std::cell::Cell::new(None) };
    }
    let Ok(xlib) = xlib::Xlib::open() else { return None };
    let disp = DISP.with(|d| {
        if let Some(p) = d.get() {
            return p;
        }
        let p = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        d.set(Some(p));
        p
    });
    if disp.is_null() {
        return None;
    }
    unsafe {
        let screen = (xlib.XDefaultScreen)(disp);
        let root = (xlib.XRootWindow)(disp, screen);
        let mut root_ret: xlib::Window = 0;
        let mut child: xlib::Window = 0;
        let mut rx: std::os::raw::c_int = 0;
        let mut ry: std::os::raw::c_int = 0;
        let mut wx: std::os::raw::c_int = 0;
        let mut wy: std::os::raw::c_int = 0;
        let mut mask: std::os::raw::c_uint = 0;
        if (xlib.XQueryPointer)(
            disp, root, &mut root_ret, &mut child, &mut rx, &mut ry, &mut wx, &mut wy, &mut mask,
        ) == 0
        {
            return None;
        }
        Some((rx as f64, ry as f64))
    }
}

#[cfg(target_os = "macos")]
pub fn global_cursor(scale: f32) -> Option<(f64, f64)> {
    use std::ffi::CString;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }
    type MsgSendPoint = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NsPoint;
    unsafe {
        let cls_name = CString::new("NSEvent").ok()?;
        let sel_name = CString::new("mouseLocation").ok()?;
        let cls = objc_getClass(cls_name.as_ptr());
        let sel = sel_registerName(sel_name.as_ptr());
        if cls.is_null() || sel.is_null() {
            return None;
        }
        let msg: MsgSendPoint = std::mem::transmute(objc_msgSend as *const c_void);
        let p = msg(cls, sel); // points, origin bottom-left
        let s = f64::from(scale.max(0.5));
        let h_px = CGDisplayPixelsHigh(CGMainDisplayID()) as f64;
        Some((p.x * s, (h_px / s - p.y) * s))
    }
}
