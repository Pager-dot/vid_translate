//! Core Audio process tap: system-audio capture with no driver and no routing changes.
//!
//! macOS 14.4+ can hand an app the system output mix directly, which replaces the virtual
//! loopback driver (BlackHole) this app used to require. Three things about it are not
//! discoverable from Apple's headers and were established by experiment — change them at
//! your peril:
//!
//! 1. **A tap must be wrapped in an aggregate device that has a real output device as its
//!    main sub-device.** The tap alone is not readable. The aggregate here is created
//!    *private*, so it never appears in Audio MIDI Setup and is never the system output —
//!    which is what keeps the user's device choice and the keyboard volume keys working.
//!    A private aggregate is also invisible to device enumeration, which is why `cpal`
//!    cannot be used on this path: its API starts from enumeration and would never return
//!    this device. We drive it by `AudioObjectID` instead.
//!
//! 2. **`AudioHardwareCreateProcessTap` returns `noErr` even when TCC denied permission.**
//!    There is no error to check. A denied tap either delivers buffers of bit-exact zeros
//!    forever, or stops delivering buffers at all. `Ring` therefore tracks both the frame
//!    count and the OR of every sample's raw bits so the caller can tell "permission
//!    denied" from "nothing is playing" — see `macos.rs`.
//!
//! 3. **The thread that creates the tap must service a run loop.** TCC cannot present its
//!    authorization dialog to a process that never pumps one and silently denies instead.
//!    A `thread::sleep` loop reproduces the denial every time; `CFRunLoopRunInMode`
//!    reproduces a working capture every time. The supervisor loop in `macos.rs` therefore
//!    pumps the run loop rather than sleeping, which also delivers the default-output
//!    property-listener callbacks.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys::*;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSString};
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

// bindgen skips these two (availability-gated in the SDK headers), so declare them.
extern "C" {
    fn AudioHardwareCreateProcessTap(
        in_description: *mut AnyObject,
        out_tap_id: *mut AudioObjectID,
    ) -> OSStatus;
    fn AudioHardwareDestroyProcessTap(in_tap_id: AudioObjectID) -> OSStatus;
}

/// Power of two so the cursors can be masked rather than divided. 262144 samples is ~2.7s
/// of 48 kHz stereo — far more than the supervisor's poll interval needs, so overflow only
/// happens if the supervisor thread is wedged, in which case dropping is correct.
const RING_CAPACITY: usize = 1 << 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapError {
    /// Below macOS 14.4 — the tap API does not exist.
    UnsupportedOs,
    /// `CATapDescription` or the tap itself could not be created.
    TapCreate(OSStatus),
    /// The aggregate wrapper could not be created, or has no usable output device.
    AggregateCreate(OSStatus),
    /// The IOProc could not be registered or started.
    IoProc(OSStatus),
    /// A CoreAudio property query failed.
    Property(OSStatus),
}

impl std::fmt::Display for TapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TapError::UnsupportedOs => write!(f, "system audio capture needs macOS 14.4 or later"),
            TapError::TapCreate(s) => write!(f, "could not create the audio tap (status {s})"),
            TapError::AggregateCreate(s) => {
                write!(f, "could not create the capture device (status {s})")
            }
            TapError::IoProc(s) => write!(f, "could not start the capture device (status {s})"),
            TapError::Property(s) => write!(f, "CoreAudio property query failed (status {s})"),
        }
    }
}

/// Lock-free SPSC ring shared between the realtime IOProc (producer) and the supervisor
/// thread (consumer).
///
/// The IOProc runs on a CoreAudio realtime thread, where allocating, locking, logging or
/// panicking is forbidden — `write` does none of those. `Vec`-based buffering or
/// `mpsc::Sender::send` (which allocates per send) would all be violations, which is why
/// this exists rather than sending straight to the channel.
pub struct Ring {
    buf: UnsafeCell<Box<[f32]>>,
    write: AtomicUsize,
    read: AtomicUsize,
    /// Samples the producer had to discard because the consumer fell behind.
    dropped: AtomicU64,
    /// Total samples delivered, ever. Distinguishes "no buffers at all" (dead aggregate)
    /// from "buffers of silence" (TCC denial).
    frames: AtomicU64,
    /// OR of every sample's raw bits. Stays exactly 0 iff every sample was +0.0.
    bits: AtomicU32,
}

