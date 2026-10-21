//! Spaghetti AudioServerPlugin driver.
//! Loaded by coreaudiod to create virtual audio devices.

mod devices;
pub(crate) mod driver;
mod iface;
pub(crate) mod registry;
mod watcher;

use std::ffi::c_void;

/// Append a timestamped line to /tmp/spaghetti-driver.log.
/// File I/O errors are silently swallowed — logging must never crash the host.
pub(crate) fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/spaghetti-driver.log")
    {
        let _ = writeln!(f, "[spaghetti-asp] {msg}");
    }
}

use coreaudio_sys::{CFAllocatorRef, CFUUIDBytes, CFUUIDGetUUIDBytes, CFUUIDRef};

/// kAudioServerPlugInTypeUUID: 443ABAB8-E7B3-491A-B985-BEB9187030DB
const PLUGIN_TYPE_UUID: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x44, byte1: 0x3A, byte2: 0xBA, byte3: 0xB8,
    byte4: 0xE7, byte5: 0xB3, byte6: 0x49, byte7: 0x1A,
    byte8: 0xB9, byte9: 0x85, byte10: 0xBE, byte11: 0xB9,
    byte12: 0x18, byte13: 0x70, byte14: 0x30, byte15: 0xDB,
};

fn uuid_bytes(u: &CFUUIDBytes) -> [u8; 16] {
    [
        u.byte0, u.byte1, u.byte2, u.byte3, u.byte4, u.byte5, u.byte6, u.byte7,
        u.byte8, u.byte9, u.byte10, u.byte11, u.byte12, u.byte13, u.byte14, u.byte15,
    ]
}

pub(crate) fn uuid_eq(a: &CFUUIDBytes, b: &CFUUIDBytes) -> bool {
    uuid_bytes(a) == uuid_bytes(b)
}

/// The CFPlugIn factory function. coreaudiod calls this after loading the bundle,
/// passing the UUID of the interface it wants. We return the driver ref if it
/// matches `kAudioServerPlugInTypeUUID`.
///
/// # Safety
///
/// `requested_type_uuid` must be a valid, non-null `CFUUIDRef` allocated by
/// Core Foundation. The caller (coreaudiod / CFPlugIn) always guarantees this.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn spaghetti_create(
    _allocator: CFAllocatorRef,
    requested_type_uuid: CFUUIDRef,
) -> *mut c_void {
    log("spaghetti_create: factory called");
    let requested = unsafe { CFUUIDGetUUIDBytes(requested_type_uuid) };
    let matched = uuid_eq(&requested, &PLUGIN_TYPE_UUID);
    log(&format!(
        "spaghetti_create: uuid={:02x}{:02x}{:02x}{:02x}-... matched_plugin_type={}",
        requested.byte0, requested.byte1, requested.byte2, requested.byte3, matched
    ));
    if matched {
        log("spaghetti_create: returning DRIVER_REF");
        iface::DRIVER_REF.0 as *mut c_void
    } else {
        log("spaghetti_create: UUID mismatch, returning null");
        std::ptr::null_mut()
    }
}
