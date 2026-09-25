//! MicYou native plugin — `opss.customizable-floating-window` (可自定义悬浮窗)
//!
//! A customizable floating window for MicYou status monitoring and control.
//! The window itself is rendered by **Slint 1.18** inside a small helper
//! child process from a user-swappable `.slint` theme file; this cdylib is
//! the MicYou-side controller:
//!
//! * subscribes to `audio.state` + control-plane state on every host tick
//!   (`set_interval`, ~10 Hz) and pushes JSON frames into the helper's stdin;
//! * receives interaction events (mute click, drag, menu…) from the helper's
//!   stdout and executes them **on host-dispatched threads only**
//!   (`handle_message`), honoring the plugin threading contract;
//! * tracks the current connection's uptime (`device_connected` /
//!   `device_disconnected` events, with a `streaming`-flag fallback);
//! * manages the helper lifecycle (spawn / restart with backoff / kill on
//!   deinit — no orphans in either direction);
//! * serves `panel.html` (theme switcher, live .slint editor, status mirror)
//!   through config keys + `ui:<action>` bus messages.
//!
//! Thread discipline (api-reference.md「使用注意事项」):
//!
//! | thread | touches | Host API |
//! | --- | --- | --- |
//! | host-dispatched (init / deinit / handle_event / handle_message incl. interval ticks) | CORE state machine | ✅ only here |
//! | helper stdin-writer / stdout-reader / stderr-tail | pipes, event queue | ❌ never |
//! | helper *process* (Slint UI) | window rendering | ❌ never (separate process) |
//!
//! Not a DSP plugin: `micyou_plugin_process` is intentionally NOT exported,
//! so the real-time audio thread never enters this library.

#![allow(non_camel_case_types)]

mod abi;
mod helper;
mod session;

use abi::{mpl_host_api_t, mpl_plugin_info_t, mpl_result_t};
use cfw_protocol::{Cmd, Ev, StatePayload};
use helper::HelperProc;
use session::Session;
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PLUGIN_ID: &[u8] = b"opss.customizable-floating-window\0";
const PLUGIN_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// Interval tick payload tag (filters foreign timers routed to our id).
const TICK_PAYLOAD: &str = "cfw";
/// Panel descriptor id (plugin.json ui.panels[0].id).
const PANEL_ID: &str = "settings";
/// Default theme shipped in `themes/`.
const DEFAULT_THEME: &str = "ring.slint";
/// Config (re)read cadence: every 10 ticks (≈1 s at 100 ms).
const CFG_POLL_TICKS: u64 = 10;
/// themes/ directory rescan cadence: every 50 ticks (≈5 s).
const DIR_SCAN_TICKS: u64 = 50;
/// `status` config mirror write cadence for the panel (≥1 s, change-gated).
const STATUS_WRITE: Duration = Duration::from_millis(1000);
/// Window-position config write throttle.
const POS_WRITE: Duration = Duration::from_millis(1000);
/// Restart backoff: 0.5s·2ⁿ capped at 15 s; give up after 6 consecutive
/// crashes (until the user intervenes from the panel).
const MAX_RESTARTS: u32 = 6;
const RESTART_CAP: Duration = Duration::from_secs(15);
/// A helper that stays healthy this long resets the crash counter.
const HEALTHY_RESET: Duration = Duration::from_secs(60);

/// Bundled themes — written into `<plugindir>/themes/` on first run so the
/// user can copy/modify them; never overwritten unless the panel asks.
const BUNDLED: &[(&str, &str)] = &[
    ("ring.slint", include_str!("../../themes/ring.slint")),
    ("pill.slint", include_str!("../../themes/pill.slint")),
    ("minimal.slint", include_str!("../../themes/minimal.slint")),
];

// ─────────────────────────── core state ───────────────────────────

#[derive(Clone)]
struct Cfg {
    theme: String,
    visible: bool,
    update_ms: u64,
    wdis_enabled: bool,
    wdis_hold_ms: u64,
}

