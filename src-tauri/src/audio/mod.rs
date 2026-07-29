pub const SAMPLE_RATE: u32 = 16000;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod linux;

#[cfg(target_os = "macos")]
pub use macos::{loopback_device_name, set_prefer_microphone, start_capture};
#[cfg(target_os = "windows")]
pub use windows::start_capture;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub use linux::start_capture;

/// Linux and Windows tap the system output mix directly (a PulseAudio sink monitor / WASAPI
/// loopback), so there is never a device for the user to install or choose and these two are
/// no-ops. Only macOS, which has no public system-audio API, needs them — see `macos.rs`.
#[cfg(not(target_os = "macos"))]
pub fn loopback_device_name() -> Option<String> {
    Some("system audio".to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn set_prefer_microphone(_prefer: bool) {}
