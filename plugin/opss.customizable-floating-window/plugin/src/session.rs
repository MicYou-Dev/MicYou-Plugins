//! Connection / session-duration tracking for the floating window.
//!
//! "本次连接的时长" semantics: time elapsed since the *current* phone/web
//! connection was established.
//!
//! Primary source: `device_connected` / `device_disconnected` plugin events
//! (the host broadcasts them from the TCP control server — authoritative,
//! includes mode/label).
//!
//! Fallback: if the plugin is enabled *after* a device already connected (no
//! event will ever fire for that connection), we infer the session from the
//! `streaming` flag inside `audio_state`: a false→true transition starts an
//! "unknown device" session; streaming staying false for [`FALLBACK_END_TICKS`]
//! consecutive observations ends it (short transport hiccups don't reset the
//! clock). An authoritative `device_connected` event always restarts the
//! clock and upgrades the label; `device_disconnected` always ends it.

use std::time::Instant;

/// How many consecutive `streaming == false` observations end a *fallback*
/// session (only applies to sessions inferred from the streaming flag, not to
/// sessions opened by a real `device_connected` event). At the default 100 ms
/// tick this is ~3 s of grace.
const FALLBACK_END_TICKS: u32 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub connected: bool,
    pub seconds: u64,
    pub label: String,
    pub mode: String,
}

#[derive(Debug)]
pub struct Session {
    connected_at: Option<Instant>,
    label: String,
    mode: String,
    /// True when the session was inferred from `streaming` (no device event
    /// seen) — such sessions end on sustained `streaming == false`.
    inferred: bool,
    offline_ticks: u32,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub const fn new() -> Self {
        Self {
            connected_at: None,
            label: String::new(),
            mode: String::new(),
            inferred: false,
            offline_ticks: 0,
        }
    }

    /// Authoritative event: `device_connected { mode, label }`.
    pub fn device_connected(&mut self, mode: &str, label: &str) {
        self.connected_at = Some(Instant::now());
        self.label = label.to_string();
        self.mode = mode.to_string();
        self.inferred = false;
        self.offline_ticks = 0;
    }

    /// Authoritative event: `device_disconnected`.
    pub fn device_disconnected(&mut self) {
        self.connected_at = None;
        self.label.clear();
        self.mode.clear();
        self.inferred = false;
        self.offline_ticks = 0;
    }

    /// Per-tick observation of `audio_state.streaming` (fallback inference).
    pub fn observe_streaming(&mut self, streaming: bool) {
        if streaming {
            self.offline_ticks = 0;
            if self.connected_at.is_none() {
                self.connected_at = Some(Instant::now());
                self.inferred = true;
            }
        } else if let Some(_) = self.connected_at {
            if self.inferred {
                self.offline_ticks += 1;
                if self.offline_ticks >= FALLBACK_END_TICKS {
                    self.device_disconnected();
                }
            }
        } else {
            self.offline_ticks = 0;
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let seconds = self
            .connected_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        SessionSnapshot {
            connected: self.connected_at.is_some(),
            seconds,
            label: self.label.clone(),
            mode: self.mode.clone(),
        }
    }
}

/// Format a duration as `H:MM:SS` (hours only when non-zero) or `M:SS`.
pub fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "00:00");
        assert_eq!(format_duration(65), "01:05");
        assert_eq!(format_duration(3599), "59:59");
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(3600 * 5 + 61), "5:01:01");
    }

    #[test]
    fn event_driven_session() {
        let mut s = Session::new();
        assert!(!s.snapshot().connected);
        s.device_connected("wifi", "MicYou Mobile");
        assert!(s.snapshot().connected);
        // streaming hiccups must NOT end an authoritative session
        for _ in 0..100 {
            s.observe_streaming(false);
        }
        let snap = s.snapshot();
        assert!(snap.connected && snap.label == "MicYou Mobile" && snap.mode == "wifi");
        s.device_disconnected();
        assert!(!s.snapshot().connected);
        assert_eq!(s.snapshot().seconds, 0);
    }

    #[test]
    fn inferred_session_starts_and_ends_with_streaming() {
        let mut s = Session::new();
        s.observe_streaming(true);
        assert!(s.snapshot().connected);
        // grace window: a few false ticks don't end it
        for _ in 0..FALLBACK_END_TICKS - 1 {
            s.observe_streaming(false);
        }
        assert!(s.snapshot().connected);
        s.observe_streaming(false);
        assert!(!s.snapshot().connected);
    }

    #[test]
    fn event_upgrades_inferred_session() {
        let mut s = Session::new();
        s.observe_streaming(true);
        assert!(s.snapshot().label.is_empty());
        s.device_connected("usb", "Pixel");
        let snap = s.snapshot();
        assert_eq!(snap.label, "Pixel");
        // now authoritative: streaming=false no longer ends it
        for _ in 0..FALLBACK_END_TICKS * 2 {
            s.observe_streaming(false);
        }
        assert!(s.snapshot().connected);
    }
}
