//! Shared-memory activity contract between the driver and the daemon.
//!
//! Layout is fixed, `#[repr(C)]`, 64 bytes. All fields are lock-free atomics
//! accessed with `Ordering::Relaxed` — the daemon only needs approximate values
//! for UI display and health checks.

use std::sync::atomic::{AtomicU32, AtomicU64};

/// POSIX shared-memory name. macOS `shm_open` uses namespaced names with a
/// leading slash; no `/tmp/` prefix.
pub const ACTIVITY_SHM_NAME: &str = "/spaghetti.activity";

/// Fixed-layout activity region. The driver writes, the daemon reads.
///
/// The layout is frozen as part of the driver/daemon contract — do not reorder
/// fields or change their types without coordinating both sides.
#[repr(C)]
pub struct ActivityRegion {
    /// Monotonically increasing count of frames written by the driver.
    pub frames_written: AtomicU64,
    /// `mach_absolute_time()` captured at the end of the last `WriteMix` block.
    pub last_write_host_time: AtomicU64,
    /// Peak absolute sample value in the last `WriteMix` block, as `f32::to_bits`.
    pub peak_bits: AtomicU32,
    /// `1` when the driver's IO is active, `0` otherwise.
    pub io_running: AtomicU32,
    /// Reserved padding — keeps the struct at 64 bytes for future fields.
    pub _pad: [u8; 40],
}

impl ActivityRegion {
    /// Zero-initialized region, suitable for populating freshly-mapped shared memory.
    pub const fn new() -> Self {
        Self {
            frames_written: AtomicU64::new(0),
            last_write_host_time: AtomicU64::new(0),
            peak_bits: AtomicU32::new(0),
            io_running: AtomicU32::new(0),
            _pad: [0; 40],
        }
    }
}

impl Default for ActivityRegion {
    fn default() -> Self {
        Self::new()
    }
}

// Compile-time assertion that the region is exactly 64 bytes.
const _: [(); 64] = [(); std::mem::size_of::<ActivityRegion>()];