// Safety: exactly one producer (the IOProc) and one consumer (the supervisor thread), each
// touching only its own cursor, with Release/Acquire pairing the data against it.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    pub fn new() -> Self {
        Self {
            buf: UnsafeCell::new(vec![0.0; RING_CAPACITY].into_boxed_slice()),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            bits: AtomicU32::new(0),
        }
    }

    /// Producer side. Realtime-safe: no allocation, no locks, no panics, no branches that
    /// can trap. Overflowing discards the incoming samples rather than blocking.
    fn write(&self, samples: &[f32]) {
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let used = w.wrapping_sub(r);
        let free = RING_CAPACITY - used;
        if samples.len() > free {
            self.dropped.fetch_add(samples.len() as u64, Ordering::Relaxed);
            return;
        }
        let buf = unsafe { &mut *self.buf.get() };
        let mut bits = 0u32;
        for (i, &s) in samples.iter().enumerate() {
            bits |= s.to_bits();
            buf[w.wrapping_add(i) & (RING_CAPACITY - 1)] = s;
        }
        self.bits.fetch_or(bits, Ordering::Relaxed);
        self.frames.fetch_add(samples.len() as u64, Ordering::Relaxed);
        self.write.store(w.wrapping_add(samples.len()), Ordering::Release);
    }

    /// Consumer side. Copies up to `out.len()` samples out and returns how many.
    pub fn read_into(&self, out: &mut [f32]) -> usize {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r).min(out.len());
        if avail == 0 {
            return 0;
        }
        let buf = unsafe { &*self.buf.get() };
        for i in 0..avail {
            out[i] = buf[r.wrapping_add(i) & (RING_CAPACITY - 1)];
        }
        self.read.store(r.wrapping_add(avail), Ordering::Release);
        avail
    }

    /// Total samples delivered since the tap started.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// False only if every sample delivered so far was bit-exact +0.0.
    pub fn saw_audio(&self) -> bool {
        self.bits.load(Ordering::Relaxed) != 0
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Discards buffered samples. Used after an aggregate rebuild so frames captured
    /// against the old output device are not spliced into the new stream.
    fn clear(&self) {
        self.read
            .store(self.write.load(Ordering::Acquire), Ordering::Release);
    }
}

/// `true` on macOS 14.4 or later. Deliberately a runtime check, not `#[cfg]`: this is an
/// OS-version question, not a target question, and `tauri dev` binaries are not gated by
/// the bundle's `LSMinimumSystemVersion` the way an installed `.app` is.
pub fn tap_supported() -> bool {
    os_version().map_or(false, |(major, minor)| (major, minor) >= (14, 4))
}

fn os_version() -> Option<(u32, u32)> {
    let mut size: usize = 0;
    let name = c"kern.osproductversion";
    unsafe {
        if libc_sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut size) != 0 || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc_sysctlbyname(name.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut size) != 0 {
            return None;
        }
        let s = String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).to_string();
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
        Some((major, minor))
    }
}

unsafe fn libc_sysctlbyname(
    name: *const std::os::raw::c_char,
    out: *mut c_void,
    size: *mut usize,
) -> i32 {
    extern "C" {
        fn sysctlbyname(
            name: *const std::os::raw::c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> i32;
    }
    sysctlbyname(name, out, size, std::ptr::null_mut(), 0)
}

/// The aggregate-description keys are C string literals in the bindings; CoreAudio wants
/// them as CFStrings.
fn cfkey(key: &[u8]) -> CFString {
    CFString::new(std::str::from_utf8(&key[..key.len() - 1]).unwrap_or_default())
}

unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let ring = &*(client as *const Ring);
    let list = &*input;
    let buffers = std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize);
    for buf in buffers {
        if buf.mData.is_null() {
            continue;
        }
        let n = buf.mDataByteSize as usize / std::mem::size_of::<f32>();
        ring.write(std::slice::from_raw_parts(buf.mData as *const f32, n));
    }
    0
}

