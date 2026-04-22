use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8};
use coreaudio_sys::{
    AudioObjectID, AudioObjectPropertyAddress, AudioServerPlugInClientInfo,
    AudioServerPlugInHostInterface, AudioServerPlugInIOCycleInfo, AudioStreamBasicDescription,
    AudioValueRange, CFDictionaryRef, CFUUIDBytes, OSStatus, kAudioDeviceClassID,
    kAudioDevicePropertyAvailableNominalSampleRates, kAudioDevicePropertyClockDomain,
    kAudioDevicePropertyDeviceCanBeDefaultDevice,
    kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyDeviceIsRunning, kAudioDevicePropertyDeviceUID, kAudioDevicePropertyIcon,
    kAudioDevicePropertyLatency, kAudioDevicePropertyModelUID,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyPreferredChannelsForStereo,
    kAudioDevicePropertyRelatedDevices, kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreams, kAudioDevicePropertyTransportType,
    kAudioDevicePropertyZeroTimeStampPeriod, kAudioDeviceTransportTypeVirtual,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatFlagsNativeEndian,
    kAudioFormatLinearPCM, kAudioHardwareBadObjectError, kAudioHardwareNoError,
    kAudioHardwareUnknownPropertyError, kAudioObjectClassID, kAudioObjectPropertyBaseClass,
    kAudioObjectPropertyClass, kAudioObjectPropertyManufacturer, kAudioObjectPropertyName,
    kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioPlugInClassID,
    kAudioPlugInPropertyDeviceList, kAudioPlugInPropertyResourceBundle,
    kAudioPlugInPropertyTranslateUIDToDevice, kAudioServerPlugInIOOperationReadInput,
    kAudioServerPlugInIOOperationWriteMix, kAudioStreamClassID,
    kAudioStreamPropertyAvailablePhysicalFormats, kAudioStreamPropertyAvailableVirtualFormats,
    kAudioStreamPropertyDirection, kAudioStreamPropertyIsActive, kAudioStreamPropertyLatency,
    kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyStartingChannel,
    kAudioStreamPropertyTerminalType, kAudioStreamPropertyVirtualFormat,
    kAudioStreamTerminalTypeLine, kAudioStreamTerminalTypeMicrophone,
};

use crate::iface::{DRIVER_INTERFACE_UUID, DRIVER_REF, DriverRef};
use crate::log;
use spaghetti_common::audio_types::{
    BYTES_PER_FRAME, BYTES_PER_SAMPLE, CHANNELS, RING_BUFFER_FRAMES, SAMPLE_RATE,
};

// ---------------------------------------------------------------------------
// Object IDs
// ---------------------------------------------------------------------------

const OBJECT_ID_PLUGIN: u32 = 1; // kAudioObjectPlugInObject
const OBJECT_ID_DEVICE: u32 = 2;
const OBJECT_ID_STREAM_INPUT: u32 = 3;
const OBJECT_ID_STREAM_OUTPUT: u32 = 4;

// kAudioDevicePropertyControlList ('ctrl') — not exported by coreaudio-sys
const K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST: u32 = 0x6374_726c;
// kAudioDevicePropertyIsHidden ('hidn') — not exported by coreaudio-sys
const K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN: u32 = 0x6869_646e;

const NUM_CHANNELS: usize = CHANNELS as usize;

// ---------------------------------------------------------------------------
// AudioStreamRangedDescription (not in coreaudio-sys bindings)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone)]
struct AudioStreamRangedDescription {
    format: AudioStreamBasicDescription,
    sample_rate_range: AudioValueRange,
}

// ---------------------------------------------------------------------------
// Mach time
// ---------------------------------------------------------------------------

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

// ---------------------------------------------------------------------------
// Driver state
// ---------------------------------------------------------------------------

/// IUnknown UUID: 00000000-0000-0000-C000-000000000046
const IUNKNOWN_UUID: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x00,
    byte1: 0x00,
    byte2: 0x00,
    byte3: 0x00,
    byte4: 0x00,
    byte5: 0x00,
    byte6: 0x00,
    byte7: 0x00,
    byte8: 0xC0,
    byte9: 0x00,
    byte10: 0x00,
    byte11: 0x00,
    byte12: 0x00,
    byte13: 0x00,
    byte14: 0x00,
    byte15: 0x46,
};

pub struct DriverState {
    pub ref_count: AtomicU32,
    /// Pointer to the host interface; null until Initialize is called.
    pub host: AtomicPtr<AudioServerPlugInHostInterface>,
}

