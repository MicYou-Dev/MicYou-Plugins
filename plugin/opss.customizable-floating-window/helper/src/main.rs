//! cfw-helper — the Slint floating-window process of the MicYou
//! `opss.customizable-floating-window` plugin.
//!
//! Runs the Slint event loop on this process' **main thread** (mandatory on
//! macOS; correct everywhere), compiles the user-selected `.slint` theme at
//! runtime with `slint-interpreter`, and talks JSON lines with the plugin
//! core over stdin (commands) / stdout (events). See `cfw-protocol` and
//! docs/TECHNICAL.md.
//!
//! ```text
//! MicYou host ──C ABI──▶ plugin cdylib ──stdin──▶ THIS PROCESS ──▶ Slint window
//!                          ▲                        │
//!                          └───────stdout───────────┘   (all Host API calls stay
//!                                                        on host-dispatched threads)
//! ```

mod arc;
mod ipc;
mod platform;
mod theme;

use cfw_protocol::{Cmd, Ev, StatePayload};
use slint::{ComponentHandle as _, PhysicalPosition, Timer, TimerMode, Weak};
use slint_interpreter::Value;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use theme::LoadedTheme;

/// Shared between the stdin reader thread and the UI (event-loop) thread.
struct Shared {
    /// Latest state frame from the plugin (reader writes, UI reads).
    target: Mutex<Option<StatePayload>>,
    /// Weak handle of the *current* component instance, so the reader thread
    /// can post work to the event loop across a theme hot-swap.
    slot: Mutex<Option<Weak<slint_interpreter::ComponentInstance>>>,
    /// Initial position request from the command line (consumed once).
    initial_pos: Mutex<Option<(i32, i32)>>,
}

/// UI-thread-only state (main thread == event-loop thread; `invoke` closures
/// run here too, so a thread_local is the safe home for non-Send data).
struct Ui {
    theme: Option<LoadedTheme>,
    /// Keep-alive of repeating timers (dropping a Timer stops it).
    timers: Vec<Timer>,
    smooth: f64,
    last_applied_smooth: f64,
    /// Wave-bar animation phase (degrees), advanced by the smoothing timer.
    phase: f64,
    dragging: bool,
    /// Last global-cursor sample during a drag (physical px).
    drag_last: Option<(f64, f64)>,
    drag_scale: f32,
    drag_events: u32,
    last_pos: Option<PhysicalPosition>,
    /// Auto-hide timer for the WDIS transcript area.
    wdis_timer: Option<Timer>,
}

impl Ui {
    const fn new() -> Self {
        Self {
            theme: None,
            timers: Vec::new(),
            smooth: 0.0,
            last_applied_smooth: f64::NAN,
            phase: 0.0,
            dragging: false,
            drag_last: None,
            drag_scale: 1.0,
            drag_events: 0,
            last_pos: None,
            wdis_timer: None,
        }
    }
}

thread_local! {
    static UI: RefCell<Ui> = const { RefCell::new(Ui::new()) };
}

fn main() {
    let args = Args::parse();
    ipc::install_panic_hook();

    if let Some(path) = args.compile_check {
        std::process::exit(compile_check(&path));
    }

    // macOS: hide from Dock/Cmd-Tab (asserted again after the loop starts —
    // winit may reset the policy when it initializes NSApplication).
    platform::macos_set_accessory_policy();

    let shared = Arc::new(Shared {
        target: Mutex::new(None),
        slot: Mutex::new(None),
        initial_pos: Mutex::new(match (args.x, args.y) {
            (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x, y)),
            _ => None,
        }),
    });

    if let Some(prefix) = args.selftest {
        run_selftest(&args.theme, prefix, shared);
        return;
    }

    run_normal(&args.theme, shared);
}