struct Core {
    dir: PathBuf,
    themes_dir: PathBuf,
    cfg: Cfg,
    /// Persisted window position (None = auto top-right).
    win_pos: Option<(i32, i32)>,
    interval_id: u64,
    tick: u64,
    session: Session,
    muted: bool,
    monitoring: bool,
    helper: Option<HelperProc>,
    restarts: u32,
    next_retry: Instant,
    gave_up: bool,
    crash_notified: bool,
    pending_pos: Option<(i32, i32)>,
    last_pos_write: Instant,
    last_status_json: String,
    last_status_write: Instant,
    last_theme_list: Vec<String>,
    last_theme_err_notified: String,
    helper_state: String,
    last_helper_state_written: String,
}

static CORE: Mutex<Option<Core>> = Mutex::new(None);

/// Keep the lock helper tiny and poisoning-tolerant (a panicked tick must
/// not brick the plugin for the rest of the host session).
fn with_core<R>(f: impl FnOnce(&mut Core) -> R) -> Option<R> {
    let mut slot = CORE.lock().ok()?;
    slot.as_mut().map(f)
}

impl Core {
    fn new(dir: PathBuf) -> Self {
        let themes_dir = dir.join("themes");
        Self {
            dir,
            themes_dir,
            cfg: Cfg {
                theme: DEFAULT_THEME.to_string(),
                visible: true,
                update_ms: 100,
                wdis_enabled: true,
                wdis_hold_ms: 4000,
            },
            win_pos: None,
            interval_id: 0,
            tick: 0,
            session: Session::new(),
            muted: false,
            monitoring: false,
            helper: None,
            restarts: 0,
            next_retry: Instant::now(),
            gave_up: false,
            crash_notified: false,
            pending_pos: None,
            last_pos_write: Instant::now(),
            last_status_json: String::new(),
            last_status_write: Instant::now() - STATUS_WRITE,
            last_theme_list: Vec::new(),
            last_theme_err_notified: String::new(),
            helper_state: "idle".into(),
            last_helper_state_written: String::new(),
        }
    }

    // ── config ──

    fn load_cfg(&mut self) {
        if let Some(v) = abi::get_config("theme")
            .and_then(|s| serde_json::from_str::<String>(&s).ok())
        {
            if valid_theme_name(&v) {
                self.cfg.theme = v;
            }
        }
        if let Some(v) = abi::get_config("visible")
            .and_then(|s| serde_json::from_str::<bool>(&s).ok())
        {
            self.cfg.visible = v;
        }
        if let Some(v) = abi::get_config("updateMs")
            .and_then(|s| serde_json::from_str::<u64>(&s).ok())
        {
            self.cfg.update_ms = v.clamp(50, 1000);
        }
        if let Some(v) = abi::get_config("wdisEnabled")
            .and_then(|s| serde_json::from_str::<bool>(&s).ok())
        {
            self.cfg.wdis_enabled = v;
        }
        if let Some(v) = abi::get_config("wdisHoldMs")
            .and_then(|s| serde_json::from_str::<u64>(&s).ok())
        {
            self.cfg.wdis_hold_ms = v.clamp(1000, 30_000);
        }
        let x = abi::get_config("windowX").and_then(|s| serde_json::from_str::<i32>(&s).ok());
        let y = abi::get_config("windowY").and_then(|s| serde_json::from_str::<i32>(&s).ok());
        self.win_pos = match (x, y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        };
    }

    fn set_cfg_key(&self, key: &str, json: &str) {
        if !abi::set_config(key, json) {
            abi::log_warn(&format!("set_config({key}) failed"));
        }
    }

    // ── themes ──

    fn ensure_themes(&self) {
        if let Err(e) = std::fs::create_dir_all(&self.themes_dir) {
            abi::log_error(&format!("cannot create themes dir: {e}"));
            return;
        }
        for (name, src) in BUNDLED {
            let p = self.themes_dir.join(name);
            let existing = std::fs::read_to_string(&p).ok();
            // Upgrade policy: files carrying the `// cfw-bundled:` marker are
            // factory content and refresh on plugin upgrade; marker-less files
            // are user property and are never touched.
            let write = match existing.as_deref() {
                None => true,
                Some(old) => old.contains("cfw-bundled:") && old != *src,
            };
            if write {
                if let Err(e) = std::fs::write(&p, src) {
                    abi::log_warn(&format!("cannot write bundled theme {name}: {e}"));
                } else if existing.is_some() {
                    abi::log_info(&format!("bundled theme upgraded: {name}"));
                }
            }
        }
    }

