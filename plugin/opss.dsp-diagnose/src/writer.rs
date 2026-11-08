//! WAV writer thread.
//!
//! A single background thread drains every tap's lock-free ring and appends it
//! to a `timestamp-nodeName.wav` file. Nothing here runs on the audio thread.

use crate::engine::{Engine, Tap};
use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Recover a poisoned lock instead of silently stopping a tap's writer.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Samples of headroom per tap (about 5.4 s of mono 48 kHz audio).
pub const RING_SAMPLES: usize = 1 << 18;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RIFF_DATA: u64 = 0xFFFF_0000;

pub struct TapWriterState {
    pub file: Option<File>,
    pub path: PathBuf,
    /// Directory the current file was opened in, so a config change rotates
    /// the file even when the new path compares equal lexically.
    pub dir: PathBuf,
    pub data_bytes: u64,
    pub opened_at: Instant,
    pub last_flush: Instant,
    pub channels: u16,
    pub bits: u8,
    pub sample_rate: u32,
}

impl Default for TapWriterState {
    fn default() -> Self {
        Self {
            file: None,
            path: PathBuf::new(),
            dir: PathBuf::new(),
            data_bytes: 0,
            opened_at: Instant::now(),
            last_flush: Instant::now(),
            channels: 1,
            bits: 16,
            sample_rate: 48_000,
        }
    }
}

pub fn spawn(engine: &'static Engine) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("dsp-diagnose-writer".to_string())
        .spawn(move || run(engine))
}

fn run(engine: &'static Engine) {
    let mut scratch: Vec<f32> = Vec::with_capacity(RING_SAMPLES);
    loop {
        // The writer is the ring's only consumer, so clearing here cannot race
        // the audio producer: it only advances the consumer cursor. Clearing
        // from `Engine::start` would race this thread's tail store. A file left
        // open by an immediate stop/start is finalized so a new take starts in
        // a new file.
        if engine.discard.swap(false, Ordering::AcqRel) {
            for tap in engine.taps.iter() {
                let mut state = lock(&tap.writer);
                if state.file.is_some() {
                    finalize(engine, tap, &mut state);
                }
                tap.ring.clear();
            }
        }

        let stopping = engine.stop.load(Ordering::Acquire);
        let recording = engine.recording.load(Ordering::Acquire);
        let node_count = engine.node_count.load(Ordering::Acquire);

        for (index, tap) in engine.taps.iter().enumerate() {
            scratch.clear();
            let available = tap.ring.pop_into(&mut scratch);
            let enabled = tap.enabled.load(Ordering::Relaxed) && index < node_count;

            let mut state = lock(&tap.writer);
            if !recording || !enabled {
                if state.file.is_some() {
                    finalize(engine, tap, &mut state);
                }
                continue;
            }

            // A format or destination change starts a new file instead of
            // interleaving incompatible frames into the current one. The
            // samples already popped above go into the new file, not the old.
            let channels = engine.channels.load(Ordering::Relaxed).max(1);
            let bits = engine.bits.load(Ordering::Relaxed);
            let rate = engine.sample_rate.load(Ordering::Relaxed).max(8_000);
            if state.file.is_some()
                && (state.channels as usize != channels
                    || state.bits != bits
                    || state.sample_rate != rate
                    || output_dir_changed(engine, &*state))
            {
                finalize(engine, tap, &mut state);
            }

            if available == 0 {
                maybe_flush(engine, tap, &mut state);
                continue;
            }

            if state.file.is_none() && !open_file(engine, tap, &mut state, index) {
                continue;
            }
            write_samples(engine, tap, &mut state, &scratch);

            let segment = engine.segment_seconds.load(Ordering::Relaxed);
            let elapsed = state.opened_at.elapsed().as_secs();
            if state.data_bytes >= MAX_RIFF_DATA
                || (segment > 0 && elapsed >= segment)
            {
                finalize(engine, tap, &mut state);
            } else {
                maybe_flush(engine, tap, &mut state);
            }
        }

        if stopping {
            for tap in engine.taps.iter() {
                let mut state = lock(&tap.writer);
                if state.file.is_some() {
                    finalize(engine, tap, &mut state);
                }
            }
            break;
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

fn output_dir_changed(engine: &Engine, state: &TapWriterState) -> bool {
    let dir = lock(&engine.output_dir);
    state.dir.as_path() != dir.as_path()
}

fn open_file(engine: &Engine, tap: &Tap, state: &mut TapWriterState, index: usize) -> bool {
    let dir = lock(&engine.output_dir).clone();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        engine.log_once(format!(
            "cannot create output directory {}: {error}",
            dir.display()
        ));
        return false;
    }

    let name = lock(&tap.name).clone();
    let timestamp = Local::now().format("%Y%m%d%H%M%S").to_string();
    let mut path = dir.join(format!("{timestamp}-{name}.wav"));
    let mut suffix = 2;
    while path.exists() {
        path = dir.join(format!("{timestamp}-{name}-{suffix}.wav"));
        suffix += 1;
    }

    let file = match OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            engine.log_once(format!("cannot create {}: {error}", path.display()));
            return false;
        }
    };

    let channels = engine.channels.load(Ordering::Relaxed).max(1) as u16;
    let bits = engine.bits.load(Ordering::Relaxed);
    let sample_rate = engine.sample_rate.load(Ordering::Relaxed).max(8_000);

    state.file = Some(file);
    state.path = path.clone();
    state.dir = dir;
    state.data_bytes = 0;
    state.opened_at = Instant::now();
    state.last_flush = Instant::now();
    state.channels = channels;
    state.bits = bits;
    state.sample_rate = sample_rate;

    if let Some(file) = state.file.as_mut() {
        if write_header(file, channels, sample_rate, bits, 0).is_err() {
            engine.log_once(format!("cannot write WAV header to {}", path.display()));
            state.file = None;
            // Do not leave a zero-byte .wav behind that the next attempt would
            // have to work around with a "-2" suffix.
            let _ = std::fs::remove_file(&path);
            return false;
        }
    }

    tap.files.fetch_add(1, Ordering::Relaxed);
    *lock(&tap.current) = path.display().to_string();
    engine.log_once(format!(
        "recording node {} -> {}",
        index + 1,
        path.display()
    ));
    true
}