// SAFETY: all mutable state is behind atomics.
unsafe impl Sync for DriverState {}

pub static DRIVER: DriverState = DriverState {
    ref_count: AtomicU32::new(1),
    host: AtomicPtr::new(std::ptr::null_mut()),
};

// IO state
static IO_RUNNING_COUNT: AtomicU32 = AtomicU32::new(0);
static ANCHOR_HOST_TIME: AtomicU64 = AtomicU64::new(0);
static TIMESTAMP_COUNT: AtomicU64 = AtomicU64::new(0);
/// f64 bits for host ticks per frame
static HOST_TICKS_PER_FRAME: AtomicU64 = AtomicU64::new(0);

// Ring buffer for routing audio between output writers and input readers.
// AtomicU32 stores f32 bits, enabling lock-free cross-thread access.
static RING_BUFFER: [AtomicU32; RING_BUFFER_FRAMES * NUM_CHANNELS] = {
    const ZERO: AtomicU32 = AtomicU32::new(0);
    [ZERO; RING_BUFFER_FRAMES * NUM_CHANNELS]
};

const UNSUPPORTED: OSStatus = 0x756E_6F70u32 as OSStatus; // 'unop' = kAudioHardwareUnsupportedOperationError

use crate::uuid_eq;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a CFStringRef from a Rust string literal. The caller owns the reference.
fn cfstr(s: &str) -> CFStringRef {
    let cstr = std::ffi::CString::new(s).unwrap();
    unsafe { CFStringCreateWithCString(kCFAllocatorDefault, cstr.as_ptr(), kCFStringEncodingUTF8) }
}

/// Write a u32 value to the output buffer.
unsafe fn write_u32(out: *mut c_void, out_size: *mut u32, val: u32) -> OSStatus {
    unsafe {
        *(out as *mut u32) = val;
        *out_size = 4;
    }
    kAudioHardwareNoError as OSStatus
}

/// Write a f64 value to the output buffer.
unsafe fn write_f64(out: *mut c_void, out_size: *mut u32, val: f64) -> OSStatus {
    unsafe {
        *(out as *mut f64) = val;
        *out_size = 8;
    }
    kAudioHardwareNoError as OSStatus
}

/// Write a CFStringRef to the output buffer, retaining it for the caller.
unsafe fn write_cfstr(out: *mut c_void, out_size: *mut u32, s: &str) -> OSStatus {
    let cf = cfstr(s);
    // The HAL takes ownership; we retain so it can CFRelease later.
    // CFStringCreateWithCString already returns +1, so no extra retain needed.
    unsafe {
        *(out as *mut CFStringRef) = cf;
        *out_size = std::mem::size_of::<CFStringRef>() as u32;
    }
    kAudioHardwareNoError as OSStatus
}

/// Build the canonical ASBD for our device.
fn stream_description() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: SAMPLE_RATE,
        mFormatID: kAudioFormatLinearPCM as u32,
        mFormatFlags: (kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagsNativeEndian) as u32,
        mBytesPerPacket: BYTES_PER_FRAME,
        mFramesPerPacket: 1,
        mBytesPerFrame: BYTES_PER_FRAME,
        mChannelsPerFrame: CHANNELS,
        mBitsPerChannel: BYTES_PER_SAMPLE * 8,
        mReserved: 0,
    }
}

// ---------------------------------------------------------------------------
// IUnknown
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn query_interface(
    _driver: *mut c_void,
    iid: CFUUIDBytes,
    out_interface: *mut *mut c_void,
) -> i32 {
    let matched = uuid_eq(&iid, &IUNKNOWN_UUID) || uuid_eq(&iid, &DRIVER_INTERFACE_UUID);
    log(&format!(
        "query_interface: uuid={:02x}{:02x}{:02x}{:02x}-... matched={}",
        iid.byte0, iid.byte1, iid.byte2, iid.byte3, matched
    ));
    if matched {
        unsafe { add_ref(_driver) };
        unsafe { *out_interface = DRIVER_REF.0 as *mut c_void };
        0 // S_OK
    } else {
        unsafe { *out_interface = std::ptr::null_mut() };
        0x80000004u32 as i32 // E_NOINTERFACE
    }
}

pub unsafe extern "C" fn add_ref(_driver: *mut c_void) -> u32 {
    DRIVER.ref_count.fetch_add(1, Ordering::SeqCst) + 1
}

pub unsafe extern "C" fn release(_driver: *mut c_void) -> u32 {
    let prev = DRIVER.ref_count.fetch_sub(1, Ordering::SeqCst);
    prev.saturating_sub(1)
}

