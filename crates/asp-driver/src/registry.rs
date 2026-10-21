//! Lock-free device registry.
//!
//! A single [`arc_swap::ArcSwap<DeviceRegistry>`] is the source of truth for
//! every virtual device the plugin publishes. The registry maps:
//!
//! * object-ID → `(device_id, role)` for fast dispatch in property and IO
//!   callbacks,
//! * device-ID → `Arc<DeviceInstance>` for the state itself,
//! * UID string → device-ID for `TranslateUIDToDevice`.
//!
//! Object IDs are allocated monotonically starting at [`FIRST_OBJECT_ID`] in
//! blocks of [`IDS_PER_DEVICE`]. IDs are never recycled — a device that
//! disappears leaves its IDs retired.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;

use spaghetti_common::audio_types::{CHANNELS, RING_BUFFER_FRAMES, SAMPLE_RATE};
use spaghetti_common::device_config::DeviceConfigEntry;

use crate::devices::{DeviceInstance, DeviceRole};
use crate::log;

/// Plugin object ID. `kAudioObjectPlugInObject` is conventionally `1`.
pub const OBJECT_ID_PLUGIN: u32 = 1;

/// First object ID handed out to devices. Starts above [`OBJECT_ID_PLUGIN`]
/// with a comfortable gap so static plugin IDs never collide.
pub const FIRST_OBJECT_ID: u32 = 100;

/// Object IDs consumed per device: device, input stream, output stream,
/// volume control, mute control.
pub const IDS_PER_DEVICE: u32 = 5;

/// UID of the always-present default loopback device.
pub const DEFAULT_DEVICE_UID: &str = "SpaghettiDevice_UID";
/// Display name of the default loopback device.
pub const DEFAULT_DEVICE_NAME: &str = "Spaghetti";

/// Immutable snapshot of device state. Mutations happen by constructing a
/// fresh [`DeviceRegistry`] and [`ArcSwap::store`]-ing it.
pub struct DeviceRegistry {
    /// Device-ID → instance.
    pub by_id: HashMap<u32, Arc<DeviceInstance>>,
    /// Every object ID → (owning device-ID, role). Includes the device's own
    /// object ID (with `DeviceRole::Device`) so lookups are uniform.
    pub by_object_role: HashMap<u32, (u32, DeviceRole)>,
    /// UID string → device-ID for `TranslateUIDToDevice`.
    pub by_uid: HashMap<String, u32>,
    /// Next object ID to hand out. Monotonic across swaps.
    pub next_id: u32,
}

impl DeviceRegistry {
    /// Empty registry with the first allocatable ID.
    pub fn empty() -> Self {
        Self {
            by_id: HashMap::new(),
            by_object_role: HashMap::new(),
            by_uid: HashMap::new(),
            next_id: FIRST_OBJECT_ID,
        }
    }

    /// Reserve a contiguous block of [`IDS_PER_DEVICE`] IDs and return the
    /// first one. The caller assigns them to roles in the fixed order
    /// (device, stream_in, stream_out, volume, mute).
    fn allocate_block(&mut self) -> u32 {
        let start = self.next_id;
        self.next_id = self.next_id.checked_add(IDS_PER_DEVICE).unwrap_or_else(|| {
            log("FATAL: AudioObjectID space exhausted — aborting");
            std::process::abort();
        });
        start
    }

