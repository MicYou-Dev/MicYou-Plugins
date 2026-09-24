//! Lifecycle of the Slint helper **child process** (the floating window UI).
//!
//! Why a child process (see docs/TECHNICAL.md §3 for the full rationale):
//! * macOS requires a UI event loop on the process **main thread** — already
//!   owned by the Tauri host, so an in-process Slint loop cannot exist there.
//! * MicYou's plugin threading contract forbids calling Host APIs from
//!   plugin-created threads; a separate process with pipe IPC makes the
//!   boundary structural instead of disciplinary.
//! * User-authored `.slint` themes are compiled at runtime — a broken theme or
//!   a renderer/driver fault crashes only the helper, never the host.
//!
//! Pipe topology (all JSON lines, `cfw-protocol`):
//! * helper stdin  ← writer thread  ← [`HelperProc::send`] (never blocks the
//!   caller: an unbounded MPSC hands off immediately; the writer thread owns
//!   the pipe and dies on write error)
//! * helper stdout → reader thread → capped event deque → drained on host
//!   ticks ([`HelperProc::drain_events`])
//! * helper stderr → tail thread (last lines kept for post-mortem logging —
//!   GUI hosts have no console)
//!
//! Orphan safety, both directions:
//! * host dies → helper sees stdin EOF → exits
//! * helper dies → reader thread sees EOF → [`HelperProc::is_dead`] → the
//!   tick loop restarts it with backoff

use cfw_protocol::{decode_ev, encode_cmd, Cmd, Ev};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Cap of the undrained event queue (a stuck plugin tick must not grow it
/// unboundedly; oldest events are dropped past the cap).
const EVENT_CAP: usize = 64;
/// Stderr lines kept for post-mortem logging.
const STDERR_TAIL: usize = 12;
/// Grace period for a clean `quit` before kill.
const QUIT_GRACE: Duration = Duration::from_millis(1500);
const QUIT_POLL: Duration = Duration::from_millis(50);

/// Resolve the helper executable for the current OS/arch inside the plugin
/// install dir. Layout produced by CI (`bin/` subdir) with a flat fallback
/// for hand-made development installs:
///   bin/floating-helper-{windows|linux|macos}-{x86_64|aarch64}[.exe]
pub fn helper_exe_path(plugin_dir: &Path) -> Option<PathBuf> {
    // Development override wins (mock-host runs, hand builds).
    if let Ok(p) = std::env::var("MICYOU_FW_HELPER") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let name = format!(
        "floating-helper-{os}-{arch}{}",
        if cfg!(windows) { ".exe" } else { "" }
    );
    let sub = plugin_dir.join("bin").join(&name);
    if sub.is_file() {
        return Some(sub);
    }
    let flat = plugin_dir.join(&name);
    if flat.is_file() {
        return Some(flat);
    }
    None
}

pub struct HelperProc {
    child: Child,
    tx: std::sync::mpsc::Sender<String>,
    events: Arc<Mutex<VecDeque<Ev>>>,
    dead: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    threads: Vec<JoinHandle<()>>,
    pub started_at: Instant,
    /// Theme the helper was launched with (diagnostics only).
    #[allow(dead_code)]
    pub theme_path: PathBuf,
}

impl HelperProc {
    /// Spawn the helper for `theme_path`. `pos` = (x, y) physical, negative
    /// meaning "default placement".
    pub fn spawn(plugin_dir: &Path, theme_path: &Path, pos: (i32, i32)) -> std::io::Result<Self> {
        let exe = helper_exe_path(plugin_dir).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "helper executable not found under {} (bin/floating-helper-<os>-<arch>)",
                    plugin_dir.display()
                ),
            )
        })?;

        let mut cmd = Command::new(&exe);
        cmd.arg("--theme").arg(theme_path);
        if pos.0 >= 0 && pos.1 >= 0 {
            cmd.arg("--x").arg(pos.0.to_string()).arg("--y").arg(pos.1.to_string());
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(plugin_dir);
        // No console window flash on Windows GUI hosts.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let dead = Arc::new(AtomicBool::new(false));
        let events: Arc<Mutex<VecDeque<Ev>>> = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = std::sync::mpsc::channel::<String>();

        // stdin writer thread — owns the pipe; exits on channel close or error.
        let dead_w = dead.clone();
        let writer = std::thread::Builder::new()
            .name("cfw-helper-stdin".into())
            .spawn(move || writer_thread(stdin, rx, dead_w))?;

        // stdout reader thread — decodes events; EOF marks the child dead.
        let dead_r = dead.clone();
        let events_r = events.clone();
        let reader = std::thread::Builder::new()
            .name("cfw-helper-stdout".into())
            .spawn(move || reader_thread(stdout, events_r, dead_r))?;

        // stderr tail thread — keeps the last lines for crash diagnostics.
        let tail_t = stderr_tail.clone();
        let tailer = std::thread::Builder::new()
            .name("cfw-helper-stderr".into())
            .spawn(move || stderr_thread(stderr, tail_t))?;

        Ok(Self {
            child,
            tx,
            events,
            dead,
            stderr_tail,
            threads: vec![writer, reader, tailer],
            started_at: Instant::now(),
            theme_path: theme_path.to_path_buf(),
        })
    }

    /// Queue a command for the helper. Non-blocking; returns false when the
    /// writer thread is gone (child dead) — callers treat that as "restart
    /// needed" on the next tick.
    pub fn send(&self, cmd: &Cmd) -> bool {
        self.tx.send(encode_cmd(cmd)).is_ok()
    }

    /// Take all pending events (called on host-dispatched ticks only).
    pub fn drain_events(&self) -> Vec<Ev> {
        let mut out = Vec::new();
        if let Ok(mut q) = self.events.lock() {
            out.extend(q.drain(..));
        }
        out
    }

    /// True when the child is gone or the pipes broke. Never blocks.
    pub fn is_dead(&mut self) -> bool {
        if self.dead.load(Ordering::Relaxed) {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.dead.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Stderr lines captured so far (crash diagnostics).
    pub fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Ask the helper to quit, wait [`QUIT_GRACE`], then kill. Joins the pipe
    /// threads (they exit promptly once the pipes close).
    pub fn shutdown(mut self) {
        let _ = self.tx.send(encode_cmd(&Cmd::Quit));
        let deadline = Instant::now() + QUIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(QUIT_POLL),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // Closing the channel wakes the writer thread; reader/tailer see EOF.
        drop(self.tx);
        for t in self.threads.into_iter() {
            let _ = t.join();
        }
    }
}

fn writer_thread(
    mut stdin: ChildStdin,
    rx: std::sync::mpsc::Receiver<String>,
    dead: Arc<AtomicBool>,
) {
    while let Ok(line) = rx.recv() {
        if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
            break;
        }
    }
    dead.store(true, Ordering::Relaxed);
}