// ---------------------------------------------------------------------------
// Basic operations
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn initialize(
    _driver: DriverRef,
    host: *const AudioServerPlugInHostInterface,
) -> OSStatus {
    log(&format!("initialize: host={host:p}"));

    // Save the host interface pointer for LATER use
    DRIVER.host.store(host as *mut _, Ordering::SeqCst);

    log("initialize: returning noErr");
    0 // noErr
}

pub unsafe extern "C" fn create_device(
    _driver: DriverRef,
    _description: CFDictionaryRef,
    _client_info: *const AudioServerPlugInClientInfo,
    _out_device_id: *mut AudioObjectID,
) -> OSStatus {
    UNSUPPORTED
}

pub unsafe extern "C" fn destroy_device(_driver: DriverRef, _device_id: AudioObjectID) -> OSStatus {
    UNSUPPORTED
}

pub unsafe extern "C" fn add_device_client(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_info: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    0 // noErr — accept all clients
}

pub unsafe extern "C" fn remove_device_client(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_info: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    0 // noErr
}

pub unsafe extern "C" fn perform_device_configuration_change(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _change_action: u64,
    _change_info: *mut c_void,
) -> OSStatus {
    UNSUPPORTED
}

pub unsafe extern "C" fn abort_device_configuration_change(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _change_action: u64,
    _change_info: *mut c_void,
) -> OSStatus {
    UNSUPPORTED
}

// ---------------------------------------------------------------------------
// Property dispatch: selectors we handle per object
// ---------------------------------------------------------------------------

