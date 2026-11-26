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
    /// Claude: the device's volume scalar, as `f32::to_bits`, written as soon
    /// as it is set. Core Audio holds up clients reading a driver's volume for
    /// tens of milliseconds after each change; this does not wait for it.
    pub volume_bits: AtomicU32,
    /// Claude: `1` while the device is muted, written as soon as it is set.
    pub muted: AtomicU32,
    /// Claude: counts changes of the volume and mute. The driver raises it
    /// after writing them and wakes every process waiting on its address
    /// (`os_sync_wait_on_address`, shared).
    pub level_changes: AtomicU32,
    /// Reserved padding — keeps the struct at 64 bytes for future fields.
    pub _pad: [u8; 28],
}

impl ActivityRegion {
    /// Zero-initialized region, suitable for populating freshly-mapped shared memory.
    pub const fn new() -> Self {
        Self {
            frames_written: AtomicU64::new(0),
            last_write_host_time: AtomicU64::new(0),
            peak_bits: AtomicU32::new(0),
            io_running: AtomicU32::new(0),
            volume_bits: AtomicU32::new(0x3f80_0000),
            muted: AtomicU32::new(0),
            level_changes: AtomicU32::new(0),
            _pad: [0; 28],
        }
    }
}

/// Claude: raising and waiting on `level_changes` across processes. The wait
/// and wake calls arrived in macOS 14.4, so they are looked up when first used;
/// without them a wait polls.
#[cfg(target_os = "macos")]
impl ActivityRegion {
    /// Counts a volume or mute change, written before this, and wakes every
    /// process waiting for one.
    pub fn count_level_change(&self) {
        use std::sync::atomic::Ordering;
        self.level_changes.fetch_add(1, Ordering::Release);
        if let Some(wake) = sync::wake() {
            unsafe { wake(self.level_changes_address(), 4, sync::SHARED) };
        }
    }

    /// Blocks until `level_changes` moves past `seen`, or `timeout` passes,
    /// and returns it then.
    pub fn wait_for_level_change(&self, seen: u32, timeout: std::time::Duration) -> u32 {
        use std::sync::atomic::Ordering;
        if self.level_changes.load(Ordering::Acquire) == seen {
            match sync::wait() {
                Some(wait) => {
                    let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX).max(1);
                    unsafe {
                        wait(
                            self.level_changes_address(),
                            u64::from(seen),
                            4,
                            sync::SHARED,
                            sync::MACH_ABSOLUTE_TIME,
                            timeout_ns,
                        )
                    };
                }
                None => std::thread::sleep(timeout.min(sync::POLL)),
            }
        }
        self.level_changes.load(Ordering::Acquire)
    }

    fn level_changes_address(&self) -> *mut std::ffi::c_void {
        std::ptr::from_ref(&self.level_changes).cast_mut().cast()
    }
}

#[cfg(target_os = "macos")]
mod sync {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;
    use std::time::Duration;

    pub const SHARED: u32 = 1;
    pub const MACH_ABSOLUTE_TIME: u32 = 32;
    pub const POLL: Duration = Duration::from_millis(5);

    pub type Wait = unsafe extern "C" fn(*mut c_void, u64, usize, u32, u32, u64) -> i32;
    pub type Wake = unsafe extern "C" fn(*mut c_void, usize, u32) -> i32;

    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    fn lookup(symbol: &std::ffi::CStr) -> Option<usize> {
        let address = unsafe { dlsym(RTLD_DEFAULT, symbol.as_ptr()) };
        (!address.is_null()).then_some(address as usize)
    }

    pub fn wait() -> Option<Wait> {
        static WAIT: OnceLock<Option<usize>> = OnceLock::new();
        let address = (*WAIT.get_or_init(|| lookup(c"os_sync_wait_on_address_with_timeout")))?;
        Some(unsafe { std::mem::transmute::<usize, Wait>(address) })
    }

    pub fn wake() -> Option<Wake> {
        static WAKE: OnceLock<Option<usize>> = OnceLock::new();
        let address = (*WAKE.get_or_init(|| lookup(c"os_sync_wake_by_address_all")))?;
        Some(unsafe { std::mem::transmute::<usize, Wake>(address) })
    }
}

impl Default for ActivityRegion {
    fn default() -> Self {
        Self::new()
    }
}

// Compile-time assertion that the region is exactly 64 bytes.
const _: [(); 64] = [(); std::mem::size_of::<ActivityRegion>()];