    /// Add a device with the given identity/format. Returns the new
    /// device-ID. The registry gains one entry in `by_id`, five in
    /// `by_object_role`, and one in `by_uid`.
    pub fn insert_device(
        &mut self,
        uid: String,
        name: String,
        channels: u32,
        sample_rate: f64,
        ring_frames: usize,
    ) -> u32 {
        let base = self.allocate_block();
        let device_id = base;
        let stream_in_id = base + 1;
        let stream_out_id = base + 2;
        let volume_id = base + 3;
        let mute_id = base + 4;

        let instance = Arc::new(DeviceInstance::new(
            device_id,
            stream_in_id,
            stream_out_id,
            volume_id,
            mute_id,
            uid.clone(),
            name,
            channels,
            sample_rate,
            ring_frames,
        ));

        self.by_id.insert(device_id, instance);
        self.by_object_role
            .insert(device_id, (device_id, DeviceRole::Device));
        self.by_object_role
            .insert(stream_in_id, (device_id, DeviceRole::StreamInput));
        self.by_object_role
            .insert(stream_out_id, (device_id, DeviceRole::StreamOutput));
        self.by_object_role
            .insert(volume_id, (device_id, DeviceRole::Volume));
        self.by_object_role
            .insert(mute_id, (device_id, DeviceRole::Mute));
        self.by_uid.insert(uid, device_id);

        device_id
    }

    /// Re-insert an existing [`DeviceInstance`] (already owns its object IDs
    /// from a previous registry snapshot). Preserves ring buffer identity so
    /// active IO doesn't glitch across reloads.
    pub fn reinsert_device(&mut self, instance: Arc<DeviceInstance>) {
        let device_id = instance.device_id;
        self.by_object_role
            .insert(device_id, (device_id, DeviceRole::Device));
        self.by_object_role
            .insert(instance.stream_in_id, (device_id, DeviceRole::StreamInput));
        self.by_object_role
            .insert(instance.stream_out_id, (device_id, DeviceRole::StreamOutput));
        self.by_object_role
            .insert(instance.volume_id, (device_id, DeviceRole::Volume));
        self.by_object_role
            .insert(instance.mute_id, (device_id, DeviceRole::Mute));
        self.by_uid.insert(instance.uid.clone(), device_id);
        self.by_id.insert(device_id, instance);
    }

    /// Resolve an object ID to `(instance, role)` without cloning the `Arc`
    /// unless the caller wants to. Used on hot paths; returns `None` for
    /// unknown IDs.
    pub fn resolve(&self, object_id: u32) -> Option<(&Arc<DeviceInstance>, DeviceRole)> {
        let (device_id, role) = *self.by_object_role.get(&object_id)?;
        let instance = self.by_id.get(&device_id)?;
        Some((instance, role))
    }

    /// Iterator over every device in stable insertion-id order.
    pub fn devices(&self) -> impl Iterator<Item = &Arc<DeviceInstance>> {
        // Sort by device_id so DeviceList is deterministic across rebuilds.
        let mut ids: Vec<u32> = self.by_id.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(move |id| self.by_id.get(&id))
    }

    pub fn default_device(&self) -> Option<&Arc<DeviceInstance>> {
        let device_id = self.by_uid.get(DEFAULT_DEVICE_UID)?;
        self.by_id.get(device_id)
    }
}

/// Singleton registry. Initialized to an empty registry at load time and
/// replaced on `initialize`. IO callbacks do one [`ArcSwap::load`] per call.
static REGISTRY: LazyLock<ArcSwap<DeviceRegistry>> =
    LazyLock::new(|| ArcSwap::from_pointee(DeviceRegistry::empty()));

/// Return the current registry snapshot. Cheap and lock-free.
pub fn current() -> arc_swap::Guard<Arc<DeviceRegistry>> {
    REGISTRY.load()
}

/// Replace the registry atomically.
pub fn store(new: DeviceRegistry) {
    REGISTRY.store(Arc::new(new));
}

/// The always-on built-in default device entry. Returned even if the config
/// file omits it, so the plugin never loses `SpaghettiDevice_UID`.
fn default_entry() -> DeviceConfigEntry {
    DeviceConfigEntry {
        uid: DEFAULT_DEVICE_UID.to_string(),
        name: DEFAULT_DEVICE_NAME.to_string(),
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE as u32,
    }
}

