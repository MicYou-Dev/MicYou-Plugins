//! Runtime state: per-node taps, chain synchronization and status reporting.
//!
//! The audio thread only ever touches [`Tap::ring`] and a few atomics. Every
//! host call (config, DSP settings, logging) happens on the plugin dispatch
//! thread: init/deinit, event/message handlers and the host timer thread.

use crate::abi::Host;
use crate::config::{Config, MAX_TAPS};
use crate::ring::SpscRing;
use crate::writer::TapWriterState;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

/// One `Plugin:<id>` entry in `AudioDspSettings.processing_chain`.
pub const CHAIN_NODE: &str = "Plugin:opss.dsp-diagnose";

/// Capability boundary: keep at most this many recent log lines for status.
const RECENT_LOGS: usize = 64;

/// Lock helper that tolerates poisoning (a panic in one host callback should
/// not wedge the writer thread for the rest of the session).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One recording tap. `index` is the 0-based chain occurrence index.
pub struct Tap {
    pub ring: SpscRing,
    pub enabled: AtomicBool,
    pub name: Mutex<String>,
    pub writer: Mutex<TapWriterState>,
    pub bytes: AtomicU64,
    pub files: AtomicU64,
    pub dropped: AtomicU64,
    pub current: Mutex<String>,
    pub last: Mutex<String>,
}

impl Tap {
    fn new(index: usize) -> Self {
        Self {
            ring: SpscRing::new(crate::writer::RING_SAMPLES),
            enabled: AtomicBool::new(index == 0),
            name: Mutex::new(format!("node-{}", index + 1)),
            writer: Mutex::new(TapWriterState::default()),
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            last: Mutex::new(String::new()),
        }
    }
}

pub struct Engine {
    host: Host,
    pub taps: Vec<Tap>,

    pub recording: AtomicBool,
    pub node_count: AtomicUsize,
    pub segment_seconds: AtomicU64,
    pub bits: AtomicU8,
    pub sample_rate: AtomicU32,
    pub channels: AtomicUsize,
    pub output_dir: Mutex<PathBuf>,
    /// Set by `shutdown`; the writer thread drains and exits.
    pub stop: AtomicBool,
    /// Set by `start`; the writer (the ring's only consumer) drops samples
    /// queued before the new take began. Clearing the ring from this thread
    /// would race the writer's tail cursor.
    pub discard: AtomicBool,

    /// Number of `Plugin:<id>` nodes observed after the last verification.
    chain_nodes: AtomicUsize,
    /// Whether [`chain_nodes`] has been observed at least once.
    chain_checked: AtomicBool,
    /// Three-state host capability observation: 0 = unknown, 1 = keeps every
    /// requested node, 2 = deduplicates plugin nodes.
    host_multi: AtomicU8,
    /// When false the plugin stops rewriting the chain on every settings
    /// change. Set after a stock (deduplicating) host ignores our write.
    auto_repair: AtomicBool,

    recent_logs: Mutex<VecDeque<String>>,
    last_status: Mutex<String>,
}

