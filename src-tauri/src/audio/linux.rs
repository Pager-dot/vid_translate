use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc, Arc,
};

use super::{CaptureFault, SAMPLE_RATE};

// 250ms chunks — Vosk processes these fast enough for word-by-word output
const CHUNK_SAMPLES: usize = (SAMPLE_RATE / 4) as usize;
const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2; // S16LE = 2 bytes/sample

/// Mirrors the macOS fault atomic so `spawn_capture_watchdog` in lib.rs can poll both the
/// same way. Without this a missing `parec`, or a monitor source the sandbox won't hand us,
/// left the app looking like it was listening forever with captions that never arrived.
static CAPTURE_FAULT: AtomicU8 = AtomicU8::new(0);

fn set_fault(fault: CaptureFault) {
    CAPTURE_FAULT.store(fault as u8, Ordering::Relaxed);
}

pub fn capture_fault() -> Option<CaptureFault> {
    CaptureFault::from_u8(CAPTURE_FAULT.load(Ordering::Relaxed))
}

/// Resolve a real monitor source name.
///
/// `@DEFAULT_MONITOR@` used to be the fallback here, but that token is expanded by
/// `pacat`/the Pulse client config, *not* by `parec --device`, which passes it through as a
/// literal device name and fails. So every path below has to produce a name that exists.
fn resolve_monitor_source() -> Option<String> {
    // Preferred: the monitor of whichever sink is currently the default.
    if let Some(sink) = run_pactl(&["get-default-sink"]) {
        let sink = sink.trim();
        if !sink.is_empty() {
            return Some(format!("{}.monitor", sink));
        }
    }

    // Fallback: first source that looks like a monitor. Covers older pactl builds with no
    // `get-default-sink`, and setups where the default sink has no monitor of its own.
    let sources = run_pactl(&["list", "short", "sources"])?;
    sources
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .find(|name| name.ends_with(".monitor"))
        .map(str::to_owned)
}

fn run_pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Check that system-audio capture can actually work before a session claims to be
/// listening. Unlike macOS there is no permission gate — what fails here is the plumbing:
/// no PulseAudio/PipeWire server, no `pactl`/`parec` on PATH, or no monitor source.
pub fn preflight() -> Result<(), CaptureFault> {
    CAPTURE_FAULT.store(0, Ordering::Relaxed);

    match resolve_monitor_source() {
        Some(_) => Ok(()),
        None => {
            set_fault(CaptureFault::TapUnavailable);
            Err(CaptureFault::TapUnavailable)
        }
    }
}

/// Start capturing system loopback audio via `parec`.
/// Returns a Receiver yielding 250ms 16kHz mono i16 chunks.
pub fn start_capture(stop: Arc<AtomicBool>) -> mpsc::Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let device = match resolve_monitor_source() {
            Some(d) => d,
            None => {
                eprintln!("[audio] No PulseAudio monitor source available");
                set_fault(CaptureFault::TapUnavailable);
                return;
            }
        };
        eprintln!("[audio] Recording from: {}", device);

        let mut child = match Command::new("parec")
            .args([
                "--device", &device,
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
                "--latency-msec=50",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[audio] Failed to spawn parec: {}", e);
                set_fault(CaptureFault::TapUnavailable);
                return;
            }
        };

        let mut stdout = child.stdout.take().expect("parec stdout");
        let mut raw_buf = vec![0u8; CHUNK_BYTES];

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if let Err(e) = read_exact(&mut stdout, &mut raw_buf, &stop) {
                if !stop.load(Ordering::Relaxed) {
                    eprintln!("[audio] Read error: {}", e);
                    // parec died (or never produced a byte) while we still wanted audio.
                    // Either way the session is over and the user needs to be told.
                    set_fault(CaptureFault::NoAudioFrames);
                }
                break;
            }
            // S16LE bytes → i16 samples (Vosk native format)
            let samples: Vec<i16> = raw_buf
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();

            if tx.send(samples).is_err() {
                break;
            }
        }

        let _ = child.kill();
    });

    rx
}

fn read_exact(reader: &mut impl Read, buf: &mut [u8], stop: &Arc<AtomicBool>) -> std::io::Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "stopped"));
        }
        match reader.read(&mut buf[pos..]) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")),
            Ok(n) => pos += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