/// Build a new registry from a config snapshot, reusing existing
/// `Arc<DeviceInstance>` entries for UIDs that already exist in `previous`
/// (preserving ring buffers + object IDs). Newly added UIDs get fresh IDs
/// allocated from `previous.next_id`, which is carried forward monotonically.
///
/// The built-in default device (`SpaghettiDevice_UID`) is always included,
/// even if the entries don't list it. If the caller's entries list it with
/// different channels/rate, the config wins for new allocations but a
/// surviving instance keeps its original format (reallocating a ring buffer
/// would break in-flight IO).
pub fn rebuild_from_entries(
    previous: &DeviceRegistry,
    entries: &[DeviceConfigEntry],
) -> DeviceRegistry {
    let mut reg = DeviceRegistry::empty();
    reg.next_id = previous.next_id;

    let default = entries
        .iter()
        .find(|e| e.uid == DEFAULT_DEVICE_UID)
        .cloned()
        .unwrap_or_else(default_entry);
    add_entry(&mut reg, previous, &default);
    for entry in entries {
        add_entry(&mut reg, previous, entry);
    }

    reg
}

fn add_entry(reg: &mut DeviceRegistry, previous: &DeviceRegistry, entry: &DeviceConfigEntry) {
    if reg.by_uid.contains_key(&entry.uid) {
        return;
    }

    if let Some(&prev_device_id) = previous.by_uid.get(&entry.uid)
        && let Some(instance) = previous.by_id.get(&prev_device_id)
    {
        reg.reinsert_device(Arc::clone(instance));
        return;
    }

    reg.insert_device(
        entry.uid.clone(),
        entry.name.clone(),
        entry.channels,
        f64::from(entry.sample_rate),
        RING_BUFFER_FRAMES,
    );
}

/// First-fire gate for unknown-object-ID warnings so repeated misses don't
/// spam the log.
static UNKNOWN_ID_LOGGED: AtomicBool = AtomicBool::new(false);

/// Log (once) that a property dispatch hit an object ID we don't recognize.
pub fn warn_unknown_object(object_id: u32, context: &str) {
    if !UNKNOWN_ID_LOGGED.swap(true, Ordering::Relaxed) {
        log(&format!(
            "WARN: unknown object id {object_id} in {context} — this is the first such miss; further misses will be silent"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uid: &str) -> DeviceConfigEntry {
        DeviceConfigEntry {
            uid: uid.into(),
            name: uid.into(),
            channels: 2,
            sample_rate: 48_000,
        }
    }

    #[test]
    fn default_device_is_first_even_when_listed_later() {
        let reg = rebuild_from_entries(
            &DeviceRegistry::empty(),
            &[entry("a"), entry(DEFAULT_DEVICE_UID)],
        );
        assert_eq!(reg.by_uid[DEFAULT_DEVICE_UID], FIRST_OBJECT_ID);
        assert_eq!(reg.by_uid["a"], FIRST_OBJECT_ID + IDS_PER_DEVICE);
        assert_eq!(reg.default_device().unwrap().uid, DEFAULT_DEVICE_UID);
    }

    #[test]
    fn default_device_survives_omission() {
        let reg = rebuild_from_entries(&DeviceRegistry::empty(), &[entry("a")]);
        assert_eq!(reg.by_id.len(), 2);
        assert!(reg.default_device().is_some());
    }

    #[test]
    fn rebuild_keeps_surviving_instances_and_retires_removed_ids() {
        let first = rebuild_from_entries(&DeviceRegistry::empty(), &[entry("a"), entry("b")]);
        let a_before = Arc::clone(&first.by_id[&first.by_uid["a"]]);

        let second = rebuild_from_entries(&first, &[entry("a"), entry("c")]);
        let a_after = &second.by_id[&second.by_uid["a"]];
        assert!(Arc::ptr_eq(&a_before, a_after));
        assert!(!second.by_uid.contains_key("b"));
        assert!(second.by_uid["c"] > first.by_uid["b"]);
        assert!(second.resolve(first.by_uid["b"] + 2).is_none());
    }

    #[test]
    fn duplicate_uids_publish_one_device() {
        let reg = rebuild_from_entries(&DeviceRegistry::empty(), &[entry("a"), entry("a")]);
        assert_eq!(reg.by_id.len(), 2);
        assert_eq!(reg.devices().count(), 2);
    }
}
