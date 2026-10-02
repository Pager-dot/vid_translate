//! macOS system-audio capture, via a Core Audio process tap.
//!
//! Unlike Linux (`parec` on a sink monitor) and Windows (WASAPI loopback), macOS gave
//! ordinary apps no way to tap the system output mix until 14.4. The old route was a virtual
//! loopback *driver* (BlackHole and friends), which the user then had to wire into a
//! Multi-Output Device by hand — six manual steps that also broke the keyboard volume keys
//! and silently captured nothing if they switched to Bluetooth or muted their output.
//!
//! macOS 14.4+ replaces all of that with a process tap: no driver, no routing change, any
//! output device, and capture that keeps working while the output is muted. See `tap.rs` for
//! the CoreAudio mechanics and the three non-obvious rules that govern them.
//!
//! The tap reports its own native rate/format (48 kHz stereo f32 on built-in output,
//! 44.1 kHz over Bluetooth), so this backend still downmixes to mono and resamples to
//! 16 kHz i16 before emitting the same 250ms chunks the Linux/Windows backends do — Vosk
//! only accepts that exact shape. `Resampler` is shared by the tap and microphone paths.

use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
// `Sample` carries the `from_sample` conversion method, `FromSample` is the bound it needs,
// and `SizedSample` is what `build_input_stream` is generic over.
use cpal::{FromSample, Sample, SizedSample};

use super::{CaptureFault, SAMPLE_RATE};
use super::tap::{Ring, SystemTap, TapError};

// 250ms chunks — matches the Linux/Windows backends' contract so recognizer.rs doesn't
// need to care which platform captured the audio.
const CHUNK_SAMPLES: usize = (SAMPLE_RATE / 4) as usize;

/// Set from the frontend (via the `prefer_microphone` argument to `start_listening`) when the
/// user chose to caption their microphone instead of system audio — either from a setup
/// screen after a capture fault, or because that is the mode they want. Process-global rather
/// than threaded through every pipeline signature because it is a single macOS-only user
/// preference, not per-session state.
static PREFER_MICROPHONE: AtomicBool = AtomicBool::new(false);

pub fn set_prefer_microphone(prefer: bool) {
    PREFER_MICROPHONE.store(prefer, Ordering::Relaxed);
}

/// Last capture fault, as a `CaptureFault` discriminant (`0` = none). Polled by the
/// watchdog in `lib.rs` rather than pushed, because the capture thread has no `AppHandle`
/// and the faults are all terminal for the session anyway.
static CAPTURE_FAULT: AtomicU8 = AtomicU8::new(0);

fn set_fault(fault: CaptureFault) {
    CAPTURE_FAULT.store(fault as u8, Ordering::Relaxed);
}

pub fn capture_fault() -> Option<CaptureFault> {
    CaptureFault::from_u8(CAPTURE_FAULT.load(Ordering::Relaxed))
}

/// Checked before a session starts, so one that could only ever transcribe silence is
/// refused up front rather than appearing to run. Only the OS version is knowable this
/// cheaply — permission state is not, because the tap API reports success either way.
pub fn preflight() -> Result<(), CaptureFault> {
    CAPTURE_FAULT.store(0, Ordering::Relaxed);
    if PREFER_MICROPHONE.load(Ordering::Relaxed) || super::tap::tap_supported() {
        Ok(())
    } else {
        Err(CaptureFault::TapUnavailable)
    }
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

/// Start capturing system audio (or, if the user opted in, the microphone).
/// Returns a Receiver yielding 250ms 16kHz mono i16 chunks — same contract as the other
/// platform backends.
pub fn start_capture(stop: Arc<AtomicBool>) -> mpsc::Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let prefer_mic = PREFER_MICROPHONE.load(Ordering::Relaxed);
        let result = if prefer_mic {
            mic_capture_loop(tx, stop)
        } else {
            tap_capture_loop(tx, stop)
        };
        if let Err(e) = result {
            eprintln!("[audio] CoreAudio capture error: {e}");
        }
    });

    rx
}

/// How long a tap may deliver nothing but bit-exact silence before we conclude permission
/// was denied. A parameter rather than a literal so the decision is unit-testable.
const SILENCE_VERDICT_AFTER: Duration = Duration::from_secs(6);
/// How long a tap may deliver no buffers at all before we conclude the device is dead.
const NO_FRAMES_VERDICT_AFTER: Duration = Duration::from_secs(3);
/// Output-change events arrive several times as a Bluetooth device appears, is selected and
/// settles, so wait for quiet before rebuilding.
const OUTPUT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(300);

