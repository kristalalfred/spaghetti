//! Reader for the driver/daemon shared-memory activity region.
//!
//! The driver creates its region (`/spaghetti.activity` for spaghetti's own
//! driver) exclusively on each load. The daemon opens it read-write without
//! creating; if the driver hasn't loaded
//! yet, `open` returns an error and callers retry lazily. Atomic reads use
//! `Relaxed` — values are advisory and only drive UI display and staleness
//! checks.

use std::ffi::CString;
use std::io;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use spaghetti_common::activity::ActivityRegion;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// Point-in-time snapshot of the shared activity region.
#[derive(Debug, Clone, Copy)]
pub struct ActivitySnapshot {
    pub frames_written: u64,
    pub last_write_host_time: u64,
    pub peak: f32,
    pub io_running: bool,
    pub volume: f32,
    pub muted: bool,
    pub level_changes: u32,
}

/// Mapped handle onto the driver's activity region. Safe to share across threads —
/// all reads go through atomics.
pub struct ActivityReader {
    ptr: *const ActivityRegion,
    fd: i32,
    timebase_numer: u64,
    timebase_denom: u64,
}

// SAFETY: The pointer targets a shared mmap region and all accesses go through
// atomic operations, which are sound from any thread.
unsafe impl Send for ActivityReader {}
unsafe impl Sync for ActivityReader {}

impl ActivityReader {
    /// Open the shared activity region a driver created under `name`. Returns
    /// an error if the driver hasn't created it yet; callers should retry
    /// lazily.
    pub fn open(name: &str) -> io::Result<Self> {
        let name =
            CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let size = std::mem::size_of::<ActivityRegion>();

        // Open existing region only — the driver owns creation and sets perms.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Best-effort fchmod; EPERM is expected when the driver (a different
        // user) already set the perms correctly.
        if unsafe { libc::fchmod(fd, 0o666) } != 0 {
            // non-fatal: driver already called fchmod(0o666) on creation
            let _ = unsafe { *libc::__error() }; // read and discard errno
        }

        // Map read+write: some platforms refuse atomic RMWs on PROT_READ
        // mappings, and atomic reads on the shared region compile to ordinary
        // loads only when the mapping permits writes.
        let addr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        let (numer, denom) = mach_timebase();

        Ok(Self {
            ptr: addr.cast::<ActivityRegion>(),
            fd,
            timebase_numer: u64::from(numer),
            timebase_denom: u64::from(denom),
        })
    }

    /// Read all fields once with `Relaxed` ordering and return plain values.
    pub fn snapshot(&self) -> ActivitySnapshot {
        // SAFETY: `ptr` is a valid mmap of an `ActivityRegion`, pinned for the
        // lifetime of `self`.
        let region = unsafe { &*self.ptr };
        ActivitySnapshot {
            frames_written: region.frames_written.load(Ordering::Relaxed),
            last_write_host_time: region.last_write_host_time.load(Ordering::Relaxed),
            peak: f32::from_bits(region.peak_bits.load(Ordering::Relaxed)),
            io_running: region.io_running.load(Ordering::Relaxed) == 1,
            level_changes: region.level_changes.load(Ordering::Acquire),
            volume: f32::from_bits(region.volume_bits.load(Ordering::Relaxed)),
            muted: region.muted.load(Ordering::Relaxed) == 1,
        }
    }

    /// Claude: blocks until the driver counts a volume or mute change past
    /// `seen`, or `timeout` passes, and returns the count then. The driver
    /// wakes the wait as it writes the change.
    pub fn wait_for_level_change(&self, seen: u32, timeout: Duration) -> u32 {
        // SAFETY: as in `snapshot`.
        unsafe { &*self.ptr }.wait_for_level_change(seen, timeout)
    }

    /// True when the driver has not written a frame within `freshness_ns`.
    pub fn is_stale(&self, last_write_host_time: u64, freshness_ns: u64) -> bool {
        if last_write_host_time == 0 {
            return true;
        }
        let now_ticks = unsafe { mach_absolute_time() };
        let delta_ticks = now_ticks.saturating_sub(last_write_host_time);
        // ticks → ns via numer/denom
        let delta_ns = delta_ticks
            .saturating_mul(self.timebase_numer)
            / self.timebase_denom.max(1);
        delta_ns > freshness_ns
    }
}