/// Returns true if we recognize this property selector for the given object.
fn property_known(object_id: u32, selector: u32, _scope: u32) -> bool {
    match object_id {
        OBJECT_ID_PLUGIN => matches!(
            selector,
            x if x == kAudioObjectPropertyBaseClass as u32
                || x == kAudioObjectPropertyClass as u32
                || x == kAudioObjectPropertyOwner as u32
                || x == kAudioObjectPropertyOwnedObjects as u32
                || x == kAudioObjectPropertyManufacturer as u32
                || x == kAudioPlugInPropertyDeviceList as u32
                || x == kAudioPlugInPropertyTranslateUIDToDevice as u32
                || x == kAudioPlugInPropertyResourceBundle as u32
        ),
        OBJECT_ID_DEVICE => matches!(
            selector,
            x if x == kAudioObjectPropertyBaseClass as u32
                || x == kAudioObjectPropertyClass as u32
                || x == kAudioObjectPropertyOwner as u32
                || x == kAudioObjectPropertyName as u32
                || x == kAudioObjectPropertyManufacturer as u32
                || x == kAudioDevicePropertyDeviceUID as u32
                || x == kAudioDevicePropertyModelUID as u32
                || x == kAudioDevicePropertyTransportType as u32
                || x == kAudioDevicePropertyRelatedDevices as u32
                || x == kAudioDevicePropertyClockDomain as u32
                || x == kAudioDevicePropertyDeviceIsAlive as u32
                || x == kAudioDevicePropertyDeviceIsRunning as u32
                || x == kAudioDevicePropertyDeviceCanBeDefaultDevice as u32
                || x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice as u32
                || x == kAudioDevicePropertyLatency as u32
                || x == kAudioDevicePropertyStreams as u32
                || x == kAudioObjectPropertyOwnedObjects as u32
                || x == kAudioDevicePropertyNominalSampleRate as u32
                || x == kAudioDevicePropertyAvailableNominalSampleRates as u32
                || x == kAudioDevicePropertyPreferredChannelsForStereo as u32
                || x == kAudioDevicePropertyZeroTimeStampPeriod as u32
                || x == kAudioDevicePropertySafetyOffset as u32
                || x == K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST
                || x == K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN
        ),
        OBJECT_ID_STREAM_INPUT | OBJECT_ID_STREAM_OUTPUT => matches!(
            selector,
            x if x == kAudioObjectPropertyBaseClass as u32
                || x == kAudioObjectPropertyClass as u32
                || x == kAudioObjectPropertyOwner as u32
                || x == kAudioStreamPropertyIsActive as u32
                || x == kAudioStreamPropertyDirection as u32
                || x == kAudioStreamPropertyTerminalType as u32
                || x == kAudioStreamPropertyStartingChannel as u32
                || x == kAudioStreamPropertyLatency as u32
                || x == kAudioStreamPropertyVirtualFormat as u32
                || x == kAudioStreamPropertyPhysicalFormat as u32
                || x == kAudioStreamPropertyAvailableVirtualFormats as u32
                || x == kAudioStreamPropertyAvailablePhysicalFormats as u32
        ),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Property operations
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn has_property(
    _driver: DriverRef,
    object_id: AudioObjectID,
    _client_pid: i32,
    address: *const AudioObjectPropertyAddress,
) -> u8 {
    let addr = unsafe { &*address };
    let known = property_known(object_id, addr.mSelector, addr.mScope);
    log(&format!(
        "has_property: object={object_id} selector={:#010x} scope={:#010x} -> {known}",
        addr.mSelector, addr.mScope
    ));
    if known { 1 } else { 0 }
}

pub unsafe extern "C" fn is_property_settable(
    _driver: DriverRef,
    object_id: AudioObjectID,
    _client_pid: i32,
    address: *const AudioObjectPropertyAddress,
    out_settable: *mut u8,
) -> OSStatus {
    let addr = unsafe { &*address };
    if !property_known(object_id, addr.mSelector, addr.mScope) {
        return kAudioHardwareUnknownPropertyError as OSStatus;
    }
    unsafe { *out_settable = 0 }; // everything read-only for now
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn get_property_data_size(
    _driver: DriverRef,
    object_id: AudioObjectID,
    _client_pid: i32,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: u32,
    _qualifier_data: *const c_void,
    out_data_size: *mut u32,
) -> OSStatus {
    let addr = unsafe { &*address };
    let sel = addr.mSelector;
    let scope = addr.mScope;

    let size: u32 = match object_id {
        OBJECT_ID_PLUGIN => plugin_data_size(sel),
        OBJECT_ID_DEVICE => device_data_size(sel, scope),
        OBJECT_ID_STREAM_INPUT | OBJECT_ID_STREAM_OUTPUT => stream_data_size(sel),
        _ => {
            log(&format!(
                "get_property_data_size: object={object_id} selector={sel:#010x} -> kAudioHardwareBadObjectError"
            ));
            return kAudioHardwareBadObjectError as OSStatus;
        }
    };

    // 0 means unknown selector; u32::MAX means known but zero-byte payload (e.g. empty list).
    if size == 0 {
        log(&format!(
            "get_property_data_size: object={object_id} selector={sel:#010x} -> kAudioHardwareUnknownPropertyError"
        ));
        return kAudioHardwareUnknownPropertyError as OSStatus;
    }

    let actual_size = if size == u32::MAX { 0 } else { size };
    log(&format!(
        "get_property_data_size: object={object_id} selector={sel:#010x} scope={scope:#010x} -> size={actual_size}"
    ));
    unsafe { *out_data_size = actual_size };
    kAudioHardwareNoError as OSStatus
}

fn plugin_data_size(sel: u32) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass as u32 => 4,
        x if x == kAudioObjectPropertyClass as u32 => 4,
        x if x == kAudioObjectPropertyOwner as u32 => 4,
        x if x == kAudioObjectPropertyOwnedObjects as u32 => 4, // one device
        x if x == kAudioObjectPropertyManufacturer as u32 => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        x if x == kAudioPlugInPropertyDeviceList as u32 => 4, // one device
        x if x == kAudioPlugInPropertyTranslateUIDToDevice as u32 => 4,
        x if x == kAudioPlugInPropertyResourceBundle as u32 => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        _ => 0,
    }
}

fn device_data_size(sel: u32, scope: u32) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass as u32 => 4,
        x if x == kAudioObjectPropertyClass as u32 => 4,
        x if x == kAudioObjectPropertyOwner as u32 => 4,
        x if x == kAudioObjectPropertyName as u32 => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioObjectPropertyManufacturer as u32 => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        x if x == kAudioDevicePropertyDeviceUID as u32 => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioDevicePropertyModelUID as u32 => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioDevicePropertyTransportType as u32 => 4,
        x if x == kAudioDevicePropertyRelatedDevices as u32 => 4,
        x if x == kAudioDevicePropertyClockDomain as u32 => 4,
        x if x == kAudioDevicePropertyDeviceIsAlive as u32 => 4,
        x if x == kAudioDevicePropertyDeviceIsRunning as u32 => 4,
        x if x == kAudioDevicePropertyDeviceCanBeDefaultDevice as u32 => 4,
        x if x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice as u32 => 4,
        x if x == kAudioDevicePropertyLatency as u32 => 4,
        x if x == kAudioDevicePropertyStreams as u32 => {
            if scope == kAudioObjectPropertyScopeGlobal as u32 {
                8 // two stream IDs
            } else {
                4 // one stream ID
            }
        }
        x if x == kAudioObjectPropertyOwnedObjects as u32 => 8, // two streams
        x if x == kAudioDevicePropertyNominalSampleRate as u32 => 8,
        x if x == kAudioDevicePropertyAvailableNominalSampleRates as u32 => {
            std::mem::size_of::<AudioValueRange>() as u32
        }
        x if x == kAudioDevicePropertyPreferredChannelsForStereo as u32 => 8,
        x if x == kAudioDevicePropertyZeroTimeStampPeriod as u32 => 4,
        x if x == kAudioDevicePropertySafetyOffset as u32 => 4,
        x if x == kAudioDevicePropertyIcon as u32 => 0, // we don't support this
        x if x == K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN => 4,
        // Empty list — known property, zero-byte payload (sentinel u32::MAX).
        x if x == K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST => u32::MAX,
        _ => 0,
    }
}

