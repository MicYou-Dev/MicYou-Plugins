//! Persistent configuration, defaults and path/name normalization.

use crate::abi::Host;
use serde_json::Value;
use std::path::PathBuf;

/// Upper bound of diagnostic nodes one plugin instance can expose. Must stay
/// in sync with `plugin.json` (`configSchema.nodeCount.max`) and `panel.html`.
pub const MAX_TAPS: usize = 16;

pub const PLUGIN_ID: &str = "opss.dsp-diagnose";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub node_count: usize,
    pub nodes: Vec<NodeConfig>,
    pub output_dir: String,
    pub segment_seconds: u64,
    /// 16, 24 or 32 (32 = IEEE float).
    pub bits: u8,
    pub auto_start: bool,
    pub sample_rate: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_count: 1,
            nodes: vec![NodeConfig {
                name: "node-1".to_string(),
                enabled: true,
            }],
            output_dir: String::new(),
            segment_seconds: 1800,
            bits: 16,
            auto_start: true,
            sample_rate: 48_000,
        }
    }
}

impl Config {
    /// Load every key from the host config store. Missing or malformed values
    /// fall back to defaults; the node list is always padded to `node_count`.
    pub fn load(host: &Host) -> Self {
        let mut cfg = Config::default();
        let json = |key: &str| host.get_config(key).and_then(|s| serde_json::from_str::<Value>(&s).ok());

        if let Some(v) = json("nodeCount").and_then(|v| number(&v)) {
            cfg.node_count = (v as usize).clamp(1, MAX_TAPS);
        }
        if let Some(v) = json("outputDir").and_then(|v| v.as_str().map(str::to_owned)) {
            cfg.output_dir = v;
        }
        if let Some(v) = json("segmentSeconds").and_then(|v| number(&v)) {
            // 0 disables rotation (documented in docs/TECHNICAL.md); values
            // below the 5 s floor otherwise clamp to the floor.
            cfg.segment_seconds = match v {
                0 => 0,
                1..=4 => 5,
                _ => v.min(24 * 3600),
            };
        }
        if let Some(v) = json("bits").and_then(|v| number(&v)) {
            cfg.bits = match v {
                24 => 24,
                32 => 32,
                _ => 16,
            };
        }
        if let Some(v) = json("autoStart").and_then(|v| v.as_bool()) {
            cfg.auto_start = v;
        }

        if let Some(arr) = json("nodes").and_then(|v| v.as_array().cloned()) {
            cfg.nodes.clear();
            for item in arr {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("node");
                let enabled = item
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                cfg.nodes.push(NodeConfig {
                    name: name.to_string(),
                    enabled,
                });
            }
        }

        cfg.normalize_nodes();
        cfg
    }

    /// Sanitize names, drop empties, de-duplicate and pad/truncate to the
    /// configured node count.
    pub fn normalize_nodes(&mut self) {
        self.node_count = self.node_count.clamp(1, MAX_TAPS);
        self.nodes.truncate(self.node_count);
        while self.nodes.len() < self.node_count {
            let index = self.nodes.len() + 1;
            self.nodes.push(NodeConfig {
                name: format!("node-{index}"),
                enabled: true,
            });
        }

        let mut used: Vec<String> = Vec::with_capacity(self.nodes.len());
        for (i, node) in self.nodes.iter_mut().enumerate() {
            let mut base = sanitize_name(&node.name);
            if base.is_empty() {
                base = format!("node-{}", i + 1);
            }
            let mut candidate = base.clone();
            let mut suffix = 2;
            while used.iter().any(|u| u == &candidate.to_lowercase()) {
                candidate = format!("{base}-{suffix}");
                suffix += 1;
            }
            used.push(candidate.to_lowercase());
            node.name = candidate;
        }
    }

    /// Directory recordings are written to. Empty config means
    /// `<home>/Music/MicYou DSP Diagnose`.
    pub fn resolved_output_dir(&self) -> PathBuf {
        let raw = self.output_dir.trim();
        if raw.is_empty() {
            return default_output_dir();
        }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            if let Some(home) = home_dir() {
                return home.join(rest);
            }
        }
        PathBuf::from(raw)
    }
}

/// Accept both JSON numbers and numeric strings (the auto-generated settings
/// form writes `select` options as strings).
fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// Strip characters that are illegal on Windows/macOS/Linux and trim dots and
/// trailing spaces (Windows cannot create files ending in them).
pub fn sanitize_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim().trim_matches('.').trim();
    let mut limited: String = trimmed.chars().take(64).collect();
    while limited.ends_with('.') || limited.ends_with(' ') {
        limited.pop();
    }
    limited
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn default_output_dir() -> PathBuf {
    match home_dir() {
        Some(home) => home.join("Music").join("MicYou DSP Diagnose"),
        None => std::env::temp_dir().join("MicYou DSP Diagnose"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized_and_unique() {
        let mut cfg = Config {
            node_count: 3,
            nodes: vec![
                NodeConfig {
                    name: " a/b ".into(),
                    enabled: true,
                },
                NodeConfig {
                    name: "a_b".into(),
                    enabled: true,
                },
                NodeConfig {
                    name: "   ".into(),
                    enabled: true,
                },
            ],
            ..Config::default()
        };
        cfg.normalize_nodes();
        assert_eq!(cfg.nodes[0].name, "a_b");
        assert_eq!(cfg.nodes[1].name, "a_b-2");
        assert_eq!(cfg.nodes[2].name, "node-3");
    }

    #[test]
    fn node_list_is_padded_and_truncated() {
        let mut cfg = Config {
            node_count: 2,
            nodes: vec![],
            ..Config::default()
        };
        cfg.normalize_nodes();
        assert_eq!(cfg.nodes.len(), 2);
        assert_eq!(cfg.nodes[1].name, "node-2");

        cfg.node_count = 1;
        cfg.normalize_nodes();
        assert_eq!(cfg.nodes.len(), 1);
    }
}