struct Args {
    theme: PathBuf,
    x: Option<i32>,
    y: Option<i32>,
    compile_check: Option<PathBuf>,
    selftest: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let mut a = Self {
            theme: PathBuf::from("ring.slint"),
            x: None,
            y: None,
            compile_check: None,
            selftest: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--theme" => {
                    if let Some(v) = it.next() {
                        a.theme = PathBuf::from(v);
                    }
                }
                "--x" => a.x = it.next().and_then(|v| v.parse().ok()),
                "--y" => a.y = it.next().and_then(|v| v.parse().ok()),
                "--compile-check" => a.compile_check = it.next().map(PathBuf::from),
                "--selftest" => a.selftest = Some(it.next().unwrap_or_else(|| "cfw".into())),
                "--version" => {
                    println!("cfw-helper {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--help" => {
                    println!(
                        "cfw-helper — Slint floating window for MicYou\n\
                         usage: floating_helper --theme <file.slint> [--x N --y N]\n\
                                floating_helper --compile-check <file.slint>\n\
                                floating_helper --theme <file.slint> --selftest <png-prefix>\n\
                         normal mode reads JSON commands on stdin, writes events on stdout"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("[cfw-helper] ignoring unknown arg {other}"),
            }
        }
        a
    }
}

// ───────────────────────── compile-check mode ─────────────────────────

/// Compile a theme without opening any window; print a contract report.
/// Exit 0 = compiles cleanly, 1 = errors.
fn compile_check(path: &Path) -> i32 {
    let Ok(src) = std::fs::read_to_string(path) else {
        eprintln!("cannot read {}", path.display());
        return 1;
    };
    match theme::compile(src, path) {
        Ok(def) => {
            let props: Vec<String> = def.properties().map(|(n, _)| n).collect();
            let cbs: Vec<String> = def.callbacks().collect();
            println!("OK {}", path.display());
            println!("  component : {}", def.name());
            println!("  properties: {}", props.join(", "));
            println!("  callbacks : {}", cbs.join(", "));
            let known_p = [
                "input-level",
                "processed-level",
                "smooth-level",
                "level-percent",
                "muted",
                "streaming",
                "monitoring",
                "sample-rate",
                "channels",
                "queued-ms",
                "session-seconds",
                "session-text",
                "device-label",
                "device-mode",
                "info-text",
                "arc-path",
                "bars-path",
            ];
            let known_c = [
                theme::CB_MUTE,
                theme::CB_MONITORING,
                theme::CB_MENU,
                theme::CB_HIDE,
                theme::CB_DRAG_START,
                theme::CB_DRAG_MOVE,
                theme::CB_DRAG_END,
            ];
            let miss_p: Vec<&str> = known_p
                .iter()
                .copied()
                .filter(|p| !props.iter().any(|n| n == p))
                .collect();
            let miss_c: Vec<&str> = known_c
                .iter()
                .copied()
                .filter(|c| !cbs.iter().any(|n| n == *c))
                .collect();
            if !miss_p.is_empty() {
                println!(
                    "  note: properties not bound (ignored at runtime): {}",
                    miss_p.join(", ")
                );
            }
            if !miss_c.is_empty() {
                println!(
                    "  note: callbacks not present (ignored at runtime): {}",
                    miss_c.join(", ")
                );
            }
            0
        }
        Err(e) => {
            eprintln!("COMPILE ERRORS:\n{e}");
            1
        }
    }
}

// ───────────────────────── normal mode ─────────────────────────

fn run_normal(theme_path: &Path, shared: Arc<Shared>) {
    // Initial theme (falls back to the bundled ring so there is always a window).
    let (res, err) = theme::load_with_fallback(theme_path);
    let theme = match res {
        Ok(t) => t,
        Err(e) => {
            // Even the bundled fallback failed (should never happen) — bail out.
            let primary = err.unwrap_or_default();
            ipc::emit(&Ev::ThemeError {
                message: format!("fatal: cannot load any theme: {e}"),
            });
            eprintln!("[cfw-helper] fatal: primary theme error: {primary}\n[cfw-helper] fatal: fallback error: {e}");
            std::process::exit(2);
        }
    };
    if let Some(msg) = err {
        ipc::emit(&Ev::ThemeError { message: msg });
    }

    activate_theme(theme, shared.clone(), false);

    // stdin reader thread — parses commands; posts UI work to the event loop.
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("cfw-stdin".into())
        .spawn(move || stdin_thread(sh))
        .expect("spawn stdin thread");

    if let Err(e) = slint::run_event_loop() {
        eprintln!("[cfw-helper] event loop error: {e}");
        ipc::emit(&Ev::Log {
            message: format!("event loop error: {e}"),
        });
        std::process::exit(3);
    }
}

/// Show a (new) theme instance, wire callbacks/timers, position the window,
/// apply platform flags. Runs on the UI thread. On hot-swap the previous
/// window is dropped *after* the new one is shown (the event loop must never
/// see zero visible windows, or Slint quits it).
fn activate_theme(theme: LoadedTheme, shared: Arc<Shared>, reloaded: bool) {
    // Remember where the old window was (reuse on hot-swap).
    let old_pos = UI.with(|u| {
        let u = u.borrow();
        u.theme.as_ref().and_then(|t| {
            platform::position_api_usable().then(|| theme::window_of(t).position())
        })
    });

    attach_callbacks(&theme);

    if let Err(e) = theme.instance.show() {
        ipc::emit(&Ev::ThemeError {
            message: format!("cannot show window: {e}"),
        });
        return;
    }

    let name = theme.name.clone();

    // Replace the current theme; the old instance (and its window) drops here.
    UI.with(|u| {
        let mut u = u.borrow_mut();
        if let Some(old) = u.theme.take() {
            let _ = theme::window_of(&old).hide();
        }
        u.theme = Some(theme);
        u.timers.clear();
        u.smooth = 0.0;
        u.last_applied_smooth = f64::NAN;
        u.dragging = false;
        u.drag_last = None;
        u.drag_events = 0;
        u.last_pos = None;
    });

    // Publish the new weak handle for the stdin thread.
    let weak = UI.with(|u| u.borrow().theme.as_ref().map(|t| t.instance.as_weak()));
    if let Ok(mut slot) = shared.slot.lock() {
        *slot = weak;
    }

    // Apply the freshest state immediately.
    UI.with(|u| {
        let u = u.borrow();
        if let (Some(t), Ok(g)) = (u.theme.as_ref(), shared.target.lock()) {
            if let Some(st) = g.as_ref() {
                theme::apply_state(t, st, u.smooth);
            }
        }
    });

    // Position: reuse old → CLI arg → default top-right.
    place_window(old_pos, &shared);

    setup_timers(shared.clone());
    schedule_platform_flags(0);

    ipc::emit(&Ev::Ready {
        theme: name,
        reloaded,
    });
}

fn place_window(old_pos: Option<PhysicalPosition>, shared: &Arc<Shared>) {
    let initial = shared
        .initial_pos
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|(x, y)| PhysicalPosition::new(x, y));
    let placed = with_window(|win| {
        let pos = match old_pos.or(initial) {
            Some(p) => p,
            None => match (platform::screen_size(), win.size()) {
                (Some((sw, sh)), size) if sw > 0 && sh > 0 => {
                    let scale = win.scale_factor().max(1.0);
                    let margin = (24.0 * scale) as i32;
                    PhysicalPosition::new((sw - size.width as i32 - margin).max(0), margin.max(0))
                }
                _ => return None, // Wayland / unknown screen → WM places us
            },
        };
        win.set_position(pos);
        Some(pos)
    })
    .flatten();
    if let Some(pos) = placed {
        UI.with(|u| u.borrow_mut().last_pos = Some(pos));
    }
}

