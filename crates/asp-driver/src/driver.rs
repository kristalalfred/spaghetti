use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use coreaudio_sys::{
    AudioObjectID, AudioObjectPropertyAddress, AudioServerPlugInClientInfo,
    AudioServerPlugInHostInterface, AudioServerPlugInIOCycleInfo, AudioStreamBasicDescription,
    AudioValueRange, CFDictionaryRef, CFStringCompare, CFStringRef, CFUUIDBytes,
    OSStatus, kAudioBooleanControlClassID, kAudioBooleanControlPropertyValue,
    kAudioControlPropertyElement, kAudioControlPropertyScope, kAudioDeviceClassID,
    kAudioDevicePropertyAvailableNominalSampleRates, kAudioDevicePropertyClockDomain,
    kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceIsRunning,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyIcon, kAudioDevicePropertyLatency,
    kAudioDevicePropertyModelUID, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyPreferredChannelsForStereo, kAudioDevicePropertyRelatedDevices,
    kAudioDevicePropertySafetyOffset, kAudioDevicePropertyStreams,
    kAudioDevicePropertyTransportType, kAudioDevicePropertyZeroTimeStampPeriod,
    kAudioDeviceTransportTypeVirtual, kAudioHardwareBadObjectError, kAudioHardwareBadPropertySizeError, kAudioHardwareNoError,
    kAudioHardwareUnknownPropertyError, kAudioHardwareUnspecifiedError, kAudioLevelControlClassID,
    kAudioLevelControlPropertyConvertDecibelsToScalar,
    kAudioLevelControlPropertyConvertScalarToDecibels, kAudioLevelControlPropertyDecibelRange,
    kAudioLevelControlPropertyDecibelValue, kAudioLevelControlPropertyScalarValue,
    kAudioMuteControlClassID, kAudioObjectClassID, kAudioObjectPropertyBaseClass,
    kAudioObjectPropertyClass, kAudioObjectPropertyElementMain, kAudioObjectPropertyManufacturer,
    kAudioObjectPropertyName, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyOwner,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioPlugInClassID, kAudioPlugInPropertyDeviceList,
    kAudioPlugInPropertyResourceBundle, kAudioPlugInPropertyTranslateUIDToDevice,
    kAudioServerPlugInIOOperationReadInput, kAudioServerPlugInIOOperationWriteMix,
    kAudioStreamClassID, kAudioStreamPropertyAvailablePhysicalFormats,
    kAudioStreamPropertyAvailableVirtualFormats, kAudioStreamPropertyDirection,
    kAudioStreamPropertyIsActive, kAudioStreamPropertyLatency,
    kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyStartingChannel,
    kAudioStreamPropertyTerminalType, kAudioStreamPropertyVirtualFormat,
    kAudioStreamTerminalTypeLine, kAudioStreamTerminalTypeMicrophone, kAudioVolumeControlClassID,
    CFStringCreateWithCString, kCFAllocatorDefault, kCFStringEncodingUTF8,
};

use crate::devices::{DeviceInstance, DeviceRole};
use crate::iface::{DRIVER_INTERFACE_UUID, DRIVER_REF, DriverRef};
use crate::log;
use crate::registry::{self, OBJECT_ID_PLUGIN};
use spaghetti_common::activity::{ACTIVITY_SHM_NAME, ActivityRegion};

// ---------------------------------------------------------------------------
// Constants (scoped to this module)
// ---------------------------------------------------------------------------

/// Minimum dB floor for scalar↔dB conversion. `20*log10(1e-5) ≈ -100 dB`, which
/// is below our exposed range of -96..0.
const DB_MIN_SCALAR: f32 = 1e-5;

// kAudioDevicePropertyControlList ('ctrl') — not exported by coreaudio-sys
const K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST: u32 = 0x6374_726c;
// kAudioDevicePropertyIsHidden ('hidn') — not exported by coreaudio-sys
const K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN: u32 = 0x6869_646e;

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
// Plugin-wide state
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

/// Host ticks per second of wall time, derived from `mach_timebase_info`.
/// Stored as `f64::to_bits`. Plugin-wide because the mach timebase is
/// system-global; per-device ticks-per-frame is derived from this and the
/// device's sample rate in `start_io`.
static HOST_TICKS_PER_SECOND: AtomicU64 = AtomicU64::new(0);

/// Pointer to the mmap-ed activity region shared with the daemon. Null until
/// `initialize` sets it up; access is guarded by a null check.
///
/// NOTE: the activity shm is single-region in this phase — only the default
/// device writes to it. Multi-device activity reporting is a follow-up.
static ACTIVITY_PTR: AtomicPtr<ActivityRegion> = AtomicPtr::new(std::ptr::null_mut());

const UNSUPPORTED: OSStatus = 0x756E_6F70u32 as OSStatus; // 'unop' = kAudioHardwareUnsupportedOperationError

use crate::uuid_eq;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a CFStringRef from a Rust string. Returns null on allocation failure.
/// The caller owns the returned +1 reference.
fn cfstr(s: &str) -> CFStringRef {
    let cstr = std::ffi::CString::new(s).unwrap_or_else(|_| std::ffi::CString::new("").unwrap());
    unsafe { CFStringCreateWithCString(kCFAllocatorDefault, cstr.as_ptr(), kCFStringEncodingUTF8) }
}

/// Convert a linear [0, 1] scalar to dB. Floors below `DB_MIN_SCALAR` to keep
/// the range finite (log10(0) = -inf).
fn scalar_to_db(scalar: f32) -> f32 {
    20.0 * scalar.max(DB_MIN_SCALAR).log10()
}