/// Fired by CoreAudio when the default output device changes. Does the absolute minimum:
/// setting a flag. Calling back into CoreAudio from inside a property listener — to query
/// the new device, let alone build an aggregate — is a documented deadlock source.
unsafe extern "C" fn output_changed_listener(
    _object: AudioObjectID,
    _n: u32,
    _addrs: *const AudioObjectPropertyAddress,
    client: *mut c_void,
) -> OSStatus {
    if !client.is_null() {
        (*(client as *const AtomicBool)).store(true, Ordering::Release);
    }
    0
}

const DEFAULT_OUTPUT_ADDRESS: AudioObjectPropertyAddress = AudioObjectPropertyAddress {
    mSelector: kAudioHardwarePropertyDefaultOutputDevice,
    mScope: kAudioObjectPropertyScopeGlobal,
    mElement: kAudioObjectPropertyElementMain,
};

unsafe fn default_output() -> Result<(AudioObjectID, String), TapError> {
    let mut id: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let st = AudioObjectGetPropertyData(
        kAudioObjectSystemObject,
        &DEFAULT_OUTPUT_ADDRESS,
        0,
        std::ptr::null(),
        &mut size,
        &mut id as *mut _ as *mut c_void,
    );
    if st != 0 || id == 0 {
        return Err(TapError::Property(st));
    }

    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut uid_ref: CFStringRef = std::ptr::null();
    let mut uid_size = std::mem::size_of::<CFStringRef>() as u32;
    let st = AudioObjectGetPropertyData(
        id,
        &addr,
        0,
        std::ptr::null(),
        &mut uid_size,
        &mut uid_ref as *mut _ as *mut c_void,
    );
    if st != 0 || uid_ref.is_null() {
        return Err(TapError::Property(st));
    }
    Ok((id, CFString::wrap_under_create_rule(uid_ref as _).to_string()))
}

unsafe fn tap_stream_format(tap: AudioObjectID) -> Result<(u32, u16), TapError> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioTapPropertyFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut asbd: AudioStreamBasicDescription = std::mem::zeroed();
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let st = AudioObjectGetPropertyData(
        tap,
        &addr,
        0,
        std::ptr::null(),
        &mut size,
        &mut asbd as *mut _ as *mut c_void,
    );
    if st != 0 {
        return Err(TapError::Property(st));
    }
    // Never assume 48 kHz stereo: Bluetooth output runs the tap at 44.1 kHz, and some
    // multichannel setups report more than 2 channels despite a "stereo" tap description.
    Ok((
        asbd.mSampleRate as u32,
        asbd.mChannelsPerFrame.max(1) as u16,
    ))
}

/// A live system-audio tap. Owns the tap, its private aggregate wrapper, the IOProc and the
/// default-output listener, and releases all of them in `Drop`.
pub struct SystemTap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    /// Kept alive because CoreAudio holds the raw pointer for the IOProc's whole life.
    ring: Arc<Ring>,
    rebuild_pending: Arc<AtomicBool>,
    format: (u32, u16),
    listener_installed: bool,
    /// The tap description's UUID string, which the aggregate's tap list references. Kept
    /// so a rebuild can re-reference the same tap.
    tap_uid: String,
    /// The description object must outlive the tap it created.
    _description: Retained<AnyObject>,
}