/// Retry chain: the raw window handle may only exist after the window is
/// mapped (first event-loop iterations), so try a few times.
fn schedule_platform_flags(tries: u32) {
    Timer::single_shot(
        Duration::from_millis(if tries == 0 { 150 } else { 200 }),
        move || {
            // Re-assert macOS accessory policy (winit may have set Regular).
            platform::macos_set_accessory_policy();
            let applied = with_window(|win| platform::apply_skip_taskbar(win));
            match applied {
                Some(a) if a != "no-handle" => {
                    ipc::log(&format!("window flags applied: {a}"));
                }
                _ if tries < 12 => schedule_platform_flags(tries + 1),
                _ => {}
            }
        },
    );
}

fn setup_timers(shared: Arc<Shared>) {
    // 30 Hz smoothing: EMA toward the latest raw level, then push
    // smooth-level / level-percent / arc-path into the theme.
    let smooth_timer = Timer::default();
    let sh = shared.clone();
    smooth_timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
        let Ok(g) = sh.target.lock() else { return };
        let Some(st) = g.as_ref() else { return };
        UI.with(|u| {
            let mut u = u.borrow_mut();
            let target = st.level.clamp(0.0, 1.0);
            u.smooth += (target - u.smooth) * 0.30;
            if (target - u.smooth).abs() < 0.004 {
                u.smooth = target;
            }
            // advance the bar animation phase while streaming & unmuted
            let animating = st.streaming && !st.muted && u.smooth > 0.02;
            if animating {
                u.phase = (u.phase + 6.0) % 360.0;
            }
            if !animating && (u.smooth - u.last_applied_smooth).abs() < 0.0015 {
                return; // nothing visible would change
            }
            u.last_applied_smooth = u.smooth;
            if let Some(t) = u.theme.as_ref() {
                theme::apply_smooth(t, u.smooth, u.phase);
            }
        });
    });

    // 1 Hz position poll — catches moves we didn't initiate ourselves
    // (WindowMoveArea system drags, WM moves). Suppressed while a custom
    // drag is running (drag-end reports those) and on Wayland (no API).
    let pos_timer = Timer::default();
    pos_timer.start(TimerMode::Repeated, Duration::from_millis(1000), || {
        // Windows: re-assert taskbar exclusion ~1 Hz (heals explorer restarts;
        // DeleteTab + exstyle are idempotent, µs-cost).
        #[cfg(windows)]
        {
            let _ = with_window(|w| platform::apply_skip_taskbar(w));
        }
        if !platform::position_api_usable() {
            return;
        }
        UI.with(|u| {
            let mut u = u.borrow_mut();
            if u.dragging {
                return;
            }
            let Some(t) = u.theme.as_ref() else {
                return;
            };
            let cur = theme::window_of(t).position();
            if cur.x == 0 && cur.y == 0 {
                return; // Wayland-style dummy position
            }
            let changed = match u.last_pos {
                Some(p) => (p.x - cur.x).abs() > 2 || (p.y - cur.y).abs() > 2,
                None => true,
            };
            u.last_pos = Some(cur);
            if changed {
                ipc::emit(&Ev::Moved { x: cur.x, y: cur.y });
            }
        });
    });

    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.timers.push(smooth_timer);
        u.timers.push(pos_timer);
    });
}

