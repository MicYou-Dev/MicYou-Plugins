//! stdout is the event channel to the plugin — everything else logs to stderr.

use cfw_protocol::{encode_ev, Ev};
use std::io::{LineWriter, Write};
use std::sync::{Mutex, OnceLock};

static OUT: OnceLock<Mutex<LineWriter<std::io::Stdout>>> = OnceLock::new();

fn out() -> &'static Mutex<LineWriter<std::io::Stdout>> {
    OUT.get_or_init(|| Mutex::new(LineWriter::new(std::io::stdout())))
}

/// Emit one protocol event (JSON line) to the plugin. Never panics.
pub fn emit(ev: &Ev) {
    if let Ok(mut w) = out().lock() {
        let _ = writeln!(w, "{}", encode_ev(ev));
        let _ = w.flush();
    }
}

/// Diagnostic line to the plugin log (also echoed to stderr).
pub fn log(msg: &str) {
    eprintln!("[cfw-helper] {msg}");
    emit(&Ev::Log {
        message: msg.to_string(),
    });
}

/// Panic hook: report on stderr AND stdout (the plugin captures both).
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("helper panic: {info}");
        eprintln!("{msg}");
        emit(&Ev::Log { message: msg });
    }));
}