fn stream_data_size(sel: u32) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass as u32 => 4,
        x if x == kAudioObjectPropertyClass as u32 => 4,
        x if x == kAudioObjectPropertyOwner as u32 => 4,
        x if x == kAudioStreamPropertyIsActive as u32 => 4,
        x if x == kAudioStreamPropertyDirection as u32 => 4,
        x if x == kAudioStreamPropertyTerminalType as u32 => 4,
        x if x == kAudioStreamPropertyStartingChannel as u32 => 4,
        x if x == kAudioStreamPropertyLatency as u32 => 4,
        x if x == kAudioStreamPropertyVirtualFormat as u32
            || x == kAudioStreamPropertyPhysicalFormat as u32 =>
        {
            std::mem::size_of::<AudioStreamBasicDescription>() as u32
        }
        x if x == kAudioStreamPropertyAvailableVirtualFormats as u32
            || x == kAudioStreamPropertyAvailablePhysicalFormats as u32 =>
        {
            std::mem::size_of::<AudioStreamRangedDescription>() as u32
        }
        _ => 0,
    }
}

pub unsafe extern "C" fn get_property_data(
    _driver: DriverRef,
    object_id: AudioObjectID,
    _client_pid: i32,
    address: *const AudioObjectPropertyAddress,
    qualifier_data_size: u32,
    qualifier_data: *const c_void,
    _data_size: u32,
    out_data_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let addr = unsafe { &*address };
    let sel = addr.mSelector;
    let scope = addr.mScope;

    log(&format!(
        "get_property_data: object={object_id} selector={sel:#010x} scope={scope:#010x}"
    ));

    let status = match object_id {
        OBJECT_ID_PLUGIN => unsafe {
            get_plugin_property(
                sel,
                qualifier_data_size,
                qualifier_data,
                out_data_size,
                out_data,
            )
        },
        OBJECT_ID_DEVICE => unsafe { get_device_property(sel, scope, out_data_size, out_data) },
        OBJECT_ID_STREAM_INPUT => unsafe {
            get_stream_property(sel, object_id, out_data_size, out_data)
        },
        OBJECT_ID_STREAM_OUTPUT => unsafe {
            get_stream_property(sel, object_id, out_data_size, out_data)
        },
        _ => kAudioHardwareBadObjectError as OSStatus,
    };

    log(&format!(
        "get_property_data: object={object_id} selector={sel:#010x} -> status={status}"
    ));
    status
}