/// Run `f` against the current theme's window (if any). `slint::Window` is
/// not Clone, so window access always goes through a closure scoped to the
/// UI-state borrow.
fn with_window<R>(f: impl FnOnce(&slint::Window) -> R) -> Option<R> {
    UI.with(|u| u.borrow().theme.as_ref().map(|t| f(t.instance.window())))
}

fn attach_callbacks(theme: &LoadedTheme) {
    let inst = &theme.instance;
    let has = |name: &str| theme.callbacks.contains(name);

    if has(theme::CB_MUTE) {
        let _ = inst.set_callback(theme::CB_MUTE, |_| {
            ipc::emit(&Ev::Mute);
            Value::Void
        });
    }
    if has(theme::CB_MONITORING) {
        let _ = inst.set_callback(theme::CB_MONITORING, |_| {
            ipc::emit(&Ev::Monitoring);
            Value::Void
        });
    }
    if has(theme::CB_MENU) {
        let _ = inst.set_callback(theme::CB_MENU, |args| {
            let action = match args.first() {
                Some(Value::String(s)) => s.to_string(),
                _ => String::new(),
            };
            ipc::emit(&Ev::Menu { action });
            Value::Void
        });
    }
    if has(theme::CB_HIDE) {
        let _ = inst.set_callback(theme::CB_HIDE, |_| {
            ipc::emit(&Ev::Hide);
            // Hiding the last window quits the event loop → helper suspends.
            // The plugin persists visible=false and respawns on "show".
            let _ = with_window(|w| w.hide());
            Value::Void
        });
    }
    if has(theme::CB_DRAG_START) {
        let _ = inst.set_callback(theme::CB_DRAG_START, |_| {
            let scale = with_window(|w| w.scale_factor()).unwrap_or(1.0);
            let c0 = platform::global_cursor(scale);
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.drag_events = 0;
                u.dragging = true;
                u.drag_scale = scale;
                u.drag_last = c0;
            });
            Value::Void
        });
    }
    if has(theme::CB_DRAG_MOVE) {
        // Event-driven global-cursor delta: the cursor frame never depends on
        // the window position → 1:1 tracking, zero feedback loop (the old
        // window-relative protocol oscillated at half speed with real mice).
        let _ = inst.set_callback(theme::CB_DRAG_MOVE, |_| {
            let (dragging, scale, last) = UI.with(|u| {
                let mut u = u.borrow_mut();
                if !u.dragging {
                    return (false, u.drag_scale, u.drag_last);
                }
                u.drag_events += 1;
                (true, u.drag_scale, u.drag_last)
            });
            if !dragging || !platform::position_api_usable() {
                return Value::Void;
            }
            let Some(c) = platform::global_cursor(scale) else {
                return Value::Void;
            };
            if let Some(last) = last {
                let dx = c.0 - last.0;
                let dy = c.1 - last.1;
                if dx != 0.0 || dy != 0.0 {
                    let _ = with_window(|w| {
                        let p = w.position();
                        w.set_position(PhysicalPosition::new(
                            p.x + dx.round() as i32,
                            p.y + dy.round() as i32,
                        ))
                    });
                }
            }
            UI.with(|u| u.borrow_mut().drag_last = Some(c));
            Value::Void
        });
    }
    if has(theme::CB_DRAG_END) {
        let _ = inst.set_callback(theme::CB_DRAG_END, |_| {
            let events = UI.with(|u| {
                let mut u = u.borrow_mut();
                let n = u.drag_events;
                u.dragging = false;
                u.drag_last = None;
                n
            });
            if events >= 2 && platform::position_api_usable() {
                if let Some(p) = with_window(|w| w.position()) {
                    if !(p.x == 0 && p.y == 0) {
                        UI.with(|u| u.borrow_mut().last_pos = Some(p));
                        ipc::emit(&Ev::Moved { x: p.x, y: p.y });
                    }
                }
            }
            Value::Void
        });
    }

}

