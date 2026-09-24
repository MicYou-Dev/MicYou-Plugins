//! Runtime theme loading: compile a user `.slint` file with
//! `slint-interpreter` and bind it to the data/interaction contract.
//!
//! # The contract (documented for theme authors; everything is OPTIONAL)
//!
//! A theme is a `.slint` file exporting exactly one window-level component
//! (`export component X inherits Window { … }`). The helper introspects the
//! compiled [`ComponentDefinition`] and only touches members that exist, so
//! any subset works — from a 10-line status dot to a full HUD.
//!
//! **Input properties** (declare `in property<…>` on the root component):
//!
//! | property | type | meaning |
//! |---|---|---|
//! | `input-level` | float | raw mic RMS 0..1 |
//! | `processed-level` | float | post-DSP RMS 0..1 |
//! | `smooth-level` | float | EMA-smoothed level (helper-side, ~30 Hz) |
//! | `level-percent` | int | smoothed level × 100, rounded |
//! | `muted` | bool | host mute state (authoritative) |
//! | `streaming` | bool | audio session streaming |
//! | `monitoring` | bool | host ear-return state |
//! | `sample-rate` | int | Hz |
//! | `channels` | int | |
//! | `queued-ms` | float | output queue latency |
//! | `session-seconds` | int | uptime of the current connection |
//! | `session-text` | string | "M:SS" / "H:MM:SS" |
//! | `device-label` | string | connected device name ("" = unknown) |
//! | `device-mode` | string | "wifi" / "usb" / "web" / "" |
//! | `arc-path` | string | SVG arc `d` for a 100×100-viewbox ring, r=45, sweep = level×360° from 12 o'clock clockwise (smoothed) |
//!
//! **Output callbacks** (declare `callback …` on the root component; the
//! theme fires them from its own TouchAreas / Menus / WindowMoveAreas):
//!
//! | callback | args | helper reaction |
//! |---|---|---|
//! | `mute-toggle()` | – | asks the plugin to flip host mute |
//! | `monitoring-toggle()` | – | asks the plugin to flip ear-return |
//! | `menu-action(string)` | action id | forwarded to the plugin; `"reload-theme"` is handled specially |
//! | `hide-window()` | – | window hides immediately + plugin persists `visible=false` |
//! | `drag-start()` | – | helper records window position (custom drag protocol) |
//! | `drag-move(float,float)` | Δ logical px since press | helper moves the window (custom drag protocol) |
//! | `drag-end()` | – | helper reports the new position for persistence |
//!
//! The custom drag trio exists so themes can distinguish *click* from *drag*
//! (legacy v1 behavior: click without movement toggles mute). Themes that
//! don't need that distinction can just use Slint 1.18's built-in
//! `WindowMoveArea` element instead (system-managed move; the helper learns
//! the new position via its 1 s position poll).

use cfw_protocol::StatePayload;
use slint::ComponentHandle;
use slint_interpreter::{
    ComponentDefinition, ComponentInstance, Compiler, DiagnosticLevel, Value, ValueType,
};
use std::collections::HashSet;
use std::path::Path;

/// Bundled fallback theme (also shipped as `themes/ring.slint`) — used when
/// the configured theme fails to compile, so the user always has *a* window
/// and can fix the theme from the panel.
pub const FALLBACK_THEME: &str = include_str!("../../themes/ring.slint");

pub const CB_MUTE: &str = "mute-toggle";
pub const CB_MONITORING: &str = "monitoring-toggle";
pub const CB_MENU: &str = "menu-action";
pub const CB_HIDE: &str = "hide-window";
pub const CB_DRAG_START: &str = "drag-start";
pub const CB_DRAG_MOVE: &str = "drag-move";
pub const CB_DRAG_END: &str = "drag-end";

/// A compiled theme instance plus the member sets it actually implements.
pub struct LoadedTheme {
    pub instance: ComponentInstance,
    pub props: HashSet<String>,
    pub callbacks: HashSet<String>,
    pub name: String,
}