impl Engine {
    pub fn new(host: Host) -> Self {
        let mut taps = Vec::with_capacity(MAX_TAPS);
        for index in 0..MAX_TAPS {
            taps.push(Tap::new(index));
        }
        let default_dir = Config::default().resolved_output_dir();
        Self {
            host,
            taps,
            recording: AtomicBool::new(false),
            node_count: AtomicUsize::new(1),
            segment_seconds: AtomicU64::new(1800),
            bits: AtomicU8::new(16),
            sample_rate: AtomicU32::new(48_000),
            channels: AtomicUsize::new(1),
            output_dir: Mutex::new(default_dir),
            stop: AtomicBool::new(false),
            discard: AtomicBool::new(false),
            chain_nodes: AtomicUsize::new(0),
            chain_checked: AtomicBool::new(false),
            host_multi: AtomicU8::new(0),
            auto_repair: AtomicBool::new(true),
            recent_logs: Mutex::new(VecDeque::new()),
            last_status: Mutex::new(String::new()),
        }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    // ── Configuration ──────────────────────────────────────────────────────

    /// Apply a loaded [`Config`]. Returns `true` when the node count changed.
    pub fn apply_config(&self, cfg: &Config) -> bool {
        let previous = self.node_count.load(Ordering::Acquire);
        self.node_count.store(cfg.node_count, Ordering::Release);
        self.segment_seconds
            .store(cfg.segment_seconds, Ordering::Release);
        self.bits.store(cfg.bits, Ordering::Release);
        self.sample_rate
            .store(cfg.sample_rate.max(8_000), Ordering::Release);

        for (index, tap) in self.taps.iter().enumerate() {
            let node = cfg.nodes.get(index);
            let enabled = index < cfg.node_count && node.map(|n| n.enabled).unwrap_or(false);
            tap.enabled.store(enabled, Ordering::Release);
            if let Some(node) = node {
                *lock(&tap.name) = node.name.clone();
            }
        }
        *lock(&self.output_dir) = cfg.resolved_output_dir();
        if previous != cfg.node_count {
            // The previous chain observation describes a different node count.
            self.chain_nodes.store(0, Ordering::Release);
            self.chain_checked.store(false, Ordering::Release);
            self.host_multi.store(0, Ordering::Release);
        }
        self.publish_status(true);
        previous != cfg.node_count
    }

    // ── Recording ──────────────────────────────────────────────────────────

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    /// Start (or keep) recording. Samples queued before this point are
    /// discarded by the writer thread, which owns the ring's consumer cursor.
    pub fn start(&self) {
        if self.recording.swap(true, Ordering::AcqRel) {
            return;
        }
        self.discard.store(true, Ordering::Release);
        self.log("recording started");
        self.publish_status(true);
    }

    pub fn stop_recording(&self) {
        if !self.recording.swap(false, Ordering::AcqRel) {
            return;
        }
        self.log("recording stopped");
        self.publish_status(true);
    }

    /// Stop recording and ask the writer thread to flush and exit.
    pub fn shutdown(&self) {
        self.recording.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
    }

    // ── Audio thread ───────────────────────────────────────────────────────

    /// Called from `micyou_plugin_process`. A stock host has exactly one node
    /// for this plugin, so only tap 0 records; a patched host calls
    /// `micyou_plugin_process_node` and tells us which occurrence this is.
    ///
    /// Real-time constraints: atomics plus a bounded ring copy only.
    pub fn process(&self, data: &[f32], channels: u32, node_index: Option<usize>) {
        if data.is_empty() || !self.recording.load(Ordering::Relaxed) {
            return;
        }
        if channels > 0 {
            self.channels.store(channels as usize, Ordering::Relaxed);
        }
        let index = node_index.unwrap_or(0);
        if index >= MAX_TAPS || index >= self.node_count.load(Ordering::Relaxed) {
            return;
        }
        let tap = &self.taps[index];
        if !tap.enabled.load(Ordering::Relaxed) {
            return;
        }
        let dropped = tap.ring.push_slice(data);
        if dropped > 0 {
            tap.dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    // ── Chain synchronization ──────────────────────────────────────────────

    /// Read `processingChain`, make it contain exactly `nodeCount` copies of
    /// our node, and verify the result.
    ///
    /// A failed verification disables automatic repair, so a stock host that
    /// collapses duplicates is not rewritten on every settings event. Explicit
    /// user actions re-enable one attempt through [`Self::request_chain_repair`].
    pub fn apply_chain(&self) -> Result<usize, String> {
        let desired = self.node_count.load(Ordering::Acquire).max(1);
        let settings = match self.host.get_dsp_settings() {
            Some(settings) => settings,
            None => {
                let message = "host did not expose DSP settings (needs control.observe)";
                self.auto_repair.store(false, Ordering::Release);
                self.log_once(message);
                self.publish_status(true);
                return Err(message.to_string());
            }
        };
        let value: Value = serde_json::from_str(&settings)
            .map_err(|error| format!("invalid DSP settings JSON: {error}"))?;
        let chain = read_chain(&value)?;

        let updated = build_chain(&chain, desired);
        if updated != chain {
            let patch = json!({ "processingChain": updated }).to_string();
            if !self.host.set_dsp_settings(&patch) {
                self.auto_repair.store(false, Ordering::Release);
                let message = "host rejected the DSP settings update (needs control.intercept)";
                self.log_once(message);
                self.publish_status(true);
                return Err(message.to_string());
            }
        }

        let observed = self.read_chain_len()?;
        self.note_chain_state(observed);
        let ok = observed == desired;
        self.auto_repair.store(ok, Ordering::Release);
        self.publish_status(true);

        if ok {
            Ok(observed)
        } else {
            Err(format!(
                "host kept {observed} of {desired} nodes; it deduplicates plugin chain entries"
            ))
        }
    }

    /// Count our nodes in the current settings without writing anything.
    pub fn verify_chain(&self) -> Option<usize> {
        let observed = self.read_chain_len().ok()?;
        self.note_chain_state(observed);
        if observed == self.node_count.load(Ordering::Acquire).max(1) {
            // The chain is consistent again (for example after the user fixed
            // it by hand), so automatic repair may resume.
            self.auto_repair.store(true, Ordering::Release);
        }
        self.publish_status(true);
        Some(observed)
    }

    /// Called when the host reports that DSP settings changed. Verify, then
    /// repair only while the host is known to honour duplicate nodes.
    pub fn on_dsp_settings_changed(&self) {
        if self.auto_repair.load(Ordering::Acquire) {
            let _ = self.apply_chain();
        } else {
            let _ = self.verify_chain();
        }
    }

    /// Re-enable automatic repair after a user-driven config change. `force`
    /// also performs one attempt when automatic repair had backed off.
    pub fn request_chain_repair(&self, force: bool) {
        if force {
            self.auto_repair.store(true, Ordering::Release);
        }
        if self.auto_repair.load(Ordering::Acquire) {
            let _ = self.apply_chain();
        }
    }

    fn read_chain_len(&self) -> Result<usize, String> {
        let Some(settings) = self.host.get_dsp_settings() else {
            let message = "host did not expose DSP settings";
            self.log_once(message);
            self.publish_status(true);
            return Err(message.to_string());
        };
        let value: Value = serde_json::from_str(&settings)
            .map_err(|error| format!("invalid DSP settings JSON: {error}"))?;
        let chain = read_chain(&value)?;
        Ok(chain.iter().filter(|node| node.as_str() == CHAIN_NODE).count())
    }

    fn note_chain_state(&self, observed: usize) {
        let desired = self.node_count.load(Ordering::Acquire).max(1);
        self.chain_nodes.store(observed, Ordering::Release);
        self.chain_checked.store(true, Ordering::Release);
        // With one requested node the host's behaviour is unobservable: both a
        // patched and a stock host keep exactly one copy.
        let multi = if observed > desired {
            1
        } else if observed < desired {
            2
        } else if desired > 1 {
            1
        } else {
            0
        };
        self.host_multi.store(multi, Ordering::Release);
    }

    // ── Status / logging ───────────────────────────────────────────────────

    /// Publish a `status` config value the panel polls. Identical payloads are
    /// skipped unless `force` is set, to avoid rewriting settings needlessly.
    pub fn publish_status(&self, force: bool) {
        let text = self.status_json().to_string();
        {
            let mut last = lock(&self.last_status);
            if !force && *last == text {
                return;
            }
            *last = text.clone();
        }
        self.host.set_config("status", &text);
    }

    fn status_json(&self) -> Value {
        let node_count = self.node_count.load(Ordering::Acquire);
        let mut nodes = Vec::with_capacity(node_count.min(MAX_TAPS));
        for (index, tap) in self.taps.iter().take(node_count).enumerate() {
            nodes.push(json!({
                "index": index + 1,
                "name": lock(&tap.name).clone(),
                "enabled": tap.enabled.load(Ordering::Acquire),
                "files": tap.files.load(Ordering::Relaxed),
                "bytes": tap.bytes.load(Ordering::Relaxed),
                "dropped": tap.dropped.load(Ordering::Relaxed),
                "current": lock(&tap.current).clone(),
                "last": lock(&tap.last).clone(),
            }));
        }
        let chain_nodes = self.chain_nodes.load(Ordering::Acquire);
        let chain_checked = self.chain_checked.load(Ordering::Acquire);
        let host_multi = match self.host_multi.load(Ordering::Acquire) {
            1 => json!(true),
            2 => json!(false),
            _ => Value::Null,
        };
        json!({
            "recording": self.is_recording(),
            "nodeCount": node_count,
            "chainNodes": chain_nodes,
            "chainChecked": chain_checked,
            "chainOk": if chain_checked {
                json!(chain_nodes == node_count)
            } else {
                Value::Null
            },
            "hostMultiNode": host_multi,
            "outputDir": lock(&self.output_dir).display().to_string(),
            "segmentSeconds": self.segment_seconds.load(Ordering::Relaxed),
            "bits": self.bits.load(Ordering::Relaxed),
            "sampleRate": self.sample_rate.load(Ordering::Relaxed),
            "channels": self.channels.load(Ordering::Relaxed),
            "nodes": nodes,
            "lastLog": lock(&self.recent_logs).back().cloned().unwrap_or_default(),
        })
    }

    /// Log a message once per session (or until it falls out of the recent
    /// window), so a failing disk cannot flood the host log.
    pub fn log_once(&self, message: impl Into<String>) {
        let message = message.into();
        {
            let mut recent = lock(&self.recent_logs);
            if recent.iter().any(|entry| entry == &message) {
                return;
            }
            recent.push_back(message.clone());
            while recent.len() > RECENT_LOGS {
                recent.pop_front();
            }
        }
        self.host.warn(&message);
    }

    pub fn log(&self, message: impl Into<String>) {
        self.host.info(&message.into());
    }
}

/// Extract `processingChain` from an `AudioDspSettings` object.
pub fn read_chain(value: &Value) -> Result<Vec<String>, String> {
    let array = value
        .get("processingChain")
        .and_then(|chain| chain.as_array())
        .ok_or_else(|| "DSP settings have no processingChain array".to_string())?;
    let mut chain = Vec::with_capacity(array.len());
    for item in array {
        match item.as_str() {
            Some(node) => chain.push(node.to_string()),
            None => return Err("processingChain contains a non-string node".to_string()),
        }
    }
    Ok(chain)
}

/// Return `chain` with exactly `desired` copies of [`CHAIN_NODE`].
///
/// Existing copies keep their positions (the user arranges node 1..N by
/// dragging). Missing copies are inserted after the last existing copy, then
/// extras beyond `desired` are removed from the end.
pub fn build_chain(chain: &[String], desired: usize) -> Vec<String> {
    let desired = desired.max(1);
    let positions: Vec<usize> = chain
        .iter()
        .enumerate()
        .filter(|(_, node)| node.as_str() == CHAIN_NODE)
        .map(|(index, _)| index)
        .collect();

    if positions.len() == desired {
        return chain.to_vec();
    }

    let mut out = chain.to_vec();
    if positions.len() > desired {
        for &position in positions.iter().skip(desired).rev() {
            out.remove(position);
        }
        return out;
    }

    let insert_at = match positions.last() {
        Some(&last) => last + 1,
        None => default_insert_position(&out),
    };
    for offset in 0..(desired - positions.len()) {
        out.insert(insert_at + offset, CHAIN_NODE.to_string());
    }
    out
}

/// Insert after the last plugin node, else right after AEC, else at the end.
fn default_insert_position(chain: &[String]) -> usize {
    if let Some(position) = chain.iter().rposition(|node| node.starts_with("Plugin:")) {
        return position + 1;
    }
    if let Some(position) = chain.iter().position(|node| node == "AEC") {
        return position + 1;
    }
    chain.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PLUGIN_ID;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn chain_node_matches_plugin_id() {
        assert_eq!(CHAIN_NODE, format!("Plugin:{PLUGIN_ID}"));
    }

    #[test]
    fn missing_nodes_are_added_after_aec() {
        let chain = owned(&["AEC", "NoiseReduction"]);
        assert_eq!(
            build_chain(&chain, 2),
            owned(&["AEC", CHAIN_NODE, CHAIN_NODE, "NoiseReduction"])
        );
    }

    #[test]
    fn existing_nodes_keep_their_positions() {
        let chain = owned(&["AEC", CHAIN_NODE, "Equalizer"]);
        assert_eq!(
            build_chain(&chain, 3),
            owned(&["AEC", CHAIN_NODE, CHAIN_NODE, CHAIN_NODE, "Equalizer"])
        );
        assert_eq!(
            build_chain(&chain, 1),
            owned(&["AEC", CHAIN_NODE, "Equalizer"])
        );
    }

    #[test]
    fn extras_are_removed_from_the_end() {
        let chain = owned(&["AEC", CHAIN_NODE, "VAD", CHAIN_NODE, CHAIN_NODE]);
        assert_eq!(
            build_chain(&chain, 2),
            owned(&["AEC", CHAIN_NODE, "VAD", CHAIN_NODE])
        );
    }

    #[test]
    fn node_is_appended_without_aec_or_plugin_nodes() {
        let chain = owned(&["NoiseReduction"]);
        assert_eq!(
            build_chain(&chain, 1),
            owned(&["NoiseReduction", CHAIN_NODE])
        );
    }
}