// ───────────────────────── stdin thread ─────────────────────────

fn stdin_thread(shared: Arc<Shared>) {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Some(cmd) = cfw_protocol::decode_cmd(&line) else {
            eprintln!("[cfw-helper] ignoring undecodable command line");
            continue;
        };
        match cmd {
            Cmd::State(payload) => {
                if let Ok(mut g) = shared.target.lock() {
                    *g = Some(payload);
                }
                // Push non-animated fields immediately (bools must feel instant).
                let sh = shared.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    UI.with(|u| {
                        let u = u.borrow();
                        if let Some(t) = u.theme.as_ref() {
                            if let Ok(g) = sh.target.lock() {
                                if let Some(st) = g.as_ref() {
                                    theme::apply_state(t, st, u.smooth);
                                }
                            }
                        }
                    });
                });
            }
            Cmd::Theme { path } => {
                let p = PathBuf::from(path);
                let sh = shared.clone();
                post_to_ui(&shared, move || reload_theme(p, sh));
            }
            Cmd::Visible { show } => {
                post_to_ui(&shared, move || {
                    let _ = with_window(|w| {
                        if show {
                            let _ = w.show();
                        } else {
                            let _ = w.hide();
                        }
                    });
                });
            }
            Cmd::Pos { x, y } => {
                post_to_ui(&shared, move || {
                    let p = PhysicalPosition::new(x, y);
                    let _ = with_window(|w| w.set_position(p));
                    UI.with(|u| u.borrow_mut().last_pos = Some(p));
                });
            }
            Cmd::PosDefault => {
                let sh = shared.clone();
                post_to_ui(&shared, move || {
                    if let Ok(mut g) = sh.initial_pos.lock() {
                        *g = None;
                    }
                    place_window(None, &sh);
                });
            }
            Cmd::Wdis { text, hold_ms } => {
                post_to_ui(&shared, move || show_wdis(text, hold_ms));
            }
            Cmd::Snapshot { path } => {
                post_to_ui(&shared, move || take_snapshot(&path));
            }
            Cmd::Quit => {
                let _ = slint::invoke_from_event_loop(|| {
                    ipc::emit(&Ev::Bye);
                    let _ = slint::quit_event_loop();
                });
                break;
            }
        }
    }
    // stdin EOF → the plugin/host is gone; do not linger as an orphan.
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

