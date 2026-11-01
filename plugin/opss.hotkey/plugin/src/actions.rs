//! Hotkey action engine — pure, platform-independent binding semantics.
//!
//! The capture backends only report neutral `(Slot, Edge)` events; every
//! decision (what the host should do, what state to remember) lives here so
//! it can be unit-tested without any OS input APIs.
//!
//! Push-to-talk / push-to-mute semantics:
//! * **press** latches the *previous* mute state and flips it (PTT → unmute,
//!   PTM → mute). If the mic is already in the target state, the press is a
//!   no-op passthrough (pressing PTT while already talking must not mute you
//!   on release).
//! * **release** restores the latched state — the classic "let go = muted"
//!   behaviour when starting from muted, without surprising users who were
//!   freely talking before the hold.
//! * Toggle actions fire on the press edge only; release is ignored.

use serde::Serialize;

/// Binding slots. The discriminants index the capture settings array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Slot {
    /// Toggle host mute on each press.
    MuteToggle = 0,
    /// Toggle host monitoring (ear-return) on each press.
    MonitorToggle = 1,
    /// Hold to talk: unmute while held, restore on release.
    PushToTalk = 2,
    /// Hold to mute: mute while held, restore on release.
    PushToMute = 3,
}

pub const SLOT_COUNT: usize = 4;

impl Slot {
    pub const ALL: [Slot; SLOT_COUNT] = [
        Slot::MuteToggle,
        Slot::MonitorToggle,
        Slot::PushToTalk,
        Slot::PushToMute,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Stable machine name used in config, panel payloads and status JSON.
    pub fn name(self) -> &'static str {
        match self {
            Slot::MuteToggle => "mute",
            Slot::MonitorToggle => "monitor",
            Slot::PushToTalk => "ptt",
            Slot::PushToMute => "ptm",
        }
    }

    pub fn parse(name: &str) -> Option<Slot> {
        Slot::ALL.into_iter().find(|s| s.name() == name)
    }

    /// True for the two hold-style bindings (they need key-release events,
    /// which the host `register_hotkey` fallback cannot provide).
    pub fn is_hold(self) -> bool {
        matches!(self, Slot::PushToTalk | Slot::PushToMute)
    }
}

/// Neutral press/release transition reported by a capture backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Press,
    Release,
}

/// A concrete host control-plane command produced by the engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(tag = "cmd", content = "value")]
pub enum Command {
    SetMuted(bool),
    SetMonitoring(bool),
}

/// Binding semantics + cached host state.
///
/// The cached `muted` / `monitoring` are kept in sync from three sources:
/// optimistic application of issued [`Command`]s, host `mute_changed` /
/// `monitoring_changed` events (authoritative), and explicit re-reads.
#[derive(Debug)]
pub struct Engine {
    muted: bool,
    monitoring: bool,
    /// Some(previous) while a PTT hold that actually flipped mute is active.
    ptt_latch: Option<bool>,
    /// Same for push-to-mute.
    ptm_latch: Option<bool>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(false, false)
    }
}