unsafe fn get_plugin_property(
    sel: u32,
    _qualifier_size: u32,
    _qualifier_data: *const c_void,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    match sel {
        x if x == kAudioObjectPropertyBaseClass as u32 => unsafe {
            write_u32(out, out_size, kAudioObjectClassID as u32)
        },
        x if x == kAudioObjectPropertyClass as u32 => unsafe {
            write_u32(out, out_size, kAudioPlugInClassID as u32)
        },
        x if x == kAudioObjectPropertyOwner as u32 => unsafe {
            write_u32(out, out_size, OBJECT_ID_PLUGIN)
        },
        x if x == kAudioObjectPropertyOwnedObjects as u32 => unsafe {
            write_u32(out, out_size, OBJECT_ID_DEVICE)
        },
        x if x == kAudioObjectPropertyManufacturer as u32 => unsafe {
            write_cfstr(out, out_size, "Spaghetti")
        },
        x if x == kAudioPlugInPropertyDeviceList as u32 => unsafe {
            let ids = [OBJECT_ID_DEVICE];
            std::ptr::copy_nonoverlapping(ids.as_ptr(), out as *mut u32, 1);
            *out_size = 4;
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioPlugInPropertyTranslateUIDToDevice as u32 => {
            // For now just return the device — we only have one.
            unsafe { write_u32(out, out_size, OBJECT_ID_DEVICE) }
        }
        x if x == kAudioPlugInPropertyResourceBundle as u32 => unsafe {
            write_cfstr(out, out_size, "")
        },
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

unsafe fn get_device_property(
    sel: u32,
    scope: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    match sel {
        x if x == kAudioObjectPropertyBaseClass as u32 => unsafe {
            write_u32(out, out_size, kAudioObjectClassID as u32)
        },
        x if x == kAudioObjectPropertyClass as u32 => unsafe {
            write_u32(out, out_size, kAudioDeviceClassID as u32)
        },
        x if x == kAudioObjectPropertyOwner as u32 => unsafe {
            write_u32(out, out_size, OBJECT_ID_PLUGIN)
        },
        x if x == kAudioObjectPropertyName as u32 => unsafe {
            write_cfstr(out, out_size, "Spaghetti")
        },
        x if x == kAudioObjectPropertyManufacturer as u32 => unsafe {
            write_cfstr(out, out_size, "Spaghetti")
        },
        x if x == kAudioDevicePropertyDeviceUID as u32 => unsafe {
            write_cfstr(out, out_size, "SpaghettiDevice_UID")
        },
        x if x == kAudioDevicePropertyModelUID as u32 => unsafe {
            write_cfstr(out, out_size, "SpaghettiModel_UID")
        },
        x if x == kAudioDevicePropertyTransportType as u32 => unsafe {
            write_u32(out, out_size, kAudioDeviceTransportTypeVirtual as u32)
        },
        x if x == kAudioDevicePropertyRelatedDevices as u32 => unsafe {
            let ids = [OBJECT_ID_DEVICE];
            std::ptr::copy_nonoverlapping(ids.as_ptr(), out as *mut u32, 1);
            *out_size = 4;
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioDevicePropertyClockDomain as u32 => unsafe { write_u32(out, out_size, 0) },
        x if x == kAudioDevicePropertyDeviceIsAlive as u32 => unsafe {
            write_u32(out, out_size, 1)
        },
        x if x == kAudioDevicePropertyDeviceIsRunning as u32 => unsafe {
            let running = if IO_RUNNING_COUNT.load(Ordering::SeqCst) > 0 {
                1u32
            } else {
                0u32
            };
            write_u32(out, out_size, running)
        },
        x if x == kAudioDevicePropertyDeviceCanBeDefaultDevice as u32 => unsafe {
            write_u32(out, out_size, 1)
        },
        x if x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice as u32 => unsafe {
            write_u32(out, out_size, 1)
        },
        x if x == kAudioDevicePropertyLatency as u32 => unsafe { write_u32(out, out_size, 0) },
        x if x == kAudioDevicePropertyStreams as u32 => unsafe {
            match scope {
                s if s == kAudioObjectPropertyScopeInput as u32 => {
                    let ids = [OBJECT_ID_STREAM_INPUT];
                    std::ptr::copy_nonoverlapping(ids.as_ptr(), out as *mut u32, 1);
                    *out_size = 4;
                }
                s if s == kAudioObjectPropertyScopeOutput as u32 => {
                    let ids = [OBJECT_ID_STREAM_OUTPUT];
                    std::ptr::copy_nonoverlapping(ids.as_ptr(), out as *mut u32, 1);
                    *out_size = 4;
                }
                _ => {
                    // Global scope: both streams
                    let ids = [OBJECT_ID_STREAM_INPUT, OBJECT_ID_STREAM_OUTPUT];
                    std::ptr::copy_nonoverlapping(ids.as_ptr(), out as *mut u32, 2);
                    *out_size = 8;
                }
            }
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioObjectPropertyOwnedObjects as u32 => unsafe {
            let ids = [OBJECT_ID_STREAM_INPUT, OBJECT_ID_STREAM_OUTPUT];
            std::ptr::copy_nonoverlapping(ids.as_ptr(), out as *mut u32, 2);
            *out_size = 8;
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioDevicePropertyNominalSampleRate as u32 => unsafe {
            write_f64(out, out_size, SAMPLE_RATE)
        },
        x if x == kAudioDevicePropertyAvailableNominalSampleRates as u32 => unsafe {
            let range = AudioValueRange {
                mMinimum: SAMPLE_RATE,
                mMaximum: SAMPLE_RATE,
            };
            std::ptr::copy_nonoverlapping(
                &range as *const AudioValueRange,
                out as *mut AudioValueRange,
                1,
            );
            *out_size = std::mem::size_of::<AudioValueRange>() as u32;
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioDevicePropertyPreferredChannelsForStereo as u32 => unsafe {
            let channels: [u32; 2] = [1, 2];
            std::ptr::copy_nonoverlapping(channels.as_ptr(), out as *mut u32, 2);
            *out_size = 8;
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioDevicePropertyZeroTimeStampPeriod as u32 => unsafe {
            write_u32(out, out_size, RING_BUFFER_FRAMES as u32)
        },
        x if x == kAudioDevicePropertySafetyOffset as u32 => unsafe { write_u32(out, out_size, 0) },
        x if x == kAudioDevicePropertyIcon as u32 => kAudioHardwareUnknownPropertyError as OSStatus,
        // Not hidden — coreaudiod queries this directly without checking has_property.
        x if x == K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN => unsafe { write_u32(out, out_size, 0) },
        // Empty control list.
        x if x == K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST => unsafe {
            *out_size = 0;
            kAudioHardwareNoError as OSStatus
        },
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

unsafe fn get_stream_property(
    sel: u32,
    object_id: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    match sel {
        x if x == kAudioObjectPropertyBaseClass as u32 => unsafe {
            write_u32(out, out_size, kAudioObjectClassID as u32)
        },
        x if x == kAudioObjectPropertyClass as u32 => unsafe {
            write_u32(out, out_size, kAudioStreamClassID as u32)
        },
        x if x == kAudioObjectPropertyOwner as u32 => unsafe {
            write_u32(out, out_size, OBJECT_ID_DEVICE)
        },
        x if x == kAudioStreamPropertyIsActive as u32 => unsafe { write_u32(out, out_size, 1) },
        x if x == kAudioStreamPropertyDirection as u32 => unsafe {
            // 1 = input, 0 = output
            let dir = if object_id == OBJECT_ID_STREAM_INPUT {
                1u32
            } else {
                0u32
            };
            write_u32(out, out_size, dir)
        },
        x if x == kAudioStreamPropertyTerminalType as u32 => unsafe {
            let term = if object_id == OBJECT_ID_STREAM_INPUT {
                kAudioStreamTerminalTypeMicrophone as u32
            } else {
                kAudioStreamTerminalTypeLine as u32
            };
            write_u32(out, out_size, term)
        },
        x if x == kAudioStreamPropertyStartingChannel as u32 => unsafe {
            write_u32(out, out_size, 1)
        },
        x if x == kAudioStreamPropertyLatency as u32 => unsafe { write_u32(out, out_size, 0) },
        x if x == kAudioStreamPropertyVirtualFormat as u32
            || x == kAudioStreamPropertyPhysicalFormat as u32 =>
        unsafe {
            let desc = stream_description();
            std::ptr::copy_nonoverlapping(
                &desc as *const AudioStreamBasicDescription,
                out as *mut AudioStreamBasicDescription,
                1,
            );
            *out_size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
            kAudioHardwareNoError as OSStatus
        },
        x if x == kAudioStreamPropertyAvailableVirtualFormats as u32
            || x == kAudioStreamPropertyAvailablePhysicalFormats as u32 =>
        unsafe {
            let ranged = AudioStreamRangedDescription {
                format: stream_description(),
                sample_rate_range: AudioValueRange {
                    mMinimum: SAMPLE_RATE,
                    mMaximum: SAMPLE_RATE,
                },
            };
            std::ptr::copy_nonoverlapping(
                &ranged as *const AudioStreamRangedDescription,
                out as *mut AudioStreamRangedDescription,
                1,
            );
            *out_size = std::mem::size_of::<AudioStreamRangedDescription>() as u32;
            kAudioHardwareNoError as OSStatus
        },
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

pub unsafe extern "C" fn set_property_data(
    _driver: DriverRef,
    _object_id: AudioObjectID,
    _client_pid: i32,
    _address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: u32,
    _qualifier_data: *const c_void,
    _data_size: u32,
    _data: *const c_void,
) -> OSStatus {
    UNSUPPORTED
}

// ---------------------------------------------------------------------------
// IO operations
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn start_io(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_id: u32,
) -> OSStatus {
    let prev = IO_RUNNING_COUNT.fetch_add(1, Ordering::SeqCst);
    if prev == 0 {
        // Compute host ticks per audio frame from mach timebase.
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut info) };

        // ticks_per_second = 1e9 * denom / numer (mach ticks are in nanos * numer/denom)
        // ticks_per_frame = ticks_per_second / sample_rate
        let ticks_per_frame =
            (info.denom as f64 / info.numer as f64) * (1_000_000_000.0 / SAMPLE_RATE);
        HOST_TICKS_PER_FRAME.store(ticks_per_frame.to_bits(), Ordering::SeqCst);

        TIMESTAMP_COUNT.store(0, Ordering::SeqCst);
        ANCHOR_HOST_TIME.store(unsafe { mach_absolute_time() }, Ordering::SeqCst);
    }
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn stop_io(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_id: u32,
) -> OSStatus {
    let _ = IO_RUNNING_COUNT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        if n > 0 { Some(n - 1) } else { None }
    });
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn get_zero_time_stamp(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_id: u32,
    out_sample_time: *mut f64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    let tpf = f64::from_bits(HOST_TICKS_PER_FRAME.load(Ordering::SeqCst));
    let ticks_per_ring = tpf * RING_BUFFER_FRAMES as f64;

    let anchor = ANCHOR_HOST_TIME.load(Ordering::SeqCst);
    let mut ts_count = TIMESTAMP_COUNT.load(Ordering::Relaxed);

    let now = unsafe { mach_absolute_time() };
    loop {
        let next_ts_time = anchor as f64 + (ts_count as f64 + 1.0) * ticks_per_ring;
        if (now as f64) < next_ts_time {
            break;
        }
        match TIMESTAMP_COUNT.compare_exchange_weak(
            ts_count,
            ts_count + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                ts_count += 1;
                break;
            }
            Err(actual) => ts_count = actual,
        }
    }

    unsafe {
        *out_sample_time = (ts_count as f64) * RING_BUFFER_FRAMES as f64;
        *out_host_time = anchor + (ts_count as f64 * ticks_per_ring) as u64;
        *out_seed = 1;
    }
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn will_do_io_operation(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_id: u32,
    operation_id: u32,
    out_will_do: *mut u8,
    out_will_do_in_place: *mut u8,
) -> OSStatus {
    match operation_id {
        x if x == kAudioServerPlugInIOOperationReadInput as u32
            || x == kAudioServerPlugInIOOperationWriteMix as u32 =>
        unsafe {
            *out_will_do = 1;
            *out_will_do_in_place = 1;
        },
        _ => unsafe {
            *out_will_do = 0;
            *out_will_do_in_place = 1;
        },
    }
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn begin_io_operation(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_id: u32,
    _operation_id: u32,
    _io_buffer_frame_size: u32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn do_io_operation(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _stream_id: AudioObjectID,
    _client_id: u32,
    operation_id: u32,
    io_buffer_frame_size: u32,
    io_cycle_info: *const AudioServerPlugInIOCycleInfo,
    main_buffer: *mut c_void,
    _secondary_buffer: *mut c_void,
) -> OSStatus {
    let frame_count = io_buffer_frame_size as usize;
    let sample_count = frame_count * NUM_CHANNELS;
    let buf = main_buffer as *mut f32;

    match operation_id {
        x if x == kAudioServerPlugInIOOperationWriteMix as u32 => {
            // Copy app's output into the ring buffer.
            let sample_time = unsafe { (*io_cycle_info).mOutputTime.mSampleTime } as usize;
            let offset = (sample_time % RING_BUFFER_FRAMES) * NUM_CHANNELS;

            for i in 0..sample_count {
                let ring_idx = (offset + i) % (RING_BUFFER_FRAMES * NUM_CHANNELS);
                let sample = unsafe { *buf.add(i) };
                RING_BUFFER[ring_idx].store(sample.to_bits(), Ordering::Relaxed);
            }
        }
        x if x == kAudioServerPlugInIOOperationReadInput as u32 => {
            // Copy from ring buffer into app's input buffer.
            let sample_time = unsafe { (*io_cycle_info).mInputTime.mSampleTime } as usize;
            let offset = (sample_time % RING_BUFFER_FRAMES) * NUM_CHANNELS;

            for i in 0..sample_count {
                let ring_idx = (offset + i) % (RING_BUFFER_FRAMES * NUM_CHANNELS);
                let bits = RING_BUFFER[ring_idx].load(Ordering::Relaxed);
                unsafe { *buf.add(i) = f32::from_bits(bits) };
            }
        }
        _ => {}
    }

    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn end_io_operation(
    _driver: DriverRef,
    _device_id: AudioObjectID,
    _client_id: u32,
    _operation_id: u32,
    _io_buffer_frame_size: u32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}
