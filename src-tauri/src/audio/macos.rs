//! macOS system-audio capture.
//!
//! Unlike Linux (`parec` on a sink monitor) and Windows (WASAPI loopback), macOS has no
//! public API that lets an ordinary app tap the system output mix. The supported route is a
//! virtual loopback *driver* — BlackHole, Loopback, VB-Cable, etc. — which installs a device
//! whose input side carries whatever is played into its output side. So here we enumerate
//! CoreAudio input devices (via cpal) and pick the loopback driver if one is installed.
//!
//! Since the picked device reports its own native rate/format (typically 48 kHz stereo f32),
//! this backend also downmixes to mono and resamples to 16 kHz i16 before emitting the same
//! 250ms chunks the Linux/Windows backends do — Vosk only accepts that exact shape.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
// `Sample` carries the `from_sample` conversion method, `FromSample` is the bound it needs,
// and `SizedSample` is what `build_input_stream` is generic over.
use cpal::{FromSample, Sample, SizedSample};

use super::SAMPLE_RATE;

// 250ms chunks — matches the Linux/Windows backends' contract so recognizer.rs doesn't
// need to care which platform captured the audio.
const CHUNK_SAMPLES: usize = (SAMPLE_RATE / 4) as usize;

/// Set from the frontend (via the `prefer_microphone` argument to `start_listening`) when the
/// user has been shown the "no loopback driver installed" setup screen and chose to caption
/// their microphone instead. Process-global rather than threaded through every pipeline
/// signature because it is a single macOS-only user preference, not per-session state.
static PREFER_MICROPHONE: AtomicBool = AtomicBool::new(false);

pub fn set_prefer_microphone(prefer: bool) {
    PREFER_MICROPHONE.store(prefer, Ordering::Relaxed);
}

/// Name fragments identifying a virtual loopback driver, most-specific first. Ordering is the
/// match priority: a machine can easily have several of these installed at once (BlackHole
/// plus an Aggregate Device wrapping it, say), and the concrete driver is the better pick —
/// "aggregate"/"virtual" are last because they also match plain multi-mic setups that carry
/// no system audio at all.
const LOOPBACK_HINTS: &[&str] = &[
    "blackhole",
    "soundflower",
    "vb-cable",
    "vb-audio",
    "ishowu",
    "loopback audio",
    "loopback",
    "existential audio",
    "audio hijack",
    "screenflick",
    "virtual",
    "aggregate",
];

fn hint_rank(device_name: &str) -> Option<usize> {
    let lower = device_name.to_lowercase();
    LOOPBACK_HINTS.iter().position(|hint| lower.contains(hint))
}

fn find_loopback_device() -> Option<(cpal::Device, String)> {
    let host = cpal::default_host();
    let devices = host.input_devices().ok()?;

    let mut best: Option<(usize, cpal::Device, String)> = None;
    for device in devices {
        let Ok(name) = device.name() else { continue };
        // A device that can't report an input config isn't usable for capture no matter what
        // it's called (this filters out output-only entries some drivers still expose).
        if device.default_input_config().is_err() {
            continue;
        }
        if let Some(rank) = hint_rank(&name) {
            let is_better = match &best {
                Some((best_rank, _, _)) => rank < *best_rank,
                None => true,
            };
            if is_better {
                best = Some((rank, device, name));
            }
        }
    }

    best.map(|(_, device, name)| (device, name))
}

/// The loopback device this backend would capture from, or `None` if no virtual loopback
/// driver is installed. `lib.rs` calls this before starting a session so the user gets a
/// setup screen with install instructions instead of a silently silent transcript.
///
/// Returns the default input device once the user has explicitly opted into microphone
/// capture, since in that mode a missing loopback driver is no longer a problem.
pub fn loopback_device_name() -> Option<String> {
    if PREFER_MICROPHONE.load(Ordering::Relaxed) {
        return cpal::default_host()
            .default_input_device()
            .and_then(|d| d.name().ok());
    }
    find_loopback_device().map(|(_, name)| name)
}

/// Downmixes interleaved input frames to mono and resamples them to 16 kHz, carrying its
/// fractional read position across CoreAudio callbacks so chunk boundaries don't click.
struct Resampler {
    /// Input samples consumed per output sample (`input_rate / 16000`).
    step: f64,
    /// Read position into `mono`, in input samples. Fractional between callbacks.
    pos: f64,
    channels: usize,
    mono: Vec<f32>,
    /// Output samples accumulated toward the next 250ms chunk.
    pending: Vec<i16>,
}

impl Resampler {
    fn new(input_rate: u32, channels: u16) -> Self {
        Self {
            step: input_rate as f64 / SAMPLE_RATE as f64,
            pos: 0.0,
            channels: channels.max(1) as usize,
            mono: Vec::new(),
            pending: Vec::with_capacity(CHUNK_SAMPLES),
        }
    }