fn reader_thread(
    stdout: ChildStdout,
    events: Arc<Mutex<VecDeque<Ev>>>,
    dead: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(ev) = decode_ev(&line) {
            if let Ok(mut q) = events.lock() {
                if q.len() >= EVENT_CAP {
                    q.pop_front();
                }
                q.push_back(ev);
            }
        }
        // Undecodable lines are ignored (forward tolerance).
    }
    dead.store(true, Ordering::Relaxed);
}

fn stderr_thread(stderr: ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Some(Ok(line)) = lines.next() {
        if let Ok(mut q) = tail.lock() {
            if q.len() >= STDERR_TAIL {
                q.pop_front();
            }
            q.push_back(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single test on purpose: both cases mutate the process-wide
    // MICYOU_FW_HELPER env var and must not race with each other.
    #[test]
    fn helper_exe_resolution() {
        std::env::remove_var("MICYOU_FW_HELPER");
        // 1) missing dir → None
        assert!(helper_exe_path(Path::new("/definitely/not/here")).is_none());
        // 2) env override pointing at an existing file wins
        let me = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/helper.rs");
        std::env::set_var("MICYOU_FW_HELPER", &me);
        assert!(
            helper_exe_path(Path::new("/nonexistent")).is_some(),
            "env override must win when the file exists"
        );
        // 3) env override pointing at a missing file is ignored
        std::env::set_var("MICYOU_FW_HELPER", "/definitely/not/here/helper");
        assert!(helper_exe_path(Path::new("/definitely/not/here")).is_none());
        std::env::remove_var("MICYOU_FW_HELPER");
    }
}