    fn scan_themes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.themes_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "slint").unwrap_or(false) {
                    if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                        if valid_theme_name(n) {
                            out.push(n.to_string());
                        }
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn publish_theme_list(&mut self, force: bool) {
        let list = self.scan_themes();
        if force || list != self.last_theme_list {
            self.last_theme_list = list.clone();
            if let Ok(json) = serde_json::to_string(&list) {
                self.set_cfg_key("themeList", &json);
            }
        }
    }

    fn resolve_theme(&self) -> PathBuf {
        let p = self.themes_dir.join(&self.cfg.theme);
        if p.is_file() {
            return p;
        }
        // Selected theme vanished → fall back to any bundled one that exists.
        for (name, _) in BUNDLED {
            let q = self.themes_dir.join(name);
            if q.is_file() {
                return q;
            }
        }
        p // let the helper report the error
    }

    // ── helper lifecycle ──

    fn spawn_helper(&mut self) {
        let theme = self.resolve_theme();
        let pos = self.win_pos.unwrap_or((-1, -1));
        match HelperProc::spawn(&self.dir, &theme, pos) {
            Ok(h) => {
                abi::log_info(&format!("helper spawned (theme {})", theme.display()));
                self.helper = Some(h);
                self.helper_state = "starting".into();
            }
            Err(e) => {
                abi::log_error(&format!("helper spawn failed: {e}"));
                self.helper_state = "missing".into();
                self.schedule_retry();
            }
        }
    }

    fn schedule_retry(&mut self) {
        self.restarts += 1;
        if self.restarts >= MAX_RESTARTS {
            self.gave_up = true;
            self.helper_state = "crashed".into();
            if !self.crash_notified {
                self.crash_notified = true;
                let tail = self
                    .helper
                    .as_ref()
                    .map(|h| h.stderr_snapshot().join(" | "))
                    .unwrap_or_default();
                abi::log_error(&format!("helper crashed {MAX_RESTARTS}× — giving up. stderr tail: {tail}"));
                abi::notify(
                    "悬浮窗 FloatingWindow",
                    "悬浮窗进程反复崩溃，已停止重试。可在插件面板中重启或更换主题。",
                );
            }
            return;
        }
        let backoff = Duration::from_millis(500u64.saturating_mul(1u64 << self.restarts.min(5)));
        self.next_retry = Instant::now() + backoff.min(RESTART_CAP);
        self.helper_state = "crashed".into();
    }

    fn kill_helper(&mut self) {
        if let Some(h) = self.helper.take() {
            h.shutdown();
        }
    }

    /// Per-tick health supervision: detect death, restart with backoff,
    /// reset counters after a long healthy run.
    fn supervise_helper(&mut self) {
        if let Some(h) = self.helper.as_mut() {
            if h.is_dead() {
                let ran = h.started_at.elapsed();
                let tail = h.stderr_snapshot();
                abi::log_warn(&format!(
                    "helper exited after {:.1}s; stderr tail: {}",
                    ran.as_secs_f32(),
                    tail.join(" | ")
                ));
                let was_healthy = ran >= HEALTHY_RESET;
                self.helper = None;
                if was_healthy {
                    self.restarts = 0;
                }
                if self.cfg.visible {
                    self.schedule_retry();
                } else {
                    // Hidden by user → expected exit (event loop quits when
                    // the last window hides). Not a crash.
                    self.helper_state = "hidden".into();
                }
            } else if h.started_at.elapsed() >= HEALTHY_RESET && self.restarts > 0 {
                self.restarts = 0;
                self.gave_up = false;
            }
        }
        if self.cfg.visible && self.helper.is_none() && !self.gave_up && Instant::now() >= self.next_retry
        {
            self.spawn_helper();
        }
    }

    // ── events from the helper ──

    fn handle_helper_events(&mut self) {
        let events = match self.helper.as_ref() {
            Some(h) => h.drain_events(),
            None => return,
        };
        for ev in events {
            match ev {
                Ev::Mute => {
                    let next = !self.muted;
                    if abi::set_muted(next) {
                        self.muted = next; // optimistic; events confirm
                    } else {
                        abi::log_warn("set_muted rejected by host");
                    }
                }
                Ev::Monitoring => {
                    let next = !self.monitoring;
                    if abi::set_monitoring(next) {
                        self.monitoring = next;
                    } else {
                        abi::log_warn("set_monitoring rejected by host");
                    }
                }
                Ev::Menu { action } => match action.as_str() {
                    "reload-theme" => self.reload_theme(),
                    other => abi::log_info(&format!("theme menu action: {other}")),
                },
                Ev::Hide => {
                    self.cfg.visible = false;
                    self.set_cfg_key("visible", "false");
                    self.helper_state = "hidden".into();
                }
                Ev::Moved { x, y } => {
                    self.pending_pos = Some((x, y));
                    self.flush_pos(false);
                }
                Ev::Ready { theme, reloaded } => {
                    self.restarts = 0;
                    self.gave_up = false;
                    self.crash_notified = false;
                    self.helper_state = "running".into();
                    self.set_cfg_key("themeError", "\"\"");
                    abi::log_info(&format!(
                        "theme ready: {theme}{}",
                        if reloaded { " (hot-swap)" } else { "" }
                    ));
                }
                Ev::ThemeError { message } => {
                    if message.is_empty() {
                        continue;
                    }
                    abi::log_warn(&format!("theme error: {message}"));
                    let json = serde_json::to_string(&message).unwrap_or_else(|_| "\"\"".into());
                    self.set_cfg_key("themeError", &json);
                    if self.last_theme_err_notified != message {
                        self.last_theme_err_notified = message.clone();
                        abi::notify("悬浮窗主题错误", &message);
                    }
                }
                Ev::Log { message } => abi::log(abi::LOG_DEBUG, &format!("[helper] {message}")),
                Ev::Bye => {}
            }
        }
    }

    fn reload_theme(&mut self) {
        let path = self.resolve_theme();
        match self.helper.as_ref() {
            Some(h) => {
                h.send(&Cmd::Theme {
                    path: path.display().to_string(),
                });
            }
            None if self.cfg.visible => self.spawn_helper(),
            None => {}
        }
    }

    // ── per-tick work (host-dispatched thread) ──

    fn push_state(&mut self, st: &abi::AudioState) {
        self.muted = st.muted;
        self.session.observe_streaming(st.streaming);
        let snap = self.session.snapshot();
        let payload = StatePayload {
            level: f64::from(st.input_level.clamp(0.0, 1.0)),
            processed: f64::from(st.processed_level.clamp(0.0, 1.0)),
            muted: st.muted,
            streaming: st.streaming,
            monitoring: self.monitoring,
            sample_rate: st.sample_rate,
            channels: st.channels,
            queued_ms: st.queued_ms,
            session_seconds: snap.seconds,
            session_text: session::format_duration(snap.seconds),
            device_label: snap.label,
            device_mode: snap.mode,
        };
        if let Some(h) = self.helper.as_ref() {
            h.send(&Cmd::State(payload.clone()));
        }
        self.mirror_status(&payload);
    }

    fn mirror_status(&mut self, p: &StatePayload) {
        let json = serde_json::json!({
            "streaming": p.streaming,
            "muted": p.muted,
            "monitoring": p.monitoring,
            "level": (p.level * 100.0).round() / 100.0,
            "sessionSeconds": p.session_seconds,
            "sessionText": p.session_text,
            "deviceLabel": p.device_label,
            "sampleRate": p.sample_rate,
            "channels": p.channels,
            "queuedMs": (p.queued_ms * 10.0).round() / 10.0,
        });
        let text = json.to_string();
        let now = Instant::now();
        if text != self.last_status_json && now - self.last_status_write >= STATUS_WRITE
            || now - self.last_status_write >= STATUS_WRITE * 4
        {
            self.last_status_json = text.clone();
            self.last_status_write = now;
            self.set_cfg_key("status", &text);
            if self.helper_state_changed() {
                let s = serde_json::to_string(&self.helper_state).unwrap_or_default();
                self.set_cfg_key("helperState", &s);
            }
        }
    }

    fn helper_state_changed(&mut self) -> bool {
        if self.last_helper_state_written != self.helper_state {
            self.last_helper_state_written = self.helper_state.clone();
            true
        } else {
            false
        }
    }

    fn flush_pos(&mut self, force: bool) {
        let Some(pos) = self.pending_pos else { return };
        let now = Instant::now();
        if !force && now - self.last_pos_write < POS_WRITE {
            return;
        }
        self.last_pos_write = now;
        self.pending_pos = None;
        self.win_pos = Some(pos);
        self.set_cfg_key("windowX", &pos.0.to_string());
        self.set_cfg_key("windowY", &pos.1.to_string());
    }

    fn poll_cfg(&mut self) {
        let old = self.cfg.clone();
        let old_pos = self.win_pos;
        self.load_cfg();

        // interval cadence change → re-arm
        if self.cfg.update_ms != old.update_ms {
            abi::clear_interval(self.interval_id);
            self.interval_id = abi::set_interval(self.cfg.update_ms, TICK_PAYLOAD);
            abi::log_info(&format!("tick interval now {}ms", self.cfg.update_ms));
        }
        // theme change (panel wrote config) → hot-swap
        if self.cfg.theme != old.theme {
            self.reload_theme();
        }
        // visibility change
        if self.cfg.visible != old.visible {
            if self.cfg.visible {
                self.gave_up = false;
                self.restarts = 0;
                self.next_retry = Instant::now();
                if self.helper.is_none() {
                    self.spawn_helper();
                }
            } else if let Some(h) = self.helper.as_ref() {
                h.send(&Cmd::Visible { show: false });
                self.helper_state = "hidden".into();
            }
        }
        // position reset from panel (config cleared to null)
        if self.win_pos.is_none() && old_pos.is_some() {
            if let Some(h) = self.helper.as_ref() {
                h.send(&Cmd::PosDefault);
            }
        }
    }

    fn tick(&mut self) {
        self.tick += 1;
        self.supervise_helper();
        self.handle_helper_events();
        if let Some(st) = abi::audio_state() {
            self.push_state(&st);
        }
        if self.tick % CFG_POLL_TICKS == 0 {
            self.poll_cfg();
        }
        if self.tick % DIR_SCAN_TICKS == 0 {
            self.publish_theme_list(false);
        }
        self.flush_pos(false);
    }

    // ── panel actions (bus messages, topic "ui:<action>") ──

    fn ui_action(&mut self, action: &str, payload: &[u8]) {
        match action {
            "apply-theme" | "reload" => {
                self.load_cfg();
                self.publish_theme_list(true);
                self.reload_theme();
            }
            "restart-helper" => {
                self.kill_helper();
                self.restarts = 0;
                self.gave_up = false;
                self.crash_notified = false;
                self.next_retry = Instant::now();
                self.load_cfg();
                if self.cfg.visible {
                    self.spawn_helper();
                }
            }
            "show" => {
                self.cfg.visible = true;
                self.set_cfg_key("visible", "true");
                self.gave_up = false;
                self.next_retry = Instant::now();
                if self.helper.is_none() {
                    self.spawn_helper();
                } else if let Some(h) = self.helper.as_ref() {
                    h.send(&Cmd::Visible { show: true });
                }
            }
            "hide" => {
                self.cfg.visible = false;
                self.set_cfg_key("visible", "false");
                if let Some(h) = self.helper.as_ref() {
                    h.send(&Cmd::Visible { show: false });
                }
                self.helper_state = "hidden".into();
            }
            "reset-pos" => {
                self.win_pos = None;
                self.pending_pos = None;
                self.set_cfg_key("windowX", "null");
                self.set_cfg_key("windowY", "null");
                if let Some(h) = self.helper.as_ref() {
                    h.send(&Cmd::PosDefault);
                }
            }
            "snapshot" => {
                // Diagnostics: render the window to a PNG (used by CI/tests).
                #[derive(serde::Deserialize)]
                struct SnapArgs {
                    #[serde(default)]
                    path: String,
                }
                if let Ok(args) = serde_json::from_slice::<SnapArgs>(payload) {
                    if !args.path.is_empty() {
                        if let Some(h) = self.helper.as_ref() {
                            h.send(&Cmd::Snapshot { path: args.path });
                        }
                    }
                }
            }
            "save-theme" => self.save_theme_draft(),
            "stage-theme" => {
                // Panel wants to edit an existing theme: stage its content
                // into the `themeDraft` config key (panels have no fs access).
                let name = abi::get_config("themeDraftName")
                    .and_then(|s| serde_json::from_str::<String>(&s).ok())
                    .filter(|n| valid_theme_name(n))
                    .unwrap_or_else(|| self.cfg.theme.clone());
                match std::fs::read_to_string(self.themes_dir.join(&name)) {
                    Ok(src) => {
                        let json =
                            serde_json::to_string(&src).unwrap_or_else(|_| "\"\"".into());
                        self.set_cfg_key("themeDraft", &json);
                    }
                    Err(e) => abi::log_warn(&format!("stage-theme: cannot read {name}: {e}")),
                }
            }
            "restore-themes" => {
                for (name, src) in BUNDLED {
                    let p = self.themes_dir.join(name);
                    if let Err(e) = std::fs::write(&p, src) {
                        abi::log_warn(&format!("restore {name} failed: {e}"));
                    }
                }
                self.publish_theme_list(true);
                abi::log_info("bundled themes restored");
            }
            "log" => {
                // Panel bridge `log` API → host plugin log.
                #[derive(serde::Deserialize)]
                struct LogArgs {
                    #[serde(default)]
                    level: String,
                    #[serde(default)]
                    message: String,
                }
                if let Ok(args) = serde_json::from_slice::<LogArgs>(payload) {
                    let level = match args.level.as_str() {
                        "error" => abi::LOG_ERROR,
                        "warn" => abi::LOG_WARN,
                        "debug" => abi::LOG_DEBUG,
                        _ => abi::LOG_INFO,
                    };
                    abi::log(level, &format!("[panel] {}", args.message));
                }
            }
            other => abi::log(abi::LOG_DEBUG, &format!("ignoring ui action {other}")),
        }
    }

    fn save_theme_draft(&mut self) {
        let name = abi::get_config("themeDraftName")
            .and_then(|s| serde_json::from_str::<String>(&s).ok())
            .unwrap_or_default();
        let src = abi::get_config("themeDraft")
            .and_then(|s| serde_json::from_str::<String>(&s).ok())
            .unwrap_or_default();
        if !valid_theme_name(&name) {
            self.set_cfg_key(
                "themeError",
                r#""文件名无效：仅允许字母数字 . _ - 且以 .slint 结尾""#,
            );
            return;
        }
        if src.trim().is_empty() {
            self.set_cfg_key("themeError", r#""主题为空""#);
            return;
        }
        // Guard rails: the file lives inside our own plugin dir; the name is
        // already validated (no separators / traversal possible).
        if src.len() > 512 * 1024 {
            self.set_cfg_key("themeError", r#""主题过大（>512KB）""#);
            return;
        }
        let p = self.themes_dir.join(&name);
        match std::fs::write(&p, &src) {
            Ok(()) => {
                abi::log_info(&format!("theme saved: {}", p.display()));
                self.set_cfg_key("themeError", "\"\"");
                self.publish_theme_list(true);
            }
            Err(e) => {
                let msg = format!("写入失败: {e}");
                let json = serde_json::to_string(&msg).unwrap_or_else(|_| "\"\"".into());
                self.set_cfg_key("themeError", &json);
            }
        }
    }

    fn shutdown(&mut self) {
        abi::clear_interval(self.interval_id);
        self.interval_id = 0;
        self.flush_pos(true);
        self.kill_helper();
        self.helper_state = "stopped".into();
    }
}

/// Theme file names must be simple, traversal-free names ending in `.slint`.
fn valid_theme_name(name: &str) -> bool {
    let ok_len = !name.is_empty() && name.len() <= 64;
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    let ok_ext = name.ends_with(".slint");
    // reject "", ".slint", "..", hidden files and any traversal-looking name
    let ok_dots = !name.starts_with('.') && name.len() > ".slint".len();
    ok_len && ok_chars && ok_ext && ok_dots
}

/// Every FFI entry point runs through this guard: panics must never cross
/// the C ABI boundary (UB); they degrade to `MPL_ERR_RUNTIME`.
fn guard<F: FnOnce() -> mpl_result_t>(f: F) -> mpl_result_t {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            // Best-effort log without the host lock (it may be poisoned).
            eprintln!("[cfw-plugin] panic contained at FFI boundary");
            mpl_result_t::MPL_ERR_RUNTIME
        }
    }
}