    /// Feeds one CoreAudio buffer, calling `emit` once per completed 250ms 16 kHz chunk.
    fn push(&mut self, interleaved: &[f32], mut emit: impl FnMut(Vec<i16>)) {
        for frame in interleaved.chunks_exact(self.channels) {
            let sum: f32 = frame.iter().sum();
            self.mono.push(sum / self.channels as f32);
        }

        // Linear interpolation needs the sample *after* `pos`, so stop one short of the end
        // and leave the remainder in `mono` for the next callback to interpolate against.
        while self.pos + 1.0 < self.mono.len() as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let sample = self.mono[i] * (1.0 - frac) + self.mono[i + 1] * frac;
            self.pending
                .push((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            self.pos += self.step;

            if self.pending.len() == CHUNK_SAMPLES {
                emit(std::mem::replace(
                    &mut self.pending,
                    Vec::with_capacity(CHUNK_SAMPLES),
                ));
            }
        }

        // The last step can carry `pos` past the end of the buffer (48kHz→16kHz advances 3
        // input samples per output, and buffer lengths are not multiples of 3), so clamp
        // before draining. The leftover fraction stays in `pos` and correctly skips that far
        // into whatever the next callback delivers.
        let consumed = (self.pos as usize).min(self.mono.len());
        if consumed > 0 {
            self.mono.drain(..consumed);
            self.pos -= consumed as f64;
        }
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut resampler: Resampler,
    tx: mpsc::Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            scratch.clear();
            scratch.extend(data.iter().map(|&s| f32::from_sample(s)));
            resampler.push(&scratch, |chunk| {
                // A closed receiver means the pipeline is shutting down; the capture thread
                // notices the stop flag on its next tick and drops the stream.
                let _ = tx.send(chunk);
            });
        },
        |err| eprintln!("[audio] CoreAudio stream error: {err}"),
        None,
    )
}

/// Start capturing loopback (or, if the user opted in, microphone) audio via CoreAudio.
/// Returns a Receiver yielding 250ms 16kHz mono i16 chunks — same contract as the other
/// platform backends.
pub fn start_capture(stop: Arc<AtomicBool>) -> mpsc::Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        if let Err(e) = capture_loop(tx, stop) {
            eprintln!("[audio] CoreAudio capture error: {e}");
        }
    });

    rx
}

fn capture_loop(
    tx: mpsc::Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let prefer_mic = PREFER_MICROPHONE.load(Ordering::Relaxed);

    let (device, name) = if prefer_mic {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or("no input device available")?;
        let name = device.name().unwrap_or_else(|_| "default input".into());
        (device, name)
    } else {
        find_loopback_device().ok_or(
            "no loopback audio device found — install BlackHole (https://existential.audio/blackhole) \
             and route system output through it",
        )?
    };

    let supported = device.default_input_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    eprintln!(
        "[audio] Recording from: {name} ({} Hz, {} ch, {sample_format:?}){}",
        config.sample_rate.0,
        config.channels,
        if prefer_mic { " [microphone]" } else { "" }
    );

    let resampler = Resampler::new(config.sample_rate.0, config.channels);
    // CoreAudio only ever hands back these formats in practice; the rest of cpal's sample
    // types are listed by the enum but unreachable here.
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, resampler, tx, stop.clone()),
        cpal::SampleFormat::F64 => build_stream::<f64>(&device, &config, resampler, tx, stop.clone()),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, resampler, tx, stop.clone()),
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, &config, resampler, tx, stop.clone()),
        cpal::SampleFormat::I8 => build_stream::<i8>(&device, &config, resampler, tx, stop.clone()),
        cpal::SampleFormat::U8 => build_stream::<u8>(&device, &config, resampler, tx, stop.clone()),
        other => return Err(format!("unsupported sample format: {other:?}").into()),
    }?;

    stream.play()?;

    // `cpal::Stream` is `!Send` on CoreAudio, so it has to be built, parked and dropped on
    // this one thread — the audio itself is delivered on CoreAudio's own realtime thread.
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    drop(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_concrete_drivers_over_generic_hints() {
        assert!(hint_rank("BlackHole 2ch").unwrap() < hint_rank("Aggregate Device").unwrap());
        assert!(hint_rank("VB-Cable").unwrap() < hint_rank("Some Virtual Thing").unwrap());
        assert_eq!(hint_rank("MacBook Pro Microphone"), None);
    }

    #[test]
    fn resamples_48k_stereo_to_16k_mono_chunks() {
        // 3 seconds of 48kHz stereo = 12 chunks of 250ms at 16kHz, and downmixing two
        // identical channels must not change the sample values.
        let mut r = Resampler::new(48_000, 2);
        let input: Vec<f32> = vec![0.5; 48_000 * 3 * 2];
        let mut chunks = Vec::new();
        // Fed in realistic-sized buffers so cross-callback position carry-over is exercised.
        for buf in input.chunks(2048) {
            r.push(buf, |c| chunks.push(c));
        }
        assert_eq!(chunks.len(), 12);
        assert!(chunks.iter().all(|c| c.len() == CHUNK_SAMPLES));
        let expected = (0.5 * i16::MAX as f32) as i16;
        assert!(chunks[5].iter().all(|&s| (s - expected).abs() <= 1));
    }

    #[test]
    fn survives_a_non_integer_rate_ratio() {
        // 44.1kHz advances a fractional 2.75625 input samples per output sample, so `pos`
        // regularly overruns the buffer it was reading from — the case the drain clamp in
        // `push` exists for. 10 seconds should yield ~40 chunks and must not panic.
        let mut r = Resampler::new(44_100, 1);
        let mut chunks = 0;
        for buf in vec![0.25f32; 44_100 * 10].chunks(1000) {
            r.push(buf, |c| {
                assert_eq!(c.len(), CHUNK_SAMPLES);
                chunks += 1;
            });
        }
        assert_eq!(chunks, 40);
    }
}