impl SystemTap {
    pub fn new(ring: Arc<Ring>) -> Result<Self, TapError> {
        if !tap_supported() {
            return Err(TapError::UnsupportedOs);
        }
        unsafe {
            let (description, tap_uid) = Self::make_description()?;
            let mut tap: AudioObjectID = 0;
            let st = AudioHardwareCreateProcessTap(
                Retained::as_ptr(&description) as *mut AnyObject,
                &mut tap,
            );
            // NB: st == 0 proves nothing about permission. See the module comment.
            if st != 0 || tap == 0 {
                return Err(TapError::TapCreate(st));
            }

            let format = match tap_stream_format(tap) {
                Ok(f) => f,
                Err(e) => {
                    AudioHardwareDestroyProcessTap(tap);
                    return Err(e);
                }
            };

            let rebuild_pending = Arc::new(AtomicBool::new(false));
            let mut this = SystemTap {
                tap,
                aggregate: 0,
                proc_id: None,
                ring,
                rebuild_pending,
                format,
                listener_installed: false,
                tap_uid: tap_uid.clone(),
                _description: description,
            };

            // Built after `this` exists so Drop cleans up whatever succeeded if this fails —
            // a leaked aggregate leaves a phantom device behind until reboot.
            this.build_aggregate(&tap_uid)?;

            let st = AudioObjectAddPropertyListener(
                kAudioObjectSystemObject,
                &DEFAULT_OUTPUT_ADDRESS,
                Some(output_changed_listener),
                Arc::as_ptr(&this.rebuild_pending) as *mut c_void,
            );
            // A failed listener only costs the Bluetooth-switch recovery, so carry on.
            this.listener_installed = st == 0;
            if st != 0 {
                eprintln!("[audio] default-output listener not installed (status {st})");
            }

            Ok(this)
        }
    }

    unsafe fn make_description() -> Result<(Retained<AnyObject>, String), TapError> {
        let cls = AnyClass::get(c"CATapDescription").ok_or(TapError::UnsupportedOs)?;
        // Empty exclusion list: this app plays no audio of its own, so there is nothing to
        // exclude and no feedback loop to create. If it ever gains sounds, its own audio
        // object id belongs in here.
        let exclude: Retained<NSArray> = NSArray::new();
        let alloc: *mut AnyObject = msg_send![cls, alloc];
        let raw: *mut AnyObject =
            msg_send![alloc, initStereoGlobalTapButExcludeProcesses: &*exclude];
        if raw.is_null() {
            return Err(TapError::TapCreate(-1));
        }
        let description = Retained::from_raw(raw).ok_or(TapError::TapCreate(-1))?;

        // CATapUnmuted (0): the user keeps hearing the audio being captured, and — because
        // the tap reads the mix before the output device applies volume — capture also keeps
        // working when the output is muted or at zero. That is what makes this usable for
        // someone who cannot hear the audio at all.
        let _: () = msg_send![&*description, setMuteBehavior: 0isize];
        let _: () = msg_send![&*description, setPrivate: true];
        let name = NSString::from_str("VidTranslate");
        let _: () = msg_send![&*description, setName: &*name];

        let uuid: *mut AnyObject = msg_send![&*description, UUID];
        if uuid.is_null() {
            return Err(TapError::TapCreate(-1));
        }
        let uuid_string: *mut NSString = msg_send![uuid, UUIDString];
        if uuid_string.is_null() {
            return Err(TapError::TapCreate(-1));
        }
        Ok((description, (*uuid_string).to_string()))
    }