impl Engine {
    pub fn new(muted: bool, monitoring: bool) -> Self {
        Self {
            muted,
            monitoring,
            ptt_latch: None,
            ptm_latch: None,
        }
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn monitoring(&self) -> bool {
        self.monitoring
    }

    pub fn ptt_active(&self) -> bool {
        self.ptt_latch.is_some()
    }

    pub fn ptm_active(&self) -> bool {
        self.ptm_latch.is_some()
    }

    /// Host reported a new authoritative mute state (`mute_changed` event or
    /// an explicit `get_muted`). If an external change ends a hold's premise
    /// the latch stays: release still restores the *original* state, which
    /// is exactly what an explicit hold promises.
    pub fn sync_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn sync_monitoring(&mut self, monitoring: bool) {
        self.monitoring = monitoring;
    }

    /// Feed one edge; returns the host commands to execute (usually 0 or 1).
    pub fn on_edge(&mut self, slot: Slot, edge: Edge) -> Vec<Command> {
        match (slot, edge) {
            (Slot::MuteToggle, Edge::Press) => {
                self.muted = !self.muted;
                vec![Command::SetMuted(self.muted)]
            }
            (Slot::MonitorToggle, Edge::Press) => {
                self.monitoring = !self.monitoring;
                vec![Command::SetMonitoring(self.monitoring)]
            }
            (Slot::PushToTalk, Edge::Press) => {
                if self.ptt_latch.is_some() {
                    return vec![]; // already holding (auto-repeat etc.)
                }
                if self.muted {
                    self.ptt_latch = Some(true);
                    self.muted = false;
                    vec![Command::SetMuted(false)]
                } else {
                    self.ptt_latch = None; // passthrough: already talking
                    vec![]
                }
            }
            (Slot::PushToTalk, Edge::Release) => {
                if self.ptt_latch.take().is_some() {
                    self.muted = true;
                    vec![Command::SetMuted(true)]
                } else {
                    vec![]
                }
            }
            (Slot::PushToMute, Edge::Press) => {
                if self.ptm_latch.is_some() {
                    return vec![];
                }
                if !self.muted {
                    self.ptm_latch = Some(false);
                    self.muted = true;
                    vec![Command::SetMuted(true)]
                } else {
                    self.ptm_latch = None; // passthrough: already muted
                    vec![]
                }
            }
            (Slot::PushToMute, Edge::Release) => {
                if self.ptm_latch.take().is_some() {
                    self.muted = false;
                    vec![Command::SetMuted(false)]
                } else {
                    vec![]
                }
            }
            // Toggles ignore release.
            (_, Edge::Release) => vec![],
        }
    }

    /// Commands needed to leave a clean state on shutdown / reload: an active
    /// hold must never leave the mic hot (PTT) or dead (PTM) after the plugin
    /// goes away.
    pub fn shutdown_restore(&mut self) -> Vec<Command> {
        let mut out = Vec::new();
        if self.ptt_latch.take().is_some() {
            self.muted = true;
            out.push(Command::SetMuted(true));
        }
        if self.ptm_latch.take().is_some() {
            self.muted = false;
            out.push(Command::SetMuted(false));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_mute_flips_each_press() {
        let mut e = Engine::new(false, false);
        assert_eq!(e.on_edge(Slot::MuteToggle, Edge::Press), vec![Command::SetMuted(true)]);
        assert_eq!(e.on_edge(Slot::MuteToggle, Edge::Release), vec![]);
        assert_eq!(e.on_edge(Slot::MuteToggle, Edge::Press), vec![Command::SetMuted(false)]);
        assert!(e.muted() == false);
    }

    #[test]
    fn toggle_monitoring_flips_each_press() {
        let mut e = Engine::new(false, false);
        assert_eq!(
            e.on_edge(Slot::MonitorToggle, Edge::Press),
            vec![Command::SetMonitoring(true)]
        );
        assert_eq!(
            e.on_edge(Slot::MonitorToggle, Edge::Press),
            vec![Command::SetMonitoring(false)]
        );
    }

    #[test]
    fn ptt_from_muted_unmutes_then_remutes() {
        let mut e = Engine::new(true, false);
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Press), vec![Command::SetMuted(false)]);
        assert!(e.ptt_active());
        // Auto-repeat / redundant press while held: no-op.
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Press), vec![]);
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Release), vec![Command::SetMuted(true)]);
        assert!(!e.ptt_active());
        // Second release: no-op.
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Release), vec![]);
        assert!(e.muted());
    }

    #[test]
    fn ptt_passthrough_when_already_talking() {
        let mut e = Engine::new(false, false);
        // Already unmuted: press must NOT latch, release must NOT mute.
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Press), vec![]);
        assert!(!e.ptt_active());
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Release), vec![]);
        assert!(!e.muted());
    }

    #[test]
    fn ptm_from_unmuted_mutes_then_restores() {
        let mut e = Engine::new(false, false);
        assert_eq!(e.on_edge(Slot::PushToMute, Edge::Press), vec![Command::SetMuted(true)]);
        assert!(e.ptm_active());
        assert_eq!(e.on_edge(Slot::PushToMute, Edge::Release), vec![Command::SetMuted(false)]);
        assert!(!e.muted());
    }

    #[test]
    fn ptm_passthrough_when_already_muted() {
        let mut e = Engine::new(true, false);
        assert_eq!(e.on_edge(Slot::PushToMute, Edge::Press), vec![]);
        assert_eq!(e.on_edge(Slot::PushToMute, Edge::Release), vec![]);
        assert!(e.muted());
    }

    #[test]
    fn external_sync_updates_cache_without_disturbing_latches() {
        let mut e = Engine::new(true, false);
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Press), vec![Command::SetMuted(false)]);
        // Host event confirms (or races) the new state:
        e.sync_muted(false);
        assert!(!e.muted());
        // GUI user mutes mid-hold; release still restores the hold promise.
        e.sync_muted(true);
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Release), vec![Command::SetMuted(true)]);
    }

    #[test]
    fn shutdown_restore_releases_active_holds() {
        let mut e = Engine::new(true, false);
        let _ = e.on_edge(Slot::PushToTalk, Edge::Press);
        assert_eq!(e.shutdown_restore(), vec![Command::SetMuted(true)]);

        let mut e2 = Engine::new(false, false);
        let _ = e2.on_edge(Slot::PushToMute, Edge::Press);
        assert_eq!(e2.shutdown_restore(), vec![Command::SetMuted(false)]);

        // Idle engine: nothing to restore.
        let mut e3 = Engine::new(false, false);
        assert!(e3.shutdown_restore().is_empty());
    }

    #[test]
    fn ptt_and_ptm_are_independent() {
        let mut e = Engine::new(true, false);
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Press), vec![Command::SetMuted(false)]);
        // While PTT holds the mic open, a PTM press mutes independently.
        assert_eq!(e.on_edge(Slot::PushToMute, Edge::Press), vec![Command::SetMuted(true)]);
        assert_eq!(e.on_edge(Slot::PushToMute, Edge::Release), vec![Command::SetMuted(false)]);
        assert_eq!(e.on_edge(Slot::PushToTalk, Edge::Release), vec![Command::SetMuted(true)]);
    }

    #[test]
    fn slot_names_roundtrip() {
        for s in Slot::ALL {
            assert_eq!(Slot::parse(s.name()), Some(s));
        }
        assert_eq!(Slot::parse("nope"), None);
        assert!(Slot::PushToTalk.is_hold() && Slot::PushToMute.is_hold());
        assert!(!Slot::MuteToggle.is_hold() && !Slot::MonitorToggle.is_hold());
        // Index mapping used by the capture settings array.
        for s in Slot::ALL {
            assert_eq!(Slot::ALL[s.index()], s);
        }
    }
}
