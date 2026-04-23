//! Sanity-check that the driver vtable is reachable and property queries work.
//! Loads the cdylib via dlopen (mirroring what coreaudiod does).
//! Run with: cargo test -p spaghetti-asp --test driver_sanity -- --nocapture

use std::ffi::c_void;
use std::ptr;

use coreaudio_sys::{
    kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreams, kAudioHardwareNoError, kAudioObjectPropertyOwnedObjects,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput,
    kAudioPlugInPropertyDeviceList, AudioObjectPropertyAddress,
    AudioServerPlugInDriverInterface, CFUUIDBytes,
};

use core_foundation_sys::string::CFStringRef;

unsafe extern "C" {
    fn dlopen(path: *const i8, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
    fn CFUUIDCreateFromUUIDBytes(alloc: *const c_void, bytes: CFUUIDBytes) -> *const c_void;
}

const RTLD_NOW: i32 = 2;

/// kAudioServerPlugInTypeUUID: 443ABAB8-E7B3-491A-B985-BEB9187030DB
const PLUGIN_TYPE_UUID: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x44, byte1: 0x3A, byte2: 0xBA, byte3: 0xB8,
    byte4: 0xE7, byte5: 0xB3, byte6: 0x49, byte7: 0x1A,
    byte8: 0xB9, byte9: 0x85, byte10: 0xBE, byte11: 0xB9,
    byte12: 0x18, byte13: 0x70, byte14: 0x30, byte15: 0xDB,
};

type FactoryFn = unsafe extern "C" fn(*const c_void, *const c_void) -> *mut c_void;
type DriverRef = *mut *mut AudioServerPlugInDriverInterface;

fn dylib_path() -> std::ffi::CString {
    let manifest = env!("CARGO_MANIFEST_DIR"); // crates/asp-driver
    let ws = std::path::Path::new(manifest).parent().unwrap().parent().unwrap();
    let release = ws.join("target/release/libspaghetti_asp.dylib");
    let debug = ws.join("target/debug/libspaghetti_asp.dylib");
    let path = if release.exists() { release } else { debug };
    std::ffi::CString::new(path.to_str().unwrap()).unwrap()
}

fn get_driver() -> DriverRef {
    unsafe {
        let path = dylib_path();
        let handle = dlopen(path.as_ptr(), RTLD_NOW);
        assert!(!handle.is_null(), "dlopen failed");

        let sym = dlsym(handle, c"spaghetti_create".as_ptr());
        assert!(!sym.is_null(), "dlsym(spaghetti_create) failed");

        let factory: FactoryFn = std::mem::transmute(sym);
        let uuid_ref = CFUUIDCreateFromUUIDBytes(ptr::null(), PLUGIN_TYPE_UUID);
        assert!(!uuid_ref.is_null(), "CFUUIDCreateFromUUIDBytes failed");

        let result = factory(ptr::null(), uuid_ref);
        assert!(!result.is_null(), "Factory returned null — UUID mismatch?");
        result as DriverRef
    }
}

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: 0,
    }
}

#[test]
fn factory_returns_non_null() {
    let driver = get_driver();
    assert!(!driver.is_null());
    let vtable = unsafe { *driver };
    assert!(!vtable.is_null(), "vtable pointer is null");
    eprintln!("OK: driver ref: {driver:p}, vtable: {vtable:p}");
}

#[test]
fn has_property_plugin_device_list() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };
    let has_prop = vtable.HasProperty.expect("HasProperty is None");
    let address = addr(
        kAudioPlugInPropertyDeviceList as u32,
        kAudioObjectPropertyScopeGlobal as u32,
    );
    let result = unsafe { has_prop(driver, 1, 0, &address) };
    eprintln!("HasProperty(plugin=1, DeviceList): {result}");
    assert_eq!(result, 1, "Plugin should report having DeviceList");
}

#[test]
fn get_device_list_returns_device_id() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };

    let get_size = vtable.GetPropertyDataSize.expect("GetPropertyDataSize is None");
    let address = addr(
        kAudioPlugInPropertyDeviceList as u32,
        kAudioObjectPropertyScopeGlobal as u32,
    );
    let mut size: u32 = 0;
    let status = unsafe { get_size(driver, 1, 0, &address, 0, ptr::null(), &mut size) };
    eprintln!("GetPropertyDataSize(plugin=1, DeviceList): status={status}, size={size}");
    assert_eq!(status, kAudioHardwareNoError as i32);
    assert_eq!(size, 4);

    let get_data = vtable.GetPropertyData.expect("GetPropertyData is None");
    let mut device_id: u32 = 0;
    let mut out_size: u32 = 0;
    let status = unsafe {
        get_data(driver, 1, 0, &address, 0, ptr::null(), 4, &mut out_size, &mut device_id as *mut u32 as *mut c_void)
    };
    eprintln!("GetPropertyData(plugin=1, DeviceList): status={status}, device_id={device_id}");
    assert_eq!(status, kAudioHardwareNoError as i32);
    assert_eq!(device_id, 2);
}