    /// Creates the private aggregate around the tap and starts its IOProc.
    unsafe fn build_aggregate(&mut self, tap_uid: &str) -> Result<(), TapError> {
        let (_, output_uid) = default_output()?;

        let sub_device = CFDictionary::from_CFType_pairs(&[(
            cfkey(kAudioSubDeviceUIDKey).as_CFType(),
            CFString::new(&output_uid).as_CFType(),
        )]);
        let tap_entry = CFDictionary::from_CFType_pairs(&[
            (
                cfkey(kAudioSubTapUIDKey).as_CFType(),
                CFString::new(tap_uid).as_CFType(),
            ),
            (
                cfkey(kAudioSubTapDriftCompensationKey).as_CFType(),
                CFNumber::from(1i32).as_CFType(),
            ),
        ]);

        // A fresh UID per aggregate, never a fixed constant: a stale aggregate left by a
        // crashed run would otherwise collide with this one.
        let aggregate_uid = format!(
            "com.paritosh.vidtranslate.tap.{}.{}",
            std::process::id(),
            self.tap
        );
        let pairs: Vec<(CFType, CFType)> = vec![
            (
                cfkey(kAudioAggregateDeviceNameKey).as_CFType(),
                CFString::new("VidTranslate Capture").as_CFType(),
            ),
            (
                cfkey(kAudioAggregateDeviceUIDKey).as_CFType(),
                CFString::new(&aggregate_uid).as_CFType(),
            ),
            // Private: invisible in Audio MIDI Setup, never selectable as the system output.
            (
                cfkey(kAudioAggregateDeviceIsPrivateKey).as_CFType(),
                CFNumber::from(1i32).as_CFType(),
            ),
            (
                cfkey(kAudioAggregateDeviceIsStackedKey).as_CFType(),
                CFNumber::from(0i32).as_CFType(),
            ),
            // The real output device clocks the aggregate. Required: a tap on its own is
            // not readable.
            (
                cfkey(kAudioAggregateDeviceMainSubDeviceKey).as_CFType(),
                CFString::new(&output_uid).as_CFType(),
            ),
            (
                cfkey(kAudioAggregateDeviceTapAutoStartKey).as_CFType(),
                CFNumber::from(1i32).as_CFType(),
            ),
            (
                cfkey(kAudioAggregateDeviceSubDeviceListKey).as_CFType(),
                CFArray::from_CFTypes(&[sub_device.as_CFType()]).as_CFType(),
            ),
            (
                cfkey(kAudioAggregateDeviceTapListKey).as_CFType(),
                CFArray::from_CFTypes(&[tap_entry.as_CFType()]).as_CFType(),
            ),
        ];
        let description = CFDictionary::from_CFType_pairs(&pairs);

        let mut aggregate: AudioObjectID = 0;
        let st =
            AudioHardwareCreateAggregateDevice(description.as_concrete_TypeRef() as _, &mut aggregate);
        if st != 0 || aggregate == 0 {
            return Err(TapError::AggregateCreate(st));
        }
        self.aggregate = aggregate;

        let mut proc_id: AudioDeviceIOProcID = None;
        let st = AudioDeviceCreateIOProcID(
            aggregate,
            Some(io_proc),
            Arc::as_ptr(&self.ring) as *mut c_void,
            &mut proc_id,
        );
        if st != 0 {
            return Err(TapError::IoProc(st));
        }
        self.proc_id = proc_id;

        let st = AudioDeviceStart(aggregate, proc_id);
        if st != 0 {
            return Err(TapError::IoProc(st));
        }
        Ok(())
    }

    /// The tap's stream format, for building the `Resampler`.
    pub fn format(&self) -> (u32, u16) {
        self.format
    }

    /// True once the default output device has changed (headphones plugged in, Bluetooth
    /// connected). Clears the flag.
    pub fn output_changed(&self) -> bool {
        self.rebuild_pending.swap(false, Ordering::AcqRel)
    }

    /// Re-points the aggregate at the new default output device.
    ///
    /// Only the aggregate is rebuilt — the tap itself survives, because a global tap is not
    /// bound to an output device and keeping it alive avoids a second TCC evaluation. The
    /// capture thread above never stops, so the pipeline sees a short gap rather than a
    /// dropped session.
    pub fn rebuild_aggregate(&mut self) -> Result<(), TapError> {
        unsafe {
            self.teardown_aggregate();
            self.ring.clear();
            let tap_uid = self.tap_uid.clone();
            self.build_aggregate(&tap_uid)?;
            self.format = tap_stream_format(self.tap)?;
        }
        Ok(())
    }

    unsafe fn teardown_aggregate(&mut self) {
        if self.aggregate != 0 {
            if let Some(proc_id) = self.proc_id {
                AudioDeviceStop(self.aggregate, Some(proc_id));
                AudioDeviceDestroyIOProcID(self.aggregate, Some(proc_id));
            }
        }
        self.proc_id = None;
        if self.aggregate != 0 {
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            self.aggregate = 0;
        }
    }
}

