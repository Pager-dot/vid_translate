use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

use super::SAMPLE_RATE;

// 250ms chunks — Vosk processes these fast enough for word-by-word output
const CHUNK_SAMPLES: usize = (SAMPLE_RATE / 4) as usize;
const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2; // S16LE = 2 bytes/sample

fn default_monitor_source() -> String {
    Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| format!("{}.monitor", s.trim()))
        .unwrap_or_else(|| "@DEFAULT_MONITOR@".into())
}

/// Start capturing system loopback audio via `parec`.
/// Returns a Receiver yielding 250ms 16kHz mono i16 chunks.
pub fn start_capture(stop: Arc<AtomicBool>) -> mpsc::Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let device = default_monitor_source();
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