#[test]
fn device_has_output_streams() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };
    let get_data = vtable.GetPropertyData.expect("GetPropertyData is None");

    let address = addr(kAudioDevicePropertyStreams as u32, kAudioObjectPropertyScopeOutput as u32);
    let mut stream_id: u32 = 0;
    let mut out_size: u32 = 0;
    let status = unsafe {
        get_data(driver, 2, 0, &address, 0, ptr::null(), 4, &mut out_size, &mut stream_id as *mut u32 as *mut c_void)
    };
    eprintln!("Streams(device=2, output): status={status}, stream_id={stream_id}");
    assert_eq!(status, kAudioHardwareNoError as i32);
    assert_eq!(stream_id, 4);
}

#[test]
fn device_can_be_default() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };
    let get_data = vtable.GetPropertyData.expect("GetPropertyData is None");

    let address = addr(kAudioDevicePropertyDeviceCanBeDefaultDevice as u32, kAudioObjectPropertyScopeOutput as u32);
    let mut val: u32 = 0;
    let mut out_size: u32 = 0;
    let status = unsafe {
        get_data(driver, 2, 0, &address, 0, ptr::null(), 4, &mut out_size, &mut val as *mut u32 as *mut c_void)
    };
    eprintln!("CanBeDefault(device=2, output): status={status}, val={val}");
    assert_eq!(status, kAudioHardwareNoError as i32);
    assert_eq!(val, 1);
}

#[test]
fn device_uid_returns_cfstring() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };
    let get_data = vtable.GetPropertyData.expect("GetPropertyData is None");

    let address = addr(kAudioDevicePropertyDeviceUID as u32, kAudioObjectPropertyScopeGlobal as u32);
    let mut uid: CFStringRef = ptr::null();
    let mut out_size: u32 = 0;
    let status = unsafe {
        get_data(
            driver, 2, 0, &address, 0, ptr::null(),
            std::mem::size_of::<CFStringRef>() as u32,
            &mut out_size, &mut uid as *mut CFStringRef as *mut c_void,
        )
    };
    eprintln!("DeviceUID(device=2): status={status}, uid={uid:p}, out_size={out_size}");
    assert_eq!(status, kAudioHardwareNoError as i32);
    assert!(!uid.is_null());
}

#[test]
fn device_owned_objects() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };
    let get_data = vtable.GetPropertyData.expect("GetPropertyData is None");

    let address = addr(kAudioObjectPropertyOwnedObjects as u32, kAudioObjectPropertyScopeGlobal as u32);
    let mut ids: [u32; 2] = [0, 0];
    let mut out_size: u32 = 0;
    let status = unsafe {
        get_data(driver, 2, 0, &address, 0, ptr::null(), 8, &mut out_size, ids.as_mut_ptr() as *mut c_void)
    };
    eprintln!("OwnedObjects(device=2): status={status}, ids={ids:?}");
    assert_eq!(status, kAudioHardwareNoError as i32);
    assert_eq!(ids, [3, 4]);
}

#[test]
fn query_interface_driver_uuid() {
    let driver = get_driver();
    let vtable = unsafe { &**driver };
    let qi = vtable.QueryInterface.expect("QueryInterface is None");

    // kAudioServerPlugInDriverInterfaceUUID: EEA5773D-CC43-49F1-8E00-8F96E7D23B17
    let iface_uuid = CFUUIDBytes {
        byte0: 0xEE, byte1: 0xA5, byte2: 0x77, byte3: 0x3D,
        byte4: 0xCC, byte5: 0x43, byte6: 0x49, byte7: 0xF1,
        byte8: 0x8E, byte9: 0x00, byte10: 0x8F, byte11: 0x96,
        byte12: 0xE7, byte13: 0xD2, byte14: 0x3B, byte15: 0x17,
    };

    let mut out: *mut c_void = ptr::null_mut();
    let result = unsafe { qi(driver as *mut c_void, iface_uuid, &mut out) };
    eprintln!("QueryInterface(driver_iface): result={result}, out={out:p}, driver={driver:p}");
    assert_eq!(result, 0, "Should return S_OK");
    assert!(!out.is_null());
    assert_eq!(out, driver as *mut c_void, "Should return same driver ref");
}
