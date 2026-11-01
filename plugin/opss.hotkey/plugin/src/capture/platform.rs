//! Per-platform backend dispatch. Exactly one backend module is compiled in;
//! the shared layer in `capture/mod.rs` calls these four functions.
//!
//! * [`start`] — spawn the capture thread(s); availability is reported
//!   asynchronously via `report_status`.
//! * [`stop`] — stop and **join** every capture thread (must complete before
//!   the cdylib is unloaded).
//! * [`on_settings_updated`] — called after the shared settings snapshot
//!   changed (Windows reconciles its mouse hook on the hook thread).
//! * [`main_key_supported`] — status reporting helper.

#[cfg(windows)]
pub use super::windows::{main_key_supported, on_settings_updated, start, stop};

#[cfg(target_os = "macos")]
pub use super::macos::{main_key_supported, on_settings_updated, start, stop};

#[cfg(target_os = "linux")]
pub use super::linux::{main_key_supported, on_settings_updated, start, stop};

// Non-desktop targets (e.g. a bare `cargo check --target wasm32`): inert
// stub so the crate still compiles; the core surfaces "unavailable" and arms
// the host fallback.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub use stub::{main_key_supported, on_settings_updated, start, stop};

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod stub {
    pub fn start() {}
    pub fn stop() {}
    pub fn on_settings_updated() {}
    pub fn main_key_supported(_: crate::combo::MainKey) -> bool {
        false
    }
}