/// The supervisor side of the tap: drains the realtime ring, resamples, and keeps the
/// aggregate pointed at whatever the user's current output device is.
fn tap_capture_loop(
    tx: mpsc::Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ring = Arc::new(Ring::new());
    let mut tap = match SystemTap::new(ring.clone()) {
        Ok(t) => t,
        Err(e) => {
            set_fault(match e {
                TapError::UnsupportedOs => CaptureFault::TapUnavailable,
                _ => CaptureFault::TapFailed,
            });
            return Err(Box::new(std::io::Error::other(e.to_string())));
        }
    };

    let (mut rate, mut channels) = tap.format();
    eprintln!("[audio] Capturing system audio ({rate} Hz, {channels} ch, tap)");
    let mut resampler = Resampler::new(rate, channels);
    // Sized for a comfortable multiple of a CoreAudio buffer, allocated once: the drain
    // must not allocate per tick.
    let mut scratch = vec![0.0f32; 1 << 14];

    let started = Instant::now();
    let mut verdict_reached = false;
    let mut change_seen: Option<Instant> = None;
    let mut rebuild_failures = 0u32;

    while !stop.load(Ordering::Relaxed) {
        if tap.output_changed() {
            change_seen = Some(Instant::now());
        }
        if let Some(seen) = change_seen {
            if seen.elapsed() >= OUTPUT_CHANGE_DEBOUNCE {
                change_seen = None;
                match tap.rebuild_aggregate() {
                    Ok(()) => {
                        rebuild_failures = 0;
                        let (new_rate, new_channels) = tap.format();
                        if resampler_needs_rebuild((rate, channels), (new_rate, new_channels)) {
                            // A partial chunk straddling two sample rates would click, and
                            // 250ms is cheap to discard.
                            resampler = Resampler::new(new_rate, new_channels);
                            rate = new_rate;
                            channels = new_channels;
                        }
                        eprintln!(
                            "[audio] output device changed — capturing at {rate} Hz, {channels} ch"
                        );
                    }
                    Err(e) => {
                        rebuild_failures += 1;
                        eprintln!("[audio] could not follow the output device change: {e}");
                        // The user may have yanked their only output device; give the system
                        // a moment to settle on a new default rather than failing the session.
                        if rebuild_failures >= 20 {
                            set_fault(CaptureFault::TapFailed);
                            return Err(Box::new(std::io::Error::other(e.to_string())));
                        }
                        change_seen = Some(Instant::now());
                    }
                }
            }
        }

        let n = ring.read_into(&mut scratch);
        if n > 0 {
            resampler.push(&scratch[..n], |chunk| {
                let _ = tx.send(chunk);
            });
        } else {
            // Pump the run loop rather than sleeping. This is load-bearing: TCC cannot
            // present its authorization dialog to a process that never services a run
            // loop and silently denies instead, which manifests as a tap that returns
            // success and then delivers nothing but zeros forever. It also delivers the
            // default-output property-listener callbacks.
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.005, 0);
            }
        }

        if !verdict_reached {
            if let Some(fault) = silence_verdict(started.elapsed(), ring.frames(), ring.saw_audio())
            {
                set_fault(fault);
                verdict_reached = true;
            }
        }
    }

    if ring.dropped() > 0 {
        eprintln!("[audio] dropped {} samples (capture ran ahead)", ring.dropped());
    }
    Ok(())
}

/// Decides whether a tap that reports success is actually working.
///
/// Split out and pure so it can be tested without CoreAudio: the live signals are only a
/// clock, a frame count and "was any sample ever non-zero".
fn silence_verdict(elapsed: Duration, frames: u64, saw_audio: bool) -> Option<CaptureFault> {
    if saw_audio {
        return None;
    }
    if frames == 0 {
        return (elapsed >= NO_FRAMES_VERDICT_AFTER).then_some(CaptureFault::NoAudioFrames);
    }
    (elapsed >= SILENCE_VERDICT_AFTER).then_some(CaptureFault::PermissionDenied)
}

/// A new output device only forces a new `Resampler` if it changed the stream's shape.
fn resampler_needs_rebuild(old: (u32, u16), new: (u32, u16)) -> bool {
    old != new
}

/// The microphone fallback: still cpal, because enumeration is exactly what is wanted for a
/// real input device, and it is the escape hatch when the tap path is unavailable.
fn mic_capture_loop(
    tx: mpsc::Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no input device available")?;
    let name = device.name().unwrap_or_else(|_| "default input".into());

    let supported = device.default_input_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    eprintln!(
        "[audio] Recording from: {name} ({} Hz, {} ch, {sample_format:?}) [microphone]",
        config.sample_rate.0, config.channels,
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

    // A tap that reports success but delivers pure silence is how a denied permission
    // looks, and it must not be confused with "nothing is playing yet" — so the verdict
    // only lands after a window, and any audio at all clears it permanently.
    #[test]
    fn all_zero_buffers_trip_the_permission_fault_after_the_window() {
        let before = SILENCE_VERDICT_AFTER - Duration::from_millis(1);
        assert_eq!(silence_verdict(before, 96_000, false), None);
        assert_eq!(
            silence_verdict(SILENCE_VERDICT_AFTER, 96_000, false),
            Some(CaptureFault::PermissionDenied)
        );
    }

    #[test]
    fn any_audio_clears_the_permission_fault_however_long_the_silence_was() {
        assert_eq!(
            silence_verdict(SILENCE_VERDICT_AFTER * 10, 96_000, true),
            None
        );
    }

    // No buffers at all is a different failure from buffers of silence: the aggregate is
    // dead rather than muted, and it is diagnosable sooner.
    #[test]
    fn no_frames_at_all_is_reported_separately_and_sooner() {
        assert_eq!(
            silence_verdict(NO_FRAMES_VERDICT_AFTER, 0, false),
            Some(CaptureFault::NoAudioFrames)
        );
        assert!(NO_FRAMES_VERDICT_AFTER < SILENCE_VERDICT_AFTER);
    }

    // Switching output devices only justifies dropping the in-flight chunk when the stream
    // shape actually changed — e.g. 48 kHz built-in output to 44.1 kHz Bluetooth.
    #[test]
    fn a_device_switch_with_a_new_sample_rate_rebuilds_the_resampler() {
        assert!(resampler_needs_rebuild((48_000, 2), (44_100, 2)));
        assert!(resampler_needs_rebuild((48_000, 2), (48_000, 1)));
        assert!(!resampler_needs_rebuild((48_000, 2), (48_000, 2)));
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