/// Post UI-thread work via the *current* instance weak (works across
/// hot-swaps); falls back to a plain invoke when no instance is alive.
fn post_to_ui(shared: &Arc<Shared>, f: impl FnOnce() + Send + 'static) {
    let weak = shared.slot.lock().ok().and_then(|g| g.clone());
    let work = Arc::new(Mutex::new(Some(f)));
    if let Some(w) = weak {
        let work2 = work.clone();
        let _ = w.upgrade_in_event_loop(move |_inst| {
            if let Ok(mut g) = work2.lock() {
                if let Some(f) = g.take() {
                    f();
                }
            }
        });
    } else {
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut g) = work.lock() {
                if let Some(f) = g.take() {
                    f();
                }
            }
        });
    }
}

/// Hot-swap the theme without restarting the process. On compile failure the
/// current window stays untouched and the error goes to the plugin/panel.
fn reload_theme(path: PathBuf, shared: Arc<Shared>) {
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            ipc::emit(&Ev::ThemeError {
                message: format!("cannot read {}: {e}", path.display()),
            });
            return;
        }
    };
    let def = match theme::compile(src, &path) {
        Ok(d) => d,
        Err(e) => {
            ipc::emit(&Ev::ThemeError { message: e });
            return;
        }
    };
    let new_theme = match theme::instantiate(def, &path) {
        Ok(t) => t,
        Err(e) => {
            ipc::emit(&Ev::ThemeError { message: e });
            return;
        }
    };
    // Success: swap (activate_theme emits Ready{reloaded:true}; the plugin
    // clears themeError when it sees Ready).
    activate_theme(new_theme, shared, true);
}

/// Show a WhatdidIsay transcript via the optional theme contract members
/// (`wdis-text` / `wdis-visible`) and schedule the auto-hide.
fn show_wdis(text: String, hold_ms: u64) {
    UI.with(|u| {
        let mut u = u.borrow_mut();
        if let Some(t) = u.theme.as_ref() {
            if t.props.contains("wdis-text") {
                let _ = t.instance.set_property("wdis-text", Value::String(text.into()));
            }
            if t.props.contains("wdis-visible") {
                let _ = t.instance.set_property("wdis-visible", Value::Bool(true));
            }
        }
        if let Some(old) = u.wdis_timer.take() {
            old.stop();
        }
        let t = Timer::default();
        t.start(TimerMode::SingleShot, Duration::from_millis(hold_ms), || {
            UI.with(|u| {
                let u = u.borrow();
                if let Some(t) = u.theme.as_ref() {
                    if t.props.contains("wdis-visible") {
                        let _ = t.instance.set_property("wdis-visible", Value::Bool(false));
                    }
                }
            });
        });
        u.wdis_timer = Some(t);
    });
}