fn write_samples(engine: &Engine, tap: &Tap, state: &mut TapWriterState, samples: &[f32]) {
    let channels = state.channels.max(1) as usize;
    let frames = samples.len() / channels;
    let usable = frames * channels;
    if usable == 0 {
        return;
    }

    let bits = state.bits;
    let mut bytes: Vec<u8> = Vec::with_capacity(usable * (bits as usize / 8));
    match bits {
        24 => {
            for sample in &samples[..usable] {
                let value = (sample.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
                bytes.extend_from_slice(&value.to_le_bytes()[..3]);
            }
        }
        32 => {
            for sample in &samples[..usable] {
                bytes.extend_from_slice(&sample.clamp(-1.0, 1.0).to_le_bytes());
            }
        }
        _ => {
            for sample in &samples[..usable] {
                let value = (sample.clamp(-1.0, 1.0) * 32_767.0) as i16;
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }

    let Some(file) = state.file.as_mut() else {
        return;
    };
    if let Err(error) = file.write_all(&bytes) {
        engine.log_once(format!("write failed for {}: {error}", state.path.display()));
        finalize(engine, tap, state);
        return;
    }
    state.data_bytes += bytes.len() as u64;
    // `Tap::bytes` is the cumulative size written for this node, so the panel
    // keeps showing a useful size after a segment rotation or a stop.
    tap.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
}

fn maybe_flush(engine: &Engine, tap: &Tap, state: &mut TapWriterState) {
    if state.file.is_none() || state.last_flush.elapsed() < FLUSH_INTERVAL {
        return;
    }
    state.last_flush = Instant::now();
    let data_len = state.data_bytes.min(MAX_RIFF_DATA) as u32;
    let Some(file) = state.file.as_mut() else {
        return;
    };
    let result = (|| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        write_header(
            file,
            state.channels,
            state.sample_rate,
            state.bits,
            data_len,
        )?;
        file.seek(SeekFrom::End(0))?;
        file.flush()
    })();
    if let Err(error) = result {
        engine.log_once(format!(
            "flush failed for {}: {error}",
            state.path.display()
        ));
        finalize(engine, tap, state);
    }
}

fn finalize(engine: &Engine, tap: &Tap, state: &mut TapWriterState) {
    let Some(mut file) = state.file.take() else {
        return;
    };
    let data_len = state.data_bytes.min(MAX_RIFF_DATA) as u32;
    let result = (|| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        write_header(
            &mut file,
            state.channels,
            state.sample_rate,
            state.bits,
            data_len,
        )?;
        file.seek(SeekFrom::End(0))?;
        file.flush()
    })();
    if let Err(error) = result {
        engine.log_once(format!(
            "finalize failed for {}: {error}",
            state.path.display()
        ));
    } else {
        *lock(&tap.last) = state.path.display().to_string();
    }
    lock(&tap.current).clear();
    state.data_bytes = 0;
    state.path = PathBuf::new();
}

fn write_header(
    file: &mut File,
    channels: u16,
    sample_rate: u32,
    bits: u8,
    data_len: u32,
) -> std::io::Result<()> {
    let block_align = channels.max(1) * (bits as u16 / 8);
    let byte_rate = sample_rate * block_align as u32;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    // 1 = PCM, 3 = IEEE float
    header.extend_from_slice(&(if bits == 32 { 3u16 } else { 1u16 }).to_le_bytes());
    header.extend_from_slice(&channels.max(1).to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&(bits as u16).to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len.to_le_bytes());
    debug_assert_eq!(header.len(), 44);
    file.write_all(&header)
}