/// Inverse of `scalar_to_db`.
fn db_to_scalar(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

// ---------------------------------------------------------------------------
// PropertyWriter
// ---------------------------------------------------------------------------

/// Safe wrapper around the HAL's `(out_data, out_data_size)` output pair.
///
/// Constructed once at the `unsafe extern "C"` boundary in
/// `get_property_data`, then passed by `&mut` reference to the per-object
/// property getters, which are all safe `fn`.
///
/// Claude: writes never exceed `capacity` (the caller's `inDataSize`). The
/// device list can grow between the HAL's size query and its data query, so
/// lists are truncated to fit and fixed-size values that don't fit fail.
struct PropertyWriter {
    out: *mut c_void,
    out_size: *mut u32,
    capacity: u32,
}

impl PropertyWriter {
    /// # Safety
    /// `out` must be writable for `capacity` bytes and `out_size` must be a
    /// valid `*mut u32`.
    unsafe fn new(out: *mut c_void, out_size: *mut u32, capacity: u32) -> Self {
        Self {
            out,
            out_size,
            capacity,
        }
    }

    fn write_value<T: Copy>(&mut self, val: T) -> OSStatus {
        let size = std::mem::size_of::<T>() as u32;
        if self.capacity < size {
            return kAudioHardwareBadPropertySizeError as OSStatus;
        }
        unsafe {
            (self.out as *mut T).write_unaligned(val);
            *self.out_size = size;
        }
        kAudioHardwareNoError as OSStatus
    }

    fn write_u32(&mut self, val: u32) -> OSStatus {
        self.write_value(val)
    }

    fn write_f64(&mut self, val: f64) -> OSStatus {
        self.write_value(val)
    }

    fn write_f32(&mut self, val: f32) -> OSStatus {
        self.write_value(val)
    }

    /// Write a `CFStringRef` to the output buffer. Returns
    /// `kAudioHardwareUnspecifiedError` if `CFStringCreateWithCString` fails
    /// to allocate.
    fn write_cfstr(&mut self, s: &str) -> OSStatus {
        if (self.capacity as usize) < std::mem::size_of::<CFStringRef>() {
            return kAudioHardwareBadPropertySizeError as OSStatus;
        }
        let cf = cfstr(s);
        if cf.is_null() {
            return kAudioHardwareUnspecifiedError as OSStatus;
        }
        // The HAL takes ownership; CFStringCreateWithCString already returns +1.
        self.write_value(cf)
    }

    fn write_asbd(&mut self, desc: AudioStreamBasicDescription) -> OSStatus {
        self.write_value(desc)
    }

    fn write_value_range(&mut self, range: AudioValueRange) -> OSStatus {
        self.write_value(range)
    }

    fn write_ranged_description(&mut self, desc: AudioStreamRangedDescription) -> OSStatus {
        self.write_value(desc)
    }

    fn write_u32_slice(&mut self, vals: &[u32]) -> OSStatus {
        let count = vals.len().min(self.capacity as usize / std::mem::size_of::<u32>());
        unsafe {
            std::ptr::copy_nonoverlapping(vals.as_ptr(), self.out as *mut u32, count);
            *self.out_size = (count * std::mem::size_of::<u32>()) as u32;
        }
        kAudioHardwareNoError as OSStatus
    }

    /// Read a `f32` from the output buffer in-place (used by the scalar↔dB
    /// conversion properties, which read *and* write through the same pointer).
    fn read_f32(&self) -> Option<f32> {
        if (self.capacity as usize) < std::mem::size_of::<f32>() {
            return None;
        }
        Some(unsafe { (self.out as *const f32).read_unaligned() })
    }

    fn set_size_zero(&mut self) -> OSStatus {
        unsafe { *self.out_size = 0 };
        kAudioHardwareNoError as OSStatus
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

    // Save the host interface pointer for later use
    DRIVER.host.store(host as *mut _, Ordering::SeqCst);

    crate::watcher::start();

    // Map the shared-memory activity region. Failure is non-fatal — the driver
    // still services audio, the daemon just won't see activity.
    init_activity_region();

    log("initialize: returning noErr");
    0 // noErr
}

/// Open, size, and mmap the activity shared-memory region. Publishes the result
/// via `ACTIVITY_PTR`. All failure paths log and leave the pointer null so the
/// IO callback skips activity updates.
fn init_activity_region() {
    use std::ffi::CString;

    let name = match CString::new(ACTIVITY_SHM_NAME) {
        Ok(n) => n,
        Err(e) => {
            log(&format!("activity: bad shm name: {e}"));
            return;
        }
    };

    let size = std::mem::size_of::<ActivityRegion>();

    unsafe {
        // Always unlink any stale region from a previous load so we can
        // recreate it with the correct permissions.
        libc::shm_unlink(name.as_ptr());

        log(&format!(
            "activity: uid={} gid={}",
            libc::geteuid(),
            libc::getegid()
        ));

        let fd = libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o666 as libc::c_uint,
        );
        if fd < 0 {
            let err = *libc::__error();
            log(&format!("activity: shm_open failed: errno={err}"));
            return;
        }

        if libc::fchmod(fd, 0o666) != 0 {
            let err = *libc::__error();
            log(&format!("activity: fchmod failed (non-fatal): errno={err}"));
        }

        if libc::ftruncate(fd, size as libc::off_t) != 0 {
            let err = *libc::__error();
            log(&format!("activity: ftruncate failed: errno={err}"));
            libc::close(fd);
            return;
        }

        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        libc::close(fd);

        if ptr == libc::MAP_FAILED {
            let err = *libc::__error();
            log(&format!("activity: mmap failed: errno={err}"));
            return;
        }

        std::ptr::write(ptr as *mut ActivityRegion, ActivityRegion::new());
        ACTIVITY_PTR.store(ptr as *mut ActivityRegion, Ordering::Release);
        log(&format!("activity: mapped region at {ptr:p}"));
    }
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

/// Selectors valid on the plugin object.
fn plugin_selector_known(selector: u32) -> bool {
    matches!(
        selector,
        x if x == kAudioObjectPropertyBaseClass
            || x == kAudioObjectPropertyClass
            || x == kAudioObjectPropertyOwner
            || x == kAudioObjectPropertyOwnedObjects
            || x == kAudioObjectPropertyManufacturer
            || x == kAudioPlugInPropertyDeviceList
            || x == kAudioPlugInPropertyTranslateUIDToDevice
            || x == kAudioPlugInPropertyResourceBundle
    )
}

/// Selectors valid on a device object.
fn device_selector_known(selector: u32) -> bool {
    matches!(
        selector,
        x if x == kAudioObjectPropertyBaseClass
            || x == kAudioObjectPropertyClass
            || x == kAudioObjectPropertyOwner
            || x == kAudioObjectPropertyName
            || x == kAudioObjectPropertyManufacturer
            || x == kAudioDevicePropertyDeviceUID
            || x == kAudioDevicePropertyModelUID
            || x == kAudioDevicePropertyTransportType
            || x == kAudioDevicePropertyRelatedDevices
            || x == kAudioDevicePropertyClockDomain
            || x == kAudioDevicePropertyDeviceIsAlive
            || x == kAudioDevicePropertyDeviceIsRunning
            || x == kAudioDevicePropertyDeviceCanBeDefaultDevice
            || x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice
            || x == kAudioDevicePropertyLatency
            || x == kAudioDevicePropertyStreams
            || x == kAudioObjectPropertyOwnedObjects
            || x == kAudioDevicePropertyNominalSampleRate
            || x == kAudioDevicePropertyAvailableNominalSampleRates
            || x == kAudioDevicePropertyPreferredChannelsForStereo
            || x == kAudioDevicePropertyZeroTimeStampPeriod
            || x == kAudioDevicePropertySafetyOffset
            || x == K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST
            || x == K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN
    )
}

/// Selectors valid on a stream object.
fn stream_selector_known(selector: u32) -> bool {
    matches!(
        selector,
        x if x == kAudioObjectPropertyBaseClass
            || x == kAudioObjectPropertyClass
            || x == kAudioObjectPropertyOwner
            || x == kAudioStreamPropertyIsActive
            || x == kAudioStreamPropertyDirection
            || x == kAudioStreamPropertyTerminalType
            || x == kAudioStreamPropertyStartingChannel
            || x == kAudioStreamPropertyLatency
            || x == kAudioStreamPropertyVirtualFormat
            || x == kAudioStreamPropertyPhysicalFormat
            || x == kAudioStreamPropertyAvailableVirtualFormats
            || x == kAudioStreamPropertyAvailablePhysicalFormats
    )
}

/// Selectors valid on a volume control.
fn volume_selector_known(selector: u32) -> bool {
    matches!(
        selector,
        x if x == kAudioObjectPropertyBaseClass
            || x == kAudioObjectPropertyClass
            || x == kAudioObjectPropertyOwner
            || x == kAudioObjectPropertyName
            || x == kAudioObjectPropertyOwnedObjects
            || x == kAudioControlPropertyScope
            || x == kAudioLevelControlPropertyScalarValue
            || x == kAudioLevelControlPropertyDecibelValue
            || x == kAudioLevelControlPropertyDecibelRange
            || x == kAudioLevelControlPropertyConvertScalarToDecibels
            || x == kAudioLevelControlPropertyConvertDecibelsToScalar
    ) || selector == kAudioControlPropertyElement
}

/// Selectors valid on a mute control.
fn mute_selector_known(selector: u32) -> bool {
    matches!(
        selector,
        x if x == kAudioObjectPropertyBaseClass
            || x == kAudioObjectPropertyClass
            || x == kAudioObjectPropertyOwner
            || x == kAudioObjectPropertyName
            || x == kAudioObjectPropertyOwnedObjects
            || x == kAudioControlPropertyScope
            || x == kAudioBooleanControlPropertyValue
    ) || selector == kAudioControlPropertyElement
}

/// Returns true if we recognize this property selector for the given object.
/// Resolves the object ID against the registry; plugin-scoped selectors are
/// handled separately.
fn property_known(object_id: u32, selector: u32, _scope: u32) -> bool {
    if object_id == OBJECT_ID_PLUGIN {
        return plugin_selector_known(selector);
    }
    let reg = registry::current();
    match reg.resolve(object_id).map(|(_, role)| role) {
        Some(DeviceRole::Device) => device_selector_known(selector),
        Some(DeviceRole::StreamInput) | Some(DeviceRole::StreamOutput) => {
            stream_selector_known(selector)
        }
        Some(DeviceRole::Volume) => volume_selector_known(selector),
        Some(DeviceRole::Mute) => mute_selector_known(selector),
        None => {
            registry::warn_unknown_object(object_id, "property_known");
            false
        }
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

    let settable = if object_id == OBJECT_ID_PLUGIN {
        0
    } else {
        let reg = registry::current();
        match reg.resolve(object_id).map(|(_, role)| role) {
            Some(DeviceRole::Volume)
                if addr.mSelector == kAudioLevelControlPropertyScalarValue
                    || addr.mSelector == kAudioLevelControlPropertyDecibelValue =>
            {
                1
            }
            Some(DeviceRole::Mute)
                if addr.mSelector == kAudioBooleanControlPropertyValue =>
            {
                1
            }
            _ => 0,
        }
    };
    unsafe { *out_settable = settable };
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

    let size: u32 = if object_id == OBJECT_ID_PLUGIN {
        plugin_data_size(sel)
    } else {
        let reg = registry::current();
        match reg.resolve(object_id) {
            Some((instance, DeviceRole::Device)) => device_data_size(sel, scope, instance),
            Some((_, DeviceRole::StreamInput)) | Some((_, DeviceRole::StreamOutput)) => {
                stream_data_size(sel)
            }
            Some((_, DeviceRole::Volume)) => volume_control_data_size(sel),
            Some((_, DeviceRole::Mute)) => mute_control_data_size(sel),
            None => {
                registry::warn_unknown_object(object_id, "get_property_data_size");
                return kAudioHardwareBadObjectError as OSStatus;
            }
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
    unsafe { *out_data_size = actual_size };
    kAudioHardwareNoError as OSStatus
}

fn plugin_data_size(sel: u32) -> u32 {
    let device_count = registry::current().by_id.len() as u32;
    let device_ids_bytes = device_count.saturating_mul(4);
    match sel {
        x if x == kAudioObjectPropertyBaseClass => 4,
        x if x == kAudioObjectPropertyClass => 4,
        x if x == kAudioObjectPropertyOwner => 4,
        x if x == kAudioObjectPropertyOwnedObjects => device_ids_bytes.max(1),
        x if x == kAudioObjectPropertyManufacturer => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        x if x == kAudioPlugInPropertyDeviceList => device_ids_bytes.max(1),
        x if x == kAudioPlugInPropertyTranslateUIDToDevice => 4,
        x if x == kAudioPlugInPropertyResourceBundle => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        _ => 0,
    }
}

fn device_data_size(sel: u32, scope: u32, _instance: &DeviceInstance) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass => 4,
        x if x == kAudioObjectPropertyClass => 4,
        x if x == kAudioObjectPropertyOwner => 4,
        x if x == kAudioObjectPropertyName => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioObjectPropertyManufacturer => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        x if x == kAudioDevicePropertyDeviceUID => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioDevicePropertyModelUID => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioDevicePropertyTransportType => 4,
        x if x == kAudioDevicePropertyRelatedDevices => 4,
        x if x == kAudioDevicePropertyClockDomain => 4,
        x if x == kAudioDevicePropertyDeviceIsAlive => 4,
        x if x == kAudioDevicePropertyDeviceIsRunning => 4,
        x if x == kAudioDevicePropertyDeviceCanBeDefaultDevice => 4,
        x if x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice => 4,
        x if x == kAudioDevicePropertyLatency => 4,
        x if x == kAudioDevicePropertyStreams => {
            if scope == kAudioObjectPropertyScopeGlobal {
                8 // two stream IDs
            } else {
                4 // one stream ID
            }
        }
        x if x == kAudioObjectPropertyOwnedObjects => 16, // two streams + two controls
        x if x == kAudioDevicePropertyNominalSampleRate => 8,
        x if x == kAudioDevicePropertyAvailableNominalSampleRates => {
            std::mem::size_of::<AudioValueRange>() as u32
        }
        x if x == kAudioDevicePropertyPreferredChannelsForStereo => 8,
        x if x == kAudioDevicePropertyZeroTimeStampPeriod => 4,
        x if x == kAudioDevicePropertySafetyOffset => 4,
        x if x == kAudioDevicePropertyIcon => 0, // we don't support this
        x if x == K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN => 4,
        // Two controls: volume + mute.
        x if x == K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST => 8,
        _ => 0,
    }
}

fn stream_data_size(sel: u32) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass => 4,
        x if x == kAudioObjectPropertyClass => 4,
        x if x == kAudioObjectPropertyOwner => 4,
        x if x == kAudioStreamPropertyIsActive => 4,
        x if x == kAudioStreamPropertyDirection => 4,
        x if x == kAudioStreamPropertyTerminalType => 4,
        x if x == kAudioStreamPropertyStartingChannel => 4,
        x if x == kAudioStreamPropertyLatency => 4,
        x if x == kAudioStreamPropertyVirtualFormat
            || x == kAudioStreamPropertyPhysicalFormat =>
        {
            std::mem::size_of::<AudioStreamBasicDescription>() as u32
        }
        x if x == kAudioStreamPropertyAvailableVirtualFormats
            || x == kAudioStreamPropertyAvailablePhysicalFormats =>
        {
            std::mem::size_of::<AudioStreamRangedDescription>() as u32
        }
        _ => 0,
    }
}

fn volume_control_data_size(sel: u32) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass => 4,
        x if x == kAudioObjectPropertyClass => 4,
        x if x == kAudioObjectPropertyOwner => 4,
        x if x == kAudioObjectPropertyName => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioObjectPropertyOwnedObjects => u32::MAX, // empty
        x if x == kAudioControlPropertyScope => 4,
        x if x == kAudioControlPropertyElement => 4,
        x if x == kAudioLevelControlPropertyScalarValue => 4,
        x if x == kAudioLevelControlPropertyDecibelValue => 4,
        x if x == kAudioLevelControlPropertyDecibelRange => {
            std::mem::size_of::<AudioValueRange>() as u32
        }
        x if x == kAudioLevelControlPropertyConvertScalarToDecibels => 4,
        x if x == kAudioLevelControlPropertyConvertDecibelsToScalar => 4,
        _ => 0,
    }
}

