//! Static vtable and driver ref chain for the AudioServerPlugIn CFPlugIn interface.

use coreaudio_sys::{AudioServerPlugInDriverInterface, CFUUIDBytes};

use crate::driver;

/// Our own driver ref type — coreaudio-sys marks its `AudioServerPlugInDriverRef` private.
/// The C type is `AudioServerPlugInDriverInterface **`.
pub(crate) type DriverRef = *mut *mut AudioServerPlugInDriverInterface;

// ---------------------------------------------------------------------------
// Sync wrappers for statics containing raw pointers
// ---------------------------------------------------------------------------

/// Wraps the vtable struct (contains fn pointers + a null `*mut c_void` reserved field).
#[repr(transparent)]
struct SyncVtable(AudioServerPlugInDriverInterface);
// SAFETY: the vtable is const-initialized and never mutated.
unsafe impl Sync for SyncVtable {}

/// Wraps a raw pointer for use in a static.
#[repr(transparent)]
struct SyncRawPtr<T>(*const T);
// SAFETY: the pointed-to data is a static that is never mutated.
unsafe impl<T> Sync for SyncRawPtr<T> {}

/// Wraps a `DriverRef` (`*mut *mut AudioServerPlugInDriverInterface`) for use in a static.
#[repr(transparent)]
pub(crate) struct SyncDriverRef(pub(crate) DriverRef);
// SAFETY: coreaudiod treats this as an opaque immutable handle. We never mutate through it.
unsafe impl Sync for SyncDriverRef {}

// ---------------------------------------------------------------------------
// UUIDs
// ---------------------------------------------------------------------------

/// kAudioServerPlugInDriverInterfaceUUID: EEA5773D-CC43-49F1-8E00-8F96E7D23B17
pub const DRIVER_INTERFACE_UUID: CFUUIDBytes = CFUUIDBytes {
    byte0: 0xEE, byte1: 0xA5, byte2: 0x77, byte3: 0x3D,
    byte4: 0xCC, byte5: 0x43, byte6: 0x49, byte7: 0xF1,
    byte8: 0x8E, byte9: 0x00, byte10: 0x8F, byte11: 0x96,
    byte12: 0xE7, byte13: 0xD2, byte14: 0x3B, byte15: 0x17,
};

// ---------------------------------------------------------------------------
// Static vtable + driver ref chain
// ---------------------------------------------------------------------------

static VTABLE: SyncVtable = SyncVtable(AudioServerPlugInDriverInterface {
    _reserved: std::ptr::null_mut(),

    QueryInterface: Some(driver::query_interface),
    AddRef: Some(driver::add_ref),
    Release: Some(driver::release),

    Initialize: Some(driver::initialize),
    CreateDevice: Some(driver::create_device),
    DestroyDevice: Some(driver::destroy_device),
    AddDeviceClient: Some(driver::add_device_client),
    RemoveDeviceClient: Some(driver::remove_device_client),
    PerformDeviceConfigurationChange: Some(driver::perform_device_configuration_change),
    AbortDeviceConfigurationChange: Some(driver::abort_device_configuration_change),

    HasProperty: Some(driver::has_property),
    IsPropertySettable: Some(driver::is_property_settable),
    GetPropertyDataSize: Some(driver::get_property_data_size),
    GetPropertyData: Some(driver::get_property_data),
    SetPropertyData: Some(driver::set_property_data),

    StartIO: Some(driver::start_io),
    StopIO: Some(driver::stop_io),
    GetZeroTimeStamp: Some(driver::get_zero_time_stamp),
    WillDoIOOperation: Some(driver::will_do_io_operation),
    BeginIOOperation: Some(driver::begin_io_operation),
    DoIOOperation: Some(driver::do_io_operation),
    EndIOOperation: Some(driver::end_io_operation),
});

/// The `DriverRef` to hand back to coreaudiod.
///
/// Pointer chain: `&DRIVER_REF -> &VTABLE_PTR -> &VTABLE` which is
/// the `AudioServerPlugInDriverInterface **` the HAL expects.
/// Intermediate pointer: points to the vtable struct.
static VTABLE_PTR: SyncRawPtr<AudioServerPlugInDriverInterface> =
    SyncRawPtr(std::ptr::addr_of!(VTABLE) as *const AudioServerPlugInDriverInterface);

/// The `DriverRef` to hand back to coreaudiod.
///
/// Pointer chain: `&DRIVER_REF -> &VTABLE_PTR -> &VTABLE` which is
/// the `AudioServerPlugInDriverInterface **` the HAL expects.
pub(crate) static DRIVER_REF: SyncDriverRef =
    SyncDriverRef(std::ptr::addr_of!(VTABLE_PTR) as DriverRef);
