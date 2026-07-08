pub const SAMPLE_RATE: u32 = 16000;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(target_os = "windows"))]
mod linux;

#[cfg(target_os = "windows")]
pub use windows::start_capture;
#[cfg(not(target_os = "windows"))]
pub use linux::start_capture;