/// Compile `source` (diagnostics returned on failure).
pub fn compile(source: String, path: &Path) -> Result<ComponentDefinition, String> {
    let compiler = Compiler::new();
    let result = pollster::block_on(compiler.build_from_source(source, path.to_path_buf()));
    let errors: Vec<String> = result
        .diagnostics()
        .filter(|d| d.level() == DiagnosticLevel::Error)
        .map(|d| format_diagnostic(&d))
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    // Warnings go to stderr (the plugin surfaces the tail in its log).
    for d in result.diagnostics() {
        if d.level() == DiagnosticLevel::Warning {
            eprintln!("[theme warning] {}", format_diagnostic(&d));
        }
    }
    pick_component(&result, path)
}

fn pick_component(
    result: &slint_interpreter::CompilationResult,
    path: &Path,
) -> Result<ComponentDefinition, String> {
    // Prefer explicit names, then any window-level component, then the first.
    for name in ["App", "MainWindow", "FloatingWindow"] {
        if let Some(c) = result.component(name) {
            return Ok(c);
        }
    }
    // Otherwise take the first exported component (themes export exactly one
    // window-level component by contract).
    result
        .components()
        .next()
        .ok_or_else(|| format!("{}: no exported component found", path.display()))
}

fn format_diagnostic(d: &slint_interpreter::Diagnostic) -> String {
    // Display impl includes level, message and source location.
    d.to_string()
}

/// Compile the file at `path`, falling back to the bundled theme on any
/// failure. Returns the loaded theme + an optional error message for the
/// plugin (surfaced in the panel / notification).
pub fn load_with_fallback(path: &Path) -> (Result<LoadedTheme, String>, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(src) => match compile(src, path) {
            Ok(def) => match instantiate(def, path) {
                Ok(t) => (Ok(t), None),
                Err(e) => fallback(Some(format!("{}: {e}", path.display()))),
            },
            Err(e) => fallback(Some(e)),
        },
        Err(e) => fallback(Some(format!("cannot read {}: {e}", path.display()))),
    }
}

fn fallback(err: Option<String>) -> (Result<LoadedTheme, String>, Option<String>) {
    let name = "ring.slint (built-in fallback)".to_string();
    match compile(FALLBACK_THEME.to_string(), Path::new("ring.slint"))
        .and_then(|def| instantiate(def, Path::new("ring.slint")))
    {
        Ok(mut t) => {
            t.name = name;
            (Ok(t), err)
        }
        Err(e) => (Err(e), err),
    }
}

pub fn instantiate(def: ComponentDefinition, path: &Path) -> Result<LoadedTheme, String> {
    let instance = def
        .create()
        .map_err(|e| format!("{}: create instance: {e}", path.display()))?;
    let props = def
        .properties()
        .map(|(n, _t)| n)
        .collect::<HashSet<String>>();
    let callbacks = def.callbacks().collect::<HashSet<String>>();
    Ok(LoadedTheme {
        instance,
        props,
        callbacks,
        name: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
    })
}

/// Push one state frame into the theme (only members that exist).
pub fn apply_state(theme: &LoadedTheme, st: &StatePayload, smooth: f64) {
    let inst = &theme.instance;
    let set = |name: &str, v: Value| {
        if theme.props.contains(name) {
            let _ = inst.set_property(name, v);
        }
    };
    set("input-level", Value::Number(st.level));
    set("processed-level", Value::Number(st.processed));
    set("smooth-level", Value::Number(smooth));
    set(
        "level-percent",
        Value::Number((smooth.clamp(0.0, 1.0) * 100.0).round()),
    );
    set("muted", Value::Bool(st.muted));
    set("streaming", Value::Bool(st.streaming));
    set("monitoring", Value::Bool(st.monitoring));
    set("sample-rate", Value::Number(st.sample_rate as f64));
    set("channels", Value::Number(st.channels as f64));
    set("queued-ms", Value::Number(st.queued_ms));
    set("session-seconds", Value::Number(st.session_seconds as f64));
    set("session-text", Value::String(st.session_text.clone().into()));
    set("device-label", Value::String(st.device_label.clone().into()));
    set("device-mode", Value::String(st.device_mode.clone().into()));
    set("info-text", Value::String(info_text(st).into()));
    set("arc-path", Value::String(crate::arc::arc_path(smooth).into()));
}

