pub const SAMPLE_RATE: u32 = 16000;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod tap;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod linux;

#[cfg(target_os = "macos")]
pub use macos::{capture_fault, preflight, set_prefer_microphone, start_capture};
#[cfg(target_os = "windows")]
pub use windows::start_capture;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub use linux::start_capture;

/// Why a capture session cannot produce audio. macOS-only in practice, but shared so
/// `lib.rs` can map faults to frontend statuses without platform branches.
///
/// The discriminants are stable because macOS stores the current fault in an atomic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureFault {
    /// The OS is too old for the tap API, or the tap could not be created at all.
    TapUnavailable = 1,
    /// The tap was created but CoreAudio refused to run it.
    TapFailed = 2,
    /// Buffers are arriving but every sample is bit-exact zero. On macOS this means TCC
    /// denied "System Audio Recording" — the tap API reports success either way, so
    /// silence is the only available signal.
    PermissionDenied = 3,
    /// The capture device delivered no buffers at all.
    NoAudioFrames = 4,
}

impl CaptureFault {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(CaptureFault::TapUnavailable),
            2 => Some(CaptureFault::TapFailed),
            3 => Some(CaptureFault::PermissionDenied),
            4 => Some(CaptureFault::NoAudioFrames),
            _ => None,
        }
    }

    /// The `status` event the frontend turns into a setup screen.
    pub fn status(self) -> &'static str {
        match self {
            CaptureFault::TapUnavailable | CaptureFault::TapFailed => "audio_tap_unavailable",
            CaptureFault::PermissionDenied | CaptureFault::NoAudioFrames => {
                "audio_permission_denied"
            }
        }
    }
}

/// Linux and Windows tap the system output mix directly (a PulseAudio sink monitor / WASAPI
/// loopback), with no permission gate and no device for the user to choose, so these two are
/// no-ops there. Only macOS needs them — see `macos.rs` and `tap.rs`.
#[cfg(not(target_os = "macos"))]
pub fn preflight() -> Result<(), CaptureFault> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn capture_fault() -> Option<CaptureFault> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn set_prefer_microphone(_prefer: bool) {}