// ─────────────────────────── FFI entry points ───────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn micyou_plugin_info() -> *const mpl_plugin_info_t {
    static INFO: mpl_plugin_info_t = mpl_plugin_info_t {
        abi_version: abi::MPL_ABI_VERSION,
        api_version: abi::MPL_API_VERSION,
        id: PLUGIN_ID.as_ptr() as *const c_char,
        version: PLUGIN_VERSION.as_ptr() as *const c_char,
    };
    &INFO
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn micyou_plugin_init(host: *const mpl_host_api_t) -> mpl_result_t {
    guard(|| {
        if host.is_null() {
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        // The host pointer is only valid during init — copy the table by value.
        let table = unsafe { *host };
        abi::store_host(table);

        // Re-init safety: a previous instance must be fully torn down first.
        if let Some(mut old) = CORE.lock().ok().and_then(|mut s| s.take()) {
            old.shutdown();
        }

        let Some(dir) = abi::plugin_dir().map(PathBuf::from).filter(|d| !d.as_os_str().is_empty())
        else {
            abi::log_error("plugin_dir unavailable — cannot locate themes");
            return mpl_result_t::MPL_ERR_RUNTIME;
        };

        let mut core = Core::new(dir);
        core.ensure_themes();
        core.load_cfg();

        if let Some(info) = abi::host_info() {
            abi::log_info(&format!("initializing against host {info}"));
        }
        core.monitoring = abi::get_monitoring().unwrap_or(false);
        core.muted = abi::get_muted().unwrap_or(false);

        core.publish_theme_list(true);
        abi::set_panel_icon(PANEL_ID, "🪟");

        if core.cfg.visible {
            core.spawn_helper();
        } else {
            core.helper_state = "hidden".into();
        }

        core.interval_id = abi::set_interval(core.cfg.update_ms, TICK_PAYLOAD);
        if core.interval_id == 0 {
            abi::log_error("set_interval unavailable — plugin cannot run");
            core.shutdown();
            return mpl_result_t::MPL_ERR_RUNTIME;
        }

        abi::log_info(&format!(
            "ready (theme {}, tick {}ms, visible {})",
            core.cfg.theme, core.cfg.update_ms, core.cfg.visible
        ));
        match CORE.lock() {
            Ok(mut slot) => *slot = Some(core),
            Err(_) => return mpl_result_t::MPL_ERR_RUNTIME,
        }
        mpl_result_t::MPL_OK
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn micyou_plugin_deinit() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(mut core) = CORE.lock().ok().and_then(|mut s| s.take()) {
            core.shutdown();
        }
        abi::clear_host();
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn micyou_plugin_handle_event(
    event_type: *const c_char,
    json: *const c_char,
) -> mpl_result_t {
    guard(|| {
        let ty = unsafe { abi::cstr_or(event_type, "") }.to_string();
        let payload = unsafe { abi::cstr_or(json, "{}") }.to_string();
        let _ = with_core(|core| {
            match ty.as_str() {
                "device_connected" => {
                    #[derive(serde::Deserialize, Default)]
                    struct Dev {
                        #[serde(default)]
                        mode: String,
                        #[serde(default)]
                        label: String,
                    }
                    let dev: Dev = serde_json::from_str(&payload).unwrap_or_default();
                    core.session.device_connected(&dev.mode, &dev.label);
                    abi::log_info(&format!("device connected: {} ({})", dev.label, dev.mode));
                }
                "device_disconnected" => {
                    core.session.device_disconnected();
                    abi::log_info("device disconnected");
                }
                "mute_changed" => {
                    #[derive(serde::Deserialize)]
                    struct M {
                        #[serde(default)]
                        muted: bool,
                    }
                    if let Ok(m) = serde_json::from_str::<M>(&payload) {
                        core.muted = m.muted;
                    }
                }
                "monitoring_changed" => {
                    #[derive(serde::Deserialize)]
                    struct M {
                        #[serde(default)]
                        enabled: bool,
                    }
                    if let Ok(m) = serde_json::from_str::<M>(&payload) {
                        core.monitoring = m.enabled;
                    }
                }
                _ => {}
            }
        });
        mpl_result_t::MPL_OK
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn micyou_plugin_handle_message(
    _source: *const c_char,
    topic: *const c_char,
    payload: *const u8,
    payload_len: u32,
) -> mpl_result_t {
    guard(|| {
        let topic = unsafe { abi::cstr_or(topic, "") }.to_string();
        let bytes: &[u8] = if payload.is_null() || payload_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(payload, payload_len as usize) }
        };

        // ── WhatdidIsay (WDIS) broadcast: magic | start i64 | end i64 | text ──
        // Checked before topic routing: the broadcaster uses the host bus
        // broadcast and consumers identify frames by magic number only.
        if bytes.len() >= 20 && &bytes[0..4] == b"WDIS" {
            let text = String::from_utf8_lossy(&bytes[20..]).trim().to_string();
            if !text.is_empty() {
                let _ = with_core(|core| {
                    if core.cfg.wdis_enabled {
                        if let Some(h) = core.helper.as_ref() {
                            h.send(&cfw_protocol::Cmd::Wdis {
                                text,
                                hold_ms: core.cfg.wdis_hold_ms,
                            });
                        }
                    }
                });
            }
            return mpl_result_t::MPL_OK;
        }

        match topic.as_str() {
            "interval:tick" => {
                #[derive(serde::Deserialize)]
                struct Tick {
                    #[serde(default)]
                    interval: u64,
                    #[serde(default)]
                    payload: String,
                }
                let Ok(t) = serde_json::from_slice::<Tick>(bytes) else {
                    return mpl_result_t::MPL_OK;
                };
                // Only OUR interval with OUR tag (stale timers from previous
                // plugin instances can still be routed here by plugin id).
                if t.payload != TICK_PAYLOAD {
                    return mpl_result_t::MPL_OK;
                }
                let _ = with_core(|core| {
                    if core.interval_id != 0 && t.interval != core.interval_id {
                        return; // stale timer id → ignore
                    }
                    core.tick();
                });
            }
            other => {
                if let Some(action) = other.strip_prefix("ui:") {
                    let action = action.to_string();
                    let body = bytes.to_vec();
                    let _ = with_core(move |core| core.ui_action(&action, &body));
                }
                // Unknown topics (hotkey:*, timer:expired, broadcast…) ignored.
            }
        }
        mpl_result_t::MPL_OK
    })
}

// NOTE: `micyou_plugin_process` is intentionally absent — this plugin never
// joins the real-time DSP chain (kind = "ui").

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_name_validation() {
        assert!(valid_theme_name("ring.slint"));
        assert!(valid_theme_name("my_theme-2.slint"));
        assert!(!valid_theme_name("../evil.slint"));
        assert!(!valid_theme_name("a/b.slint"));
        assert!(!valid_theme_name("x.slint.exe"));
        assert!(!valid_theme_name(""));
        assert!(!valid_theme_name(".slint"));
        assert!(valid_theme_name(&format!("{}.slint", "a".repeat(58))));
        assert!(!valid_theme_name(&format!("{}.slint", "a".repeat(64))));
    }
}
