//! Plugin ⇄ helper IPC protocol (JSON lines over the helper's stdin/stdout).
//!
//! Both sides are **forward tolerant**: unknown fields are ignored by serde,
//! and both sides must ignore whole messages they cannot decode. That keeps a
//! newer helper working with an older plugin core and vice versa.
//!
//! Framing: one JSON object per line, UTF-8, `\n` terminated.
//! * plugin → helper: [`Cmd`] on the helper's **stdin**
//! * helper → plugin: [`Ev`] on the helper's **stdout** (the helper must keep
//!   stdout free of any other output — diagnostics go to stderr)

use serde::{Deserialize, Serialize};

/// plugin → helper command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Cmd {
    /// Live audio/connection state push (every host tick, ~10 Hz).
    State(StatePayload),
    /// Hot-swap the .slint theme file (absolute path). The helper recompiles
    /// and replaces the window content without restarting.
    Theme { path: String },
    /// Show/hide the window (process stays alive either way).
    Visible { show: bool },
    /// Move the window. Physical pixels.
    Pos { x: i32, y: i32 },
    /// Restore default placement (top-right of the primary screen).
    PosDefault,
    /// Render the window to a PNG file (diagnostics / tests / docs).
    Snapshot { path: String },
    /// WhatdidIsay (WDIS) transcription broadcast reached the plugin:
    /// show `text` in the theme's transcript area for `hold_ms`.
    Wdis { text: String, hold_ms: u64 },
    /// Clean shutdown.
    Quit,
}

/// One state frame. Field names on the wire are camelCase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatePayload {
    /// Raw input level (RMS 0..1) from `audio_state`.
    pub level: f64,
    /// Post-DSP level (RMS 0..1).
    pub processed: f64,
    pub muted: bool,
    pub streaming: bool,
    /// Host ear-return/monitoring state.
    pub monitoring: bool,
    pub sample_rate: u32,
    pub channels: u32,
    pub queued_ms: f64,
    /// Seconds since the current connection was established.
    pub session_seconds: u64,
    /// Pre-formatted `session_seconds` ("M:SS" / "H:MM:SS").
    pub session_text: String,
    /// Label/mode of the connected device (empty when unknown).
    pub device_label: String,
    pub device_mode: String,
}

/// helper → plugin event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "camelCase")]
pub enum Ev {
    /// Theme compiled and window shown. `reloaded` = hot-swap succeeded.
    Ready {
        theme: String,
        #[serde(default)]
        reloaded: bool,
    },
    /// Theme failed to compile/initialize; `message` holds the diagnostics.
    /// The helper keeps the previous theme alive when possible.
    ThemeError { message: String },
    /// User asked to toggle host mute.
    Mute,
    /// User asked to toggle host monitoring (ear-return).
    Monitoring,
    /// Theme-defined menu action string (e.g. `"reload-theme"`).
    Menu { action: String },
    /// User asked to hide the window (helper already hid it).
    Hide,
    /// Window finished moving; physical pixel position (persist it).
    Moved { x: i32, y: i32 },
    /// Free-form helper log line (surfaced into the plugin log).
    Log { message: String },
    /// Clean exit acknowledgment.
    Bye,
}

pub fn encode_cmd(cmd: &Cmd) -> String {
    serde_json::to_string(cmd).unwrap_or_else(|_| r#"{"cmd":"quit"}"#.into())
}

pub fn decode_cmd(line: &str) -> Option<Cmd> {
    serde_json::from_str::<Cmd>(line).ok()
}

pub fn encode_ev(ev: &Ev) -> String {
    serde_json::to_string(ev).unwrap_or_default()
}

pub fn decode_ev(line: &str) -> Option<Ev> {
    serde_json::from_str::<Ev>(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_roundtrip() {
        let c = Cmd::State(StatePayload {
            level: 0.5,
            muted: true,
            session_text: "01:02".into(),
            ..Default::default()
        });
        let line = encode_cmd(&c);
        assert!(line.contains(r#""cmd":"state""#));
        match decode_cmd(&line).unwrap() {
            Cmd::State(s) => {
                assert!(s.muted && s.level == 0.5 && s.session_text == "01:02");
            }
            _ => panic!("wrong variant"),
        }
        assert!(matches!(decode_cmd(r#"{"cmd":"quit"}"#).unwrap(), Cmd::Quit));
        assert!(decode_cmd(r#"{"cmd":"unknownThing"}"#).is_none());
        // forward tolerance: unknown fields ignored
        assert!(matches!(
            decode_cmd(r#"{"cmd":"visible","show":true,"future":42}"#).unwrap(),
            Cmd::Visible { show: true }
        ));
    }

    #[test]
    fn ev_roundtrip() {
        let line = encode_ev(&Ev::Menu {
            action: "reload-theme".into(),
        });
        assert!(line.contains(r#""ev":"menu""#));
        assert!(matches!(
            decode_ev(&line).unwrap(),
            Ev::Menu { ref action } if action == "reload-theme"
        ));
        assert!(matches!(decode_ev(r#"{"ev":"mute"}"#).unwrap(), Ev::Mute));
        assert!(decode_ev("not json").is_none());
    }
}
