use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use super::SAMPLE_RATE;

// 250ms chunks — matches the Linux backend's contract so recognizer.rs/vad callers
// don't need to care which platform captured the audio.
const CHUNK_SAMPLES: usize = (SAMPLE_RATE / 4) as usize;
const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2; // S16 mono = 2 bytes/sample

/// Start capturing system loopback audio via WASAPI.
/// Returns a Receiver yielding 250ms 16kHz mono i16 chunks — same contract as the Linux backend.
pub fn start_capture(stop: Arc<AtomicBool>) -> mpsc::Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        if let Err(e) = capture_loop(tx, stop) {
            eprintln!("[audio] WASAPI capture error: {}", e);
        }
    });

    rx
}

fn capture_loop(
    tx: mpsc::Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    initialize_mta().ok()?;

    let enumerator = DeviceEnumerator::new()?;
    // Opening the default *render* (output) device for *capture* puts WASAPI into loopback
    // mode — this is the Windows equivalent of parec's "<sink>.monitor" source on Linux.
    let device = enumerator.get_default_device(&Direction::Render)?;
    let mut audio_client = device.get_iaudioclient()?;

    // Request 16kHz mono S16 directly. `autoconvert: true` sets AUTOCONVERTPCM +
    // SRC_DEFAULT_QUALITY, so the shared-mode audio engine resamples/downmixes from the
    // device's native mix format for us — no manual resampling needed here.
    let desired_format = WaveFormat::new(16, 16, &SampleType::Int, SAMPLE_RATE as usize, 1, None);

    let (_, min_time) = audio_client.get_device_period()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: min_time,
    };
    audio_client.initialize_client(&desired_format, &Direction::Capture, &mode)?;

    let h_event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(CHUNK_BYTES * 4);

    audio_client.start_stream()?;

    while !stop.load(Ordering::Relaxed) {
        while byte_queue.len() >= CHUNK_BYTES {
            let bytes: Vec<u8> = byte_queue.drain(..CHUNK_BYTES).collect();
            let samples: Vec<i16> = bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            if tx.send(samples).is_err() {
                audio_client.stop_stream()?;
                return Ok(());
            }
        }
        capture_client.read_from_device_to_deque(&mut byte_queue)?;
        // Timing out just means no new audio arrived yet (e.g. silence) — loop back
        // around to re-check the stop flag rather than blocking forever.
        let _ = h_event.wait_for_event(1000);
    }

    audio_client.stop_stream()?;
    Ok(())
}
