//! Claude: Core Audio property listeners through `bridge/watch.m`, whose
//! calls each hold the watcher alive, however late the HAL makes them.

use std::ffi::c_void;
use std::sync::Arc;

use coreaudio_sys::{AudioObjectID, OSStatus};

unsafe extern "C" {
    unsafe fn spaghetti_watch_new(
        notify: extern "C" fn(*mut c_void),
        release: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> *mut c_void;

    unsafe fn spaghetti_watch_add(
        watch: *mut c_void,
        object: AudioObjectID,
        selector: u32,
        scope: u32,
    ) -> OSStatus;

    unsafe fn spaghetti_watch_end(watch: *mut c_void);
}

/// Claude: what a watch tells about a change to a property it watches, on the
/// watch's own queue.
pub trait Watcher: Send + Sync + 'static {
    fn changed(&self);
}

extern "C" fn notify<W: Watcher>(context: *mut c_void) {
    unsafe { &*context.cast::<W>() }.changed();
}

extern "C" fn release<W: Watcher>(context: *mut c_void) {
    drop(unsafe { Arc::from_raw(context.cast::<W>()) });
}

/// Claude: listeners on Core Audio properties, all calling one watcher.
/// Dropping it removes them and returns once every call already queued has
/// run; the watcher is released once no call can reach it any more.
pub struct Watch(*mut c_void);

// Claude: the bridge's watch is an Objective-C object behind a retained
// pointer; adding to it and ending it happen from the thread that owns this.
unsafe impl Send for Watch {}

impl Watch {
    pub fn new<W: Watcher>(watcher: Arc<W>) -> Self {
        let context = Arc::into_raw(watcher).cast_mut().cast();
        Self(unsafe { spaghetti_watch_new(notify::<W>, release::<W>, context) })
    }

    pub fn add(&mut self, object: AudioObjectID, selector: u32, scope: u32) -> Result<(), String> {
        match unsafe { spaghetti_watch_add(self.0, object, selector, scope) } {
            0 => Ok(()),
            status => Err(format!(
                "could not watch property {selector:#x} of object {object}: {status:#010x}"
            )),
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { spaghetti_watch_end(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use coreaudio_sys::{
        kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    };

    use super::*;

    struct Counted(Arc<AtomicUsize>);

    impl Watcher for Counted {
        fn changed(&self) {}
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_watcher_is_released_once_its_watch_ends() {
        let released = Arc::new(AtomicUsize::new(0));
        let mut watch = Watch::new(Arc::new(Counted(Arc::clone(&released))));
        watch
            .add(
                kAudioObjectSystemObject,
                kAudioHardwarePropertyDevices,
                kAudioObjectPropertyScopeGlobal,
            )
            .unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 0);
        drop(watch);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_watch_that_watched_nothing_releases_its_watcher() {
        let released = Arc::new(AtomicUsize::new(0));
        let mut watch = Watch::new(Arc::new(Counted(Arc::clone(&released))));
        assert!(
            watch
                .add(0xdead_beef, kAudioHardwarePropertyDevices, 0)
                .is_err()
        );
        drop(watch);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