fn mute_control_data_size(sel: u32) -> u32 {
    match sel {
        x if x == kAudioObjectPropertyBaseClass => 4,
        x if x == kAudioObjectPropertyClass => 4,
        x if x == kAudioObjectPropertyOwner => 4,
        x if x == kAudioObjectPropertyName => std::mem::size_of::<CFStringRef>() as u32,
        x if x == kAudioObjectPropertyOwnedObjects => u32::MAX, // empty
        x if x == kAudioControlPropertyScope => 4,
        x if x == kAudioControlPropertyElement => 4,
        x if x == kAudioBooleanControlPropertyValue => 4,
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
    data_size: u32,
    out_data_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let addr = unsafe { &*address };
    let sel = addr.mSelector;
    let scope = addr.mScope;

    // SAFETY: out_data is a HAL-owned buffer of data_size bytes and
    // out_data_size is a valid HAL-owned u32.
    let mut pw = unsafe { PropertyWriter::new(out_data, out_data_size, data_size) };

    if object_id == OBJECT_ID_PLUGIN {
        get_plugin_property(sel, qualifier_data_size, qualifier_data, &mut pw)
    } else {
        let reg = registry::current();
        match reg.resolve(object_id) {
            Some((instance, DeviceRole::Device)) => {
                get_device_property(sel, scope, instance, &mut pw)
            }
            Some((instance, DeviceRole::StreamInput)) => {
                get_stream_property(sel, DeviceRole::StreamInput, instance, &mut pw)
            }
            Some((instance, DeviceRole::StreamOutput)) => {
                get_stream_property(sel, DeviceRole::StreamOutput, instance, &mut pw)
            }
            Some((instance, DeviceRole::Volume)) => {
                get_volume_control_property(sel, instance, &mut pw)
            }
            Some((instance, DeviceRole::Mute)) => {
                get_mute_control_property(sel, instance, &mut pw)
            }
            None => {
                registry::warn_unknown_object(object_id, "get_property_data");
                kAudioHardwareBadObjectError as OSStatus
            }
        }
    }
}

fn get_plugin_property(
    sel: u32,
    qualifier_size: u32,
    qualifier_data: *const c_void,
    pw: &mut PropertyWriter,
) -> OSStatus {
    match sel {
        x if x == kAudioObjectPropertyBaseClass => {
            pw.write_u32(kAudioObjectClassID)
        }
        x if x == kAudioObjectPropertyClass => pw.write_u32(kAudioPlugInClassID),
        x if x == kAudioObjectPropertyOwner => pw.write_u32(OBJECT_ID_PLUGIN),
        x if x == kAudioObjectPropertyOwnedObjects
            || x == kAudioPlugInPropertyDeviceList =>
        {
            let reg = registry::current();
            let ids: Vec<u32> = reg.devices().map(|d| d.device_id).collect();
            if ids.is_empty() {
                pw.set_size_zero()
            } else {
                pw.write_u32_slice(&ids)
            }
        }
        x if x == kAudioObjectPropertyManufacturer => pw.write_cfstr("Spaghetti"),
        x if x == kAudioPlugInPropertyTranslateUIDToDevice => {
            translate_uid_to_device(qualifier_size, qualifier_data, pw)
        }
        x if x == kAudioPlugInPropertyResourceBundle => pw.write_cfstr(""),
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

/// Resolve the UID string in the qualifier data to a device ID. Writes the
/// matched device's object ID to `out`, or `0` if no match (the documented
/// "not found" sentinel for this property).
fn translate_uid_to_device(
    qualifier_size: u32,
    qualifier_data: *const c_void,
    pw: &mut PropertyWriter,
) -> OSStatus {
    if qualifier_size < std::mem::size_of::<CFStringRef>() as u32 || qualifier_data.is_null() {
        return pw.write_u32(0);
    }
    // qualifier_data points at a `CFStringRef` value (coreaudio_sys type).
    let cf_uid = unsafe { *(qualifier_data as *const CFStringRef) };
    if cf_uid.is_null() {
        return pw.write_u32(0);
    }

    let reg = registry::current();
    let mut matched: u32 = 0;
    for (uid, device_id) in &reg.by_uid {
        let candidate = cfstr(uid);
        if candidate.is_null() {
            continue;
        }
        let cmp = unsafe { CFStringCompare(candidate, cf_uid, 0) };
        unsafe {
            coreaudio_sys::CFRelease(candidate as *const c_void);
        }
        if cmp == 0 {
            matched = *device_id;
            break;
        }
    }
    pw.write_u32(matched)
}

fn get_device_property(
    sel: u32,
    scope: u32,
    instance: &DeviceInstance,
    pw: &mut PropertyWriter,
) -> OSStatus {
    match sel {
        x if x == kAudioObjectPropertyBaseClass => {
            pw.write_u32(kAudioObjectClassID)
        }
        x if x == kAudioObjectPropertyClass => pw.write_u32(kAudioDeviceClassID),
        x if x == kAudioObjectPropertyOwner => pw.write_u32(OBJECT_ID_PLUGIN),
        x if x == kAudioObjectPropertyName => pw.write_cfstr(&instance.name),
        x if x == kAudioObjectPropertyManufacturer => pw.write_cfstr("Spaghetti"),
        x if x == kAudioDevicePropertyDeviceUID => pw.write_cfstr(&instance.uid),
        x if x == kAudioDevicePropertyModelUID => pw.write_cfstr("SpaghettiModel_UID"),
        x if x == kAudioDevicePropertyTransportType => {
            pw.write_u32(kAudioDeviceTransportTypeVirtual)
        }
        x if x == kAudioDevicePropertyRelatedDevices => {
            pw.write_u32_slice(&[instance.device_id])
        }
        x if x == kAudioDevicePropertyClockDomain => pw.write_u32(0),
        x if x == kAudioDevicePropertyDeviceIsAlive => pw.write_u32(1),
        x if x == kAudioDevicePropertyDeviceIsRunning => {
            let running = u32::from(instance.io_running_count.load(Ordering::SeqCst) > 0);
            pw.write_u32(running)
        }
        x if x == kAudioDevicePropertyDeviceCanBeDefaultDevice => pw.write_u32(1),
        x if x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice => pw.write_u32(1),
        x if x == kAudioDevicePropertyLatency => pw.write_u32(0),
        x if x == kAudioDevicePropertyStreams => match scope {
            s if s == kAudioObjectPropertyScopeInput => {
                pw.write_u32_slice(&[instance.stream_in_id])
            }
            s if s == kAudioObjectPropertyScopeOutput => {
                pw.write_u32_slice(&[instance.stream_out_id])
            }
            _ => pw.write_u32_slice(&[instance.stream_in_id, instance.stream_out_id]),
        },
        x if x == kAudioObjectPropertyOwnedObjects => pw.write_u32_slice(&[
            instance.stream_in_id,
            instance.stream_out_id,
            instance.volume_id,
            instance.mute_id,
        ]),
        x if x == kAudioDevicePropertyNominalSampleRate => pw.write_f64(instance.sample_rate),
        x if x == kAudioDevicePropertyAvailableNominalSampleRates => pw.write_value_range(
            AudioValueRange {
                mMinimum: instance.sample_rate,
                mMaximum: instance.sample_rate,
            },
        ),
        x if x == kAudioDevicePropertyPreferredChannelsForStereo => {
            pw.write_u32_slice(&[1, 2])
        }
        x if x == kAudioDevicePropertyZeroTimeStampPeriod => {
            pw.write_u32(instance.ring_frames as u32)
        }
        x if x == kAudioDevicePropertySafetyOffset => pw.write_u32(0),
        x if x == kAudioDevicePropertyIcon => kAudioHardwareUnknownPropertyError as OSStatus,
        // Not hidden — coreaudiod queries this directly without checking has_property.
        x if x == K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN => pw.write_u32(0),
        // Volume + mute controls.
        x if x == K_AUDIO_DEVICE_PROPERTY_CONTROL_LIST => {
            pw.write_u32_slice(&[instance.volume_id, instance.mute_id])
        }
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

fn get_stream_property(
    sel: u32,
    role: DeviceRole,
    instance: &DeviceInstance,
    pw: &mut PropertyWriter,
) -> OSStatus {
    let is_input = matches!(role, DeviceRole::StreamInput);
    match sel {
        x if x == kAudioObjectPropertyBaseClass => pw.write_u32(kAudioObjectClassID),
        x if x == kAudioObjectPropertyClass => pw.write_u32(kAudioStreamClassID),
        x if x == kAudioObjectPropertyOwner => pw.write_u32(instance.device_id),
        x if x == kAudioStreamPropertyIsActive => pw.write_u32(1),
        x if x == kAudioStreamPropertyDirection => {
            // 1 = input, 0 = output
            pw.write_u32(if is_input { 1 } else { 0 })
        }
        x if x == kAudioStreamPropertyTerminalType => {
            let term = if is_input {
                kAudioStreamTerminalTypeMicrophone
            } else {
                kAudioStreamTerminalTypeLine
            };
            pw.write_u32(term)
        }
        x if x == kAudioStreamPropertyStartingChannel => pw.write_u32(1),
        x if x == kAudioStreamPropertyLatency => pw.write_u32(0),
        x if x == kAudioStreamPropertyVirtualFormat
            || x == kAudioStreamPropertyPhysicalFormat =>
        {
            pw.write_asbd(instance.asbd())
        }
        x if x == kAudioStreamPropertyAvailableVirtualFormats
            || x == kAudioStreamPropertyAvailablePhysicalFormats =>
        {
            pw.write_ranged_description(AudioStreamRangedDescription {
                format: instance.asbd(),
                sample_rate_range: AudioValueRange {
                    mMinimum: instance.sample_rate,
                    mMaximum: instance.sample_rate,
                },
            })
        }
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

fn get_volume_control_property(
    sel: u32,
    instance: &DeviceInstance,
    pw: &mut PropertyWriter,
) -> OSStatus {
    match sel {
        // Immediate parent class: kAudioVolumeControlClassID ⊂ kAudioLevelControlClassID.
        x if x == kAudioObjectPropertyBaseClass => {
            pw.write_u32(kAudioLevelControlClassID)
        }
        // System Settings filters on this exact class to show a volume slider.
        x if x == kAudioObjectPropertyClass => {
            pw.write_u32(kAudioVolumeControlClassID)
        }
        x if x == kAudioObjectPropertyOwner => pw.write_u32(instance.device_id),
        x if x == kAudioObjectPropertyName => {
            pw.write_cfstr(&format!("{} Output Volume", instance.name))
        }
        x if x == kAudioObjectPropertyOwnedObjects => pw.set_size_zero(),
        x if x == kAudioControlPropertyScope => {
            pw.write_u32(kAudioObjectPropertyScopeOutput)
        }
        x if x == kAudioControlPropertyElement => {
            pw.write_u32(kAudioObjectPropertyElementMain)
        }
        x if x == kAudioLevelControlPropertyScalarValue => {
            pw.write_f32(f32::from_bits(instance.volume.load(Ordering::Relaxed)))
        }
        x if x == kAudioLevelControlPropertyDecibelValue => {
            let scalar = f32::from_bits(instance.volume.load(Ordering::Relaxed));
            pw.write_f32(scalar_to_db(scalar))
        }
        x if x == kAudioLevelControlPropertyDecibelRange => pw.write_value_range(
            AudioValueRange {
                mMinimum: -96.0,
                mMaximum: 0.0,
            },
        ),
        // Input: scalar in *out, output: dB in *out (in-place conversion).
        x if x == kAudioLevelControlPropertyConvertScalarToDecibels => match pw.read_f32() {
            Some(scalar) => pw.write_f32(scalar_to_db(scalar.clamp(0.0, 1.0))),
            None => kAudioHardwareBadPropertySizeError as OSStatus,
        },
        // Input: dB in *out, output: scalar in *out.
        x if x == kAudioLevelControlPropertyConvertDecibelsToScalar => match pw.read_f32() {
            Some(db) => pw.write_f32(db_to_scalar(db).clamp(0.0, 1.0)),
            None => kAudioHardwareBadPropertySizeError as OSStatus,
        },
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

fn get_mute_control_property(
    sel: u32,
    instance: &DeviceInstance,
    pw: &mut PropertyWriter,
) -> OSStatus {
    match sel {
        // Immediate parent class: kAudioMuteControlClassID ⊂ kAudioBooleanControlClassID.
        x if x == kAudioObjectPropertyBaseClass => {
            pw.write_u32(kAudioBooleanControlClassID)
        }
        // System Settings filters on this exact class to show a mute toggle.
        x if x == kAudioObjectPropertyClass => pw.write_u32(kAudioMuteControlClassID),
        x if x == kAudioObjectPropertyOwner => pw.write_u32(instance.device_id),
        x if x == kAudioObjectPropertyName => {
            pw.write_cfstr(&format!("{} Output Mute", instance.name))
        }
        x if x == kAudioObjectPropertyOwnedObjects => pw.set_size_zero(),
        x if x == kAudioControlPropertyScope => {
            pw.write_u32(kAudioObjectPropertyScopeOutput)
        }
        x if x == kAudioControlPropertyElement => {
            pw.write_u32(kAudioObjectPropertyElementMain)
        }
        x if x == kAudioBooleanControlPropertyValue => {
            pw.write_u32(instance.mute.load(Ordering::Relaxed))
        }
        _ => kAudioHardwareUnknownPropertyError as OSStatus,
    }
}

pub unsafe extern "C" fn set_property_data(
    _driver: DriverRef,
    object_id: AudioObjectID,
    _client_pid: i32,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: u32,
    _qualifier_data: *const c_void,
    data_size: u32,
    data: *const c_void,
) -> OSStatus {
    let addr = unsafe { &*address };
    let sel = addr.mSelector;

    if object_id == OBJECT_ID_PLUGIN {
        return UNSUPPORTED;
    }

    let reg = registry::current();
    match reg.resolve(object_id) {
        Some((instance, DeviceRole::Volume)) => unsafe {
            set_volume_property(sel, data_size, data, instance, object_id)
        },
        Some((instance, DeviceRole::Mute)) => unsafe {
            set_mute_property(sel, data_size, data, instance, object_id)
        },
        Some(_) => UNSUPPORTED,
        None => {
            registry::warn_unknown_object(object_id, "set_property_data");
            UNSUPPORTED
        }
    }
}

unsafe fn set_volume_property(
    sel: u32,
    data_size: u32,
    data: *const c_void,
    instance: &DeviceInstance,
    control_id: u32,
) -> OSStatus {
    match sel {
        x if x == kAudioLevelControlPropertyScalarValue => {
            if data_size < 4 {
                return kAudioHardwareBadObjectError as OSStatus;
            }
            let new_scalar = unsafe { *(data as *const f32) }.clamp(0.0, 1.0);
            instance
                .volume
                .store(new_scalar.to_bits(), Ordering::Relaxed);
            notify_host_property_changed(control_id, kAudioLevelControlPropertyScalarValue);
            notify_host_property_changed(control_id, kAudioLevelControlPropertyDecibelValue);
            kAudioHardwareNoError as OSStatus
        }
        x if x == kAudioLevelControlPropertyDecibelValue => {
            if data_size < 4 {
                return kAudioHardwareBadObjectError as OSStatus;
            }
            let db = unsafe { *(data as *const f32) }.clamp(-96.0, 0.0);
            let scalar = db_to_scalar(db).clamp(0.0, 1.0);
            instance.volume.store(scalar.to_bits(), Ordering::Relaxed);
            notify_host_property_changed(control_id, kAudioLevelControlPropertyScalarValue);
            notify_host_property_changed(control_id, kAudioLevelControlPropertyDecibelValue);
            kAudioHardwareNoError as OSStatus
        }
        _ => UNSUPPORTED,
    }
}

unsafe fn set_mute_property(
    sel: u32,
    data_size: u32,
    data: *const c_void,
    instance: &DeviceInstance,
    control_id: u32,
) -> OSStatus {
    match sel {
        x if x == kAudioBooleanControlPropertyValue => {
            if data_size < 4 {
                return kAudioHardwareBadObjectError as OSStatus;
            }
            let v = unsafe { *(data as *const u32) };
            instance
                .mute
                .store(if v != 0 { 1 } else { 0 }, Ordering::Relaxed);
            notify_host_property_changed(control_id, kAudioBooleanControlPropertyValue);
            kAudioHardwareNoError as OSStatus
        }
        _ => UNSUPPORTED,
    }
}

/// Notify the host that the plugin's device list (and owned-objects list)
/// changed. Called by the config-file watcher after a registry swap; Core
/// Audio responds by re-querying `kAudioPlugInPropertyDeviceList`.
pub(crate) fn notify_device_list_changed() {
    notify_host_property_changed(OBJECT_ID_PLUGIN, kAudioPlugInPropertyDeviceList);
    notify_host_property_changed(OBJECT_ID_PLUGIN, kAudioObjectPropertyOwnedObjects);
}

/// Notify the HAL host that a property on an object changed so clients re-read
/// it. Safe to call from non-realtime property handlers; never call from the
/// IO callback.
fn notify_host_property_changed(object_id: u32, selector: u32) {
    let host_ptr = DRIVER.host.load(Ordering::SeqCst);
    if host_ptr.is_null() {
        return;
    }
    unsafe {
        let host = &*host_ptr;
        if let Some(props_changed) = host.PropertiesChanged {
            let addr = AudioObjectPropertyAddress {
                mSelector: selector,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let _ = props_changed(host_ptr, object_id, 1, &addr);
        }
    }
}

// ---------------------------------------------------------------------------
// IO operations
// ---------------------------------------------------------------------------

/// Mirror the default-device IO-running state into the activity region. The
/// activity shm is single-region for now (see ACTIVITY_PTR comment); multi-
/// device reporting is follow-up work.
fn publish_io_running() {
    let ptr = ACTIVITY_PTR.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    let reg = registry::current();
    let running = reg
        .default_device()
        .map(|d| u32::from(d.io_running_count.load(Ordering::SeqCst) > 0))
        .unwrap_or(0);
    unsafe { (*ptr).io_running.store(running, Ordering::Relaxed) };
}

pub unsafe extern "C" fn start_io(
    _driver: DriverRef,
    device_id: AudioObjectID,
    _client_id: u32,
) -> OSStatus {
    let reg = registry::current();
    let Some((instance, DeviceRole::Device)) = reg.resolve(device_id) else {
        registry::warn_unknown_object(device_id, "start_io");
        return kAudioHardwareBadObjectError as OSStatus;
    };

    let prev = instance.io_running_count.fetch_add(1, Ordering::SeqCst);
    if prev == 0 {
        // Plugin-wide: ticks-per-second from mach timebase. Computed once.
        let mut tps_bits = HOST_TICKS_PER_SECOND.load(Ordering::SeqCst);
        if tps_bits == 0 {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            unsafe { mach_timebase_info(&mut info) };
            let ticks_per_second = (info.denom as f64 / info.numer as f64) * 1_000_000_000.0;
            tps_bits = ticks_per_second.to_bits();
            HOST_TICKS_PER_SECOND.store(tps_bits, Ordering::SeqCst);
        }
        let ticks_per_second = f64::from_bits(tps_bits);
        let ticks_per_frame = ticks_per_second / instance.sample_rate;
        instance
            .host_ticks_per_frame
            .store(ticks_per_frame.to_bits(), Ordering::SeqCst);

        instance.timestamp_count.store(0, Ordering::SeqCst);
        instance
            .anchor_host_time
            .store(unsafe { mach_absolute_time() }, Ordering::SeqCst);
    }
    publish_io_running();
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn stop_io(
    _driver: DriverRef,
    device_id: AudioObjectID,
    _client_id: u32,
) -> OSStatus {
    let reg = registry::current();
    let Some((instance, DeviceRole::Device)) = reg.resolve(device_id) else {
        registry::warn_unknown_object(device_id, "stop_io");
        return kAudioHardwareBadObjectError as OSStatus;
    };

    let _ = instance
        .io_running_count
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            if n > 0 { Some(n - 1) } else { None }
        });
    publish_io_running();
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn get_zero_time_stamp(
    _driver: DriverRef,
    device_id: AudioObjectID,
    _client_id: u32,
    out_sample_time: *mut f64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    let reg = registry::current();
    let Some((instance, DeviceRole::Device)) = reg.resolve(device_id) else {
        registry::warn_unknown_object(device_id, "get_zero_time_stamp");
        return kAudioHardwareBadObjectError as OSStatus;
    };

    let tpf = f64::from_bits(instance.host_ticks_per_frame.load(Ordering::SeqCst));
    let ticks_per_ring = tpf * instance.ring_frames as f64;

    let anchor = instance.anchor_host_time.load(Ordering::SeqCst);
    let mut ts_count = instance.timestamp_count.load(Ordering::Relaxed);

    let now = unsafe { mach_absolute_time() };
    loop {
        let next_ts_time = anchor as f64 + (ts_count as f64 + 1.0) * ticks_per_ring;
        if (now as f64) < next_ts_time {
            break;
        }
        match instance.timestamp_count.compare_exchange_weak(
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
        *out_sample_time = (ts_count as f64) * instance.ring_frames as f64;
        *out_host_time = anchor + (ts_count as f64 * ticks_per_ring) as u64;
        *out_seed = 1;
    }
    kAudioHardwareNoError as OSStatus
}

pub unsafe extern "C" fn will_do_io_operation(
    _driver: DriverRef,
    device_id: AudioObjectID,
    _client_id: u32,
    operation_id: u32,
    out_will_do: *mut u8,
    out_will_do_in_place: *mut u8,
) -> OSStatus {
    // Confirm the device exists but we don't need the instance here.
    let reg = registry::current();
    if reg.resolve(device_id).is_none() {
        registry::warn_unknown_object(device_id, "will_do_io_operation");
        return kAudioHardwareBadObjectError as OSStatus;
    }

    match operation_id {
        x if x == kAudioServerPlugInIOOperationReadInput
            || x == kAudioServerPlugInIOOperationWriteMix =>
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
    device_id: AudioObjectID,
    _stream_id: AudioObjectID,
    _client_id: u32,
    operation_id: u32,
    io_buffer_frame_size: u32,
    io_cycle_info: *const AudioServerPlugInIOCycleInfo,
    main_buffer: *mut c_void,
    _secondary_buffer: *mut c_void,
) -> OSStatus {
    let reg = registry::current();
    let Some((instance, DeviceRole::Device)) = reg.resolve(device_id) else {
        registry::warn_unknown_object(device_id, "do_io_operation");
        return kAudioHardwareBadObjectError as OSStatus;
    };

    let channels = instance.channels as usize;
    let ring_frames = instance.ring_frames;
    let ring_samples = ring_frames * channels;
    let frame_count = io_buffer_frame_size as usize;
    let sample_count = frame_count * channels;
    let buf = main_buffer as *mut f32;

    let is_default = instance.uid == registry::DEFAULT_DEVICE_UID;

    match operation_id {
        x if x == kAudioServerPlugInIOOperationWriteMix => {
            let sample_time = unsafe { (*io_cycle_info).mOutputTime.mSampleTime } as usize;
            let offset = (sample_time % ring_frames) * channels;

            let muted = instance.mute.load(Ordering::Relaxed) != 0;
            let gain = if muted {
                0.0_f32
            } else {
                f32::from_bits(instance.volume.load(Ordering::Relaxed))
            };

            let mut peak: f32 = 0.0;
            for i in 0..sample_count {
                let ring_idx = (offset + i) % ring_samples;
                let scaled = unsafe { *buf.add(i) } * gain;
                let abs = scaled.abs();
                if abs > peak {
                    peak = abs;
                }
                instance.ring_buffer[ring_idx].store(scaled.to_bits(), Ordering::Relaxed);
            }

            if is_default {
                let activity = ACTIVITY_PTR.load(Ordering::Acquire);
                if !activity.is_null() {
                    unsafe {
                        (*activity)
                            .frames_written
                            .fetch_add(io_buffer_frame_size as u64, Ordering::Relaxed);
                        (*activity)
                            .last_write_host_time
                            .store(mach_absolute_time(), Ordering::Relaxed);
                        (*activity)
                            .peak_bits
                            .store(peak.to_bits(), Ordering::Relaxed);
                    }
                }
            }
        }
        x if x == kAudioServerPlugInIOOperationReadInput => {
            let sample_time = unsafe { (*io_cycle_info).mInputTime.mSampleTime } as usize;
            let offset = (sample_time % ring_frames) * channels;

            for i in 0..sample_count {
                let ring_idx = (offset + i) % ring_samples;
                let bits = instance.ring_buffer[ring_idx].load(Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_writes_are_truncated_to_capacity() {
        let mut buf = [0u32; 2];
        let mut written = 0u32;
        let mut pw = unsafe { PropertyWriter::new(buf.as_mut_ptr().cast(), &mut written, 4) };
        assert_eq!(pw.write_u32_slice(&[7, 8]), kAudioHardwareNoError as OSStatus);
        assert_eq!(written, 4);
        assert_eq!(buf, [7, 0]);
    }

    #[test]
    fn scalar_writes_fail_when_they_do_not_fit() {
        let mut buf = [0u32; 1];
        let mut written = 0u32;
        let mut pw = unsafe { PropertyWriter::new(buf.as_mut_ptr().cast(), &mut written, 4) };
        assert_eq!(pw.write_f64(1.0), kAudioHardwareBadPropertySizeError as OSStatus);
        assert_eq!(written, 0);
        assert_eq!(buf, [0]);
    }
}