fn take_snapshot(path: &str) {
    let res = UI.with(|u| {
        let u = u.borrow();
        let t = u.theme.as_ref()?;
        t.instance.window().take_snapshot().ok()
    });
    match res {
        Some(buf) => {
            let (w, h) = (buf.width(), buf.height());
            match std::fs::File::create(path) {
                Ok(file) => {
                    let mut enc = png::Encoder::new(file, w, h);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    let written = enc
                        .write_header()
                        .and_then(|mut wr| wr.write_image_data(buf.as_bytes()));
                    match written {
                        Ok(_) => ipc::log(&format!("snapshot written: {path} ({w}x{h})")),
                        Err(e) => ipc::log(&format!("snapshot encode failed: {e}")),
                    }
                }
                Err(e) => ipc::log(&format!("snapshot create failed: {e}")),
            }
        }
        None => ipc::log("snapshot unavailable"),
    }
}

// ───────────────────────── selftest mode ─────────────────────────

/// Synthetic run without a plugin on stdin: drives fake states, writes PNG
/// snapshots (`<prefix>-stream.png` / `-muted.png` / `-idle.png`) and exits.
/// Used by CI (under xvfb) and to generate docs images.
fn run_selftest(theme_path: &Path, prefix: String, shared: Arc<Shared>) {
    let (res, err) = theme::load_with_fallback(theme_path);
    let theme = match res {
        Ok(t) => t,
        Err(e) => {
            let primary = err.unwrap_or_default();
            eprintln!("selftest: primary theme error: {primary}\nselftest: fallback error: {e}");
            std::process::exit(2);
        }
    };
    if let Some(msg) = err {
        eprintln!("selftest: theme error (fallback in use): {msg}");
    }
    activate_theme(theme, shared.clone(), false);

    fn set_state(st: &StatePayload, shared: &Arc<Shared>) {
        if let Ok(mut g) = shared.target.lock() {
            *g = Some(st.clone());
        }
        UI.with(|u| {
            let u = u.borrow();
            if let Some(t) = u.theme.as_ref() {
                theme::apply_state(t, st, st.level);
                theme::apply_smooth(t, st.level, 0.0);
            }
        });
    }

    let streaming = StatePayload {
        level: 0.62,
        processed: 0.55,
        muted: false,
        streaming: true,
        monitoring: false,
        sample_rate: 48000,
        channels: 1,
        queued_ms: 12.5,
        session_seconds: 754,
        session_text: "12:34".into(),
        device_label: "MicYou Mobile".into(),
        device_mode: "wifi".into(),
    };
    set_state(&streaming, &shared);
    if std::env::var_os("CFW_SELFTEST_WDIS").is_some() {
        show_wdis("Hello everyone, let's start the meeting.".into(), 5000);
    }

    let p1 = format!("{prefix}-stream.png");
    let p2 = format!("{prefix}-muted.png");
    let p3 = format!("{prefix}-idle.png");
    let sh2 = shared.clone();
    let sh3 = shared.clone();

    Timer::single_shot(Duration::from_millis(900), move || {
        take_snapshot(&p1);
        let muted = StatePayload {
            muted: true,
            level: 0.3,
            ..streaming.clone()
        };
        set_state(&muted, &sh2);
        Timer::single_shot(Duration::from_millis(600), move || {
            take_snapshot(&p2);
            let idle = StatePayload {
                level: 0.0,
                processed: 0.0,
                streaming: false,
                session_seconds: 0,
                session_text: "00:00".into(),
                device_label: String::new(),
                device_mode: String::new(),
                ..streaming.clone()
            };
            set_state(&idle, &sh3);
            Timer::single_shot(Duration::from_millis(600), move || {
                take_snapshot(&p3);
                let _ = slint::quit_event_loop();
            });
        });
    });

    if let Err(e) = slint::run_event_loop() {
        eprintln!("selftest event loop error: {e}");
        std::process::exit(3);
    }
}
