//! Spaghetti AudioServerPlugin driver.
//! Loaded by coreaudiod to create virtual audio devices.

mod driver;
mod iface;

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

pub(crate) fn uuid_eq(a: &CFUUIDBytes, b: &CFUUIDBytes) -> bool {
    unsafe {
        std::slice::from_raw_parts(a as *const CFUUIDBytes as *const u8, 16)
            == std::slice::from_raw_parts(b as *const CFUUIDBytes as *const u8, 16)
    }
}

/// The CFPlugIn factory function. coreaudiod calls this after loading the bundle,
/// passing the UUID of the interface it wants. We return the driver ref if it
/// matches `kAudioServerPlugInTypeUUID`.
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