/// Pre-formatted stream summary for themes that want text but can't convert
/// numbers to strings (Slint has no int→string in expressions).
fn info_text(st: &StatePayload) -> String {
    if st.sample_rate == 0 {
        return String::new();
    }
    format!(
        "{:.1}kHz · {}ch · q{:.0}ms",
        st.sample_rate as f64 / 1000.0,
        st.channels,
        st.queued_ms
    )
}

/// Push the smoothed level + derived values (called ~30 Hz by the smoothing
/// timer; kept separate from [`apply_state`] so the hot path stays tiny).
pub fn apply_smooth(theme: &LoadedTheme, smooth: f64, phase_deg: f64) {
    let inst = &theme.instance;
    let s = smooth.clamp(0.0, 1.0);
    let set = |name: &str, v: Value| {
        if theme.props.contains(name) {
            let _ = inst.set_property(name, v);
        }
    };
    set("smooth-level", Value::Number(s));
    set("level-percent", Value::Number((s * 100.0).round()));
    set("arc-path", Value::String(crate::arc::arc_path(s).into()));
    set("bars-path", Value::String(crate::arc::bars_path(s, phase_deg).into()));
}

/// Convenience: property type query (used by tests / diagnostics).
#[allow(dead_code)]
pub fn prop_type(def: &ComponentDefinition, name: &str) -> Option<ValueType> {
    def.properties().find(|(n, _)| n == name).map(|(_, t)| t)
}

/// Keep the instance alive & reachable for `ComponentHandle` methods.
pub fn window_of(theme: &LoadedTheme) -> &slint::Window {
    theme.instance.window()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_themes_compile() {
        // Guards against contract drift between shipped themes and helper.
        i_slint_backend_testing::init_no_event_loop();
        for name in ["ring.slint", "pill.slint", "minimal.slint"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../themes")
                .join(name);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let def = compile(src, &path).unwrap_or_else(|e| panic!("{name}: {e}"));
            let props: HashSet<String> = def.properties().map(|(n, _)| n).collect();
            let cbs: HashSet<String> = def.callbacks().collect();
            // Every theme must at least accept the core state and fire mute.
            for p in ["muted", "streaming", "input-level"] {
                assert!(props.contains(p), "{name} missing property {p}");
            }
            assert!(cbs.contains(CB_MUTE), "{name} missing callback {CB_MUTE}");
            // Instantiation must succeed on the (testing) platform too.
            let theme = instantiate(def, &path).unwrap_or_else(|e| panic!("{name}: {e}"));
            apply_state(
                &theme,
                &StatePayload {
                    level: 0.5,
                    muted: true,
                    session_text: "01:02".into(),
                    ..Default::default()
                },
                0.5,
            );
            assert!(matches!(
                theme.instance.get_property("muted"),
                Ok(Value::Bool(true))
            ));
        }
    }

    #[test]
    fn broken_theme_falls_back() {
        i_slint_backend_testing::init_no_event_loop();
        let dir = std::env::temp_dir().join("cfw-test-broken.slint");
        std::fs::write(&dir, "export component Broken inherits Window { this is not slint }")
            .unwrap();
        let (res, err) = load_with_fallback(&dir);
        assert!(err.is_some(), "error must be reported");
        let theme = res.expect("fallback must load");
        assert!(theme.name.contains("fallback"));
        let _ = std::fs::remove_file(&dir);
    }
}