impl Drop for ActivityReader {
    fn drop(&mut self) {
        let size = std::mem::size_of::<ActivityRegion>();
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, size);
            libc::close(self.fd);
        }
    }
}

fn mach_timebase() -> (u32, u32) {
    let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
    unsafe { mach_timebase_info(&mut info) };
    if info.denom == 0 {
        (1, 1)
    } else {
        (info.numer, info.denom)
    }
}

/// Tracks the previous sample used to compute a rolling frames-per-second.
///
/// Updated no more than twice per second; between updates, callers read back
/// the cached `fps` so short-interval polling doesn't alias to zero.
#[derive(Default)]
pub struct FpsTracker {
    inner: Mutex<Option<FpsSample>>,
}

struct FpsSample {
    frames: u64,
    when: Instant,
    fps: u64,
}

impl FpsTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute frames-per-second given the latest total frame count.
    ///
    /// Uses a 500ms minimum window. If called more frequently, returns the last
    /// computed value; if this is the first call, seeds the tracker and returns 0.
    pub fn update(&self, frames: u64) -> u64 {
        let now = Instant::now();
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        match guard.as_mut() {
            None => {
                *guard = Some(FpsSample {
                    frames,
                    when: now,
                    fps: 0,
                });
                0
            }
            Some(prev) => {
                let elapsed_ns = now.duration_since(prev.when).as_nanos() as u64;
                if elapsed_ns < 500_000_000 {
                    return prev.fps;
                }
                let delta = frames.saturating_sub(prev.frames);
                let fps = delta.saturating_mul(1_000_000_000) / elapsed_ns;
                prev.frames = frames;
                prev.when = now;
                prev.fps = fps;
                fps
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use spaghetti_common::activity::ActivityRegion;

    use super::ActivityReader;

    /// Claude: a region as a driver makes it, unlinked when dropped.
    struct Region {
        name: std::ffi::CString,
        ptr: *mut ActivityRegion,
    }

    impl Region {
        fn create(name: &str) -> Self {
            let name = std::ffi::CString::new(name).unwrap();
            let size = std::mem::size_of::<ActivityRegion>();
            unsafe {
                libc::shm_unlink(name.as_ptr());
                let fd = libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
                assert!(fd >= 0);
                assert_eq!(libc::ftruncate(fd, size as libc::off_t), 0);
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                );
                libc::close(fd);
                assert_ne!(ptr, libc::MAP_FAILED);
                std::ptr::write(ptr.cast(), ActivityRegion::new());
                Self {
                    name,
                    ptr: ptr.cast(),
                }
            }
        }

        fn change_level(&self) {
            unsafe { &*self.ptr }.count_level_change();
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            unsafe { libc::shm_unlink(self.name.as_ptr()) };
        }
    }

    struct Shared(Region);
    unsafe impl Send for Shared {}
    unsafe impl Sync for Shared {}

    #[test]
    fn a_wait_ends_when_the_level_changes_or_when_it_times_out() {
        let name = format!("/spaghetti.test.{}", std::process::id());
        let region = std::sync::Arc::new(Shared(Region::create(&name)));
        let reader = ActivityReader::open(&name).unwrap();

        let started = Instant::now();
        assert_eq!(
            reader.wait_for_level_change(0, Duration::from_millis(50)),
            0
        );
        assert!(started.elapsed() >= Duration::from_millis(40));

        let changer = std::thread::spawn({
            let region = std::sync::Arc::clone(&region);
            move || {
                std::thread::sleep(Duration::from_millis(50));
                region.0.change_level();
            }
        });
        let started = Instant::now();
        assert_eq!(reader.wait_for_level_change(0, Duration::from_secs(5)), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
        changer.join().unwrap();
        assert_eq!(reader.snapshot().level_changes, 1);
    }
}