impl Drop for SystemTap {
    fn drop(&mut self) {
        unsafe {
            if self.listener_installed {
                AudioObjectRemovePropertyListener(
                    kAudioObjectSystemObject,
                    &DEFAULT_OUTPUT_ADDRESS,
                    Some(output_changed_listener),
                    Arc::as_ptr(&self.rebuild_pending) as *mut c_void,
                );
            }
            self.teardown_aggregate();
            if self.tap != 0 {
                AudioHardwareDestroyProcessTap(self.tap);
                self.tap = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ring is the hand-off between a realtime thread and a normal one, and cursor
    // wrap-around is its only real failure mode — so write far more than capacity in
    // IOProc-sized bursts while draining in differently-sized reads.
    #[test]
    fn ring_round_trips_samples_unchanged_across_wraparound() {
        let ring = Ring::new();
        let mut expected: Vec<f32> = Vec::new();
        let mut got: Vec<f32> = Vec::new();
        let mut out = vec![0.0f32; 777];
        let mut next = 0.0f32;
        for _ in 0..600 {
            let burst: Vec<f32> = (0..1024)
                .map(|_| {
                    next += 1.0;
                    next
                })
                .collect();
            ring.write(&burst);
            expected.extend_from_slice(&burst);
            loop {
                let n = ring.read_into(&mut out);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&out[..n]);
            }
        }
        assert_eq!(got.len(), expected.len());
        assert_eq!(got, expected);
    }

    // The realtime rule is "never block the IOProc", so a full ring must drop and move on.
    #[test]
    fn a_full_ring_drops_instead_of_blocking() {
        let ring = Ring::new();
        let burst = vec![0.5f32; RING_CAPACITY / 2];
        ring.write(&burst);
        ring.write(&burst);
        assert_eq!(ring.dropped(), 0, "exactly full must still fit");
        ring.write(&burst);
        assert_eq!(ring.dropped(), burst.len() as u64);

        // The reader still gets a valid, contiguous prefix rather than torn data.
        let mut out = vec![0.0f32; RING_CAPACITY];
        let n = ring.read_into(&mut out);
        assert_eq!(n, RING_CAPACITY);
        assert!(out.iter().all(|&s| s == 0.5));
    }

    // How a TCC denial is detected: buffers arrive, every sample is bit-exact zero. One
    // non-zero sample has to be enough to clear the verdict, or a quiet passage would be
    // reported as a permission problem.
    #[test]
    fn silence_is_distinguishable_from_audio_and_from_no_frames() {
        let ring = Ring::new();
        assert_eq!(ring.frames(), 0);
        assert!(!ring.saw_audio());

        ring.write(&[0.0; 512]);
        assert_eq!(ring.frames(), 512, "frames flow even when denied");
        assert!(!ring.saw_audio(), "pure zeros must not read as audio");

        ring.write(&[0.0, 0.0, 1.0e-7, 0.0]);
        assert!(ring.saw_audio(), "any non-zero sample clears the verdict");
    }

    // After re-pointing the aggregate at a new output device, frames captured against the
    // old one must not be spliced into the new stream.
    #[test]
    fn clearing_the_ring_discards_buffered_samples() {
        let ring = Ring::new();
        ring.write(&[0.25f32; 4096]);
        ring.clear();
        let mut out = vec![0.0f32; 4096];
        assert_eq!(ring.read_into(&mut out), 0);

        // ...and the ring keeps working afterwards.
        ring.write(&[0.75f32; 16]);
        assert_eq!(ring.read_into(&mut out), 16);
        assert!(out[..16].iter().all(|&s| s == 0.75));
    }

    // This machine is the reference: the spike that validated the whole approach ran on
    // macOS 27, and the floor is 14.4.
    #[test]
    fn os_version_parses_and_gates_at_14_4() {
        let v = os_version().expect("kern.osproductversion must be readable on macOS");
        assert!(v.0 >= 11, "implausible major version: {v:?}");
        assert_eq!(tap_supported(), v >= (14, 4));
    }
}
