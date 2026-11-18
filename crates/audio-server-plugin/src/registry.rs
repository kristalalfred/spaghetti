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

use spaghetti_common::audio_types::RING_BUFFER_FRAMES;
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
    pub fn insert_device(&mut self, entry: &DeviceConfigEntry, ring_frames: usize) -> u32 {
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
            entry,
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
        self.by_uid.insert(entry.uid.clone(), device_id);

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

    pub fn device(&self, uid: &str) -> Option<&Arc<DeviceInstance>> {
        let device_id = self.by_uid.get(uid)?;
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

/// Claude: the devices the plugin always publishes, as config entries.
pub fn builtin_entries() -> Vec<DeviceConfigEntry> {
    crate::plugin()
        .devices
        .iter()
        .map(|device| DeviceConfigEntry {
            uid: device.uid.to_string(),
            name: device.name.to_string(),
            channels: device.channels,
            sample_rate: device.sample_rate,
            volume: device.volume,
        })
        .collect()
}

/// Build a new registry from the built-in devices and a config snapshot,
/// reusing existing `Arc<DeviceInstance>` entries for UIDs that already exist
/// in `previous` (preserving ring buffers + object IDs). Newly added UIDs get
/// fresh IDs allocated from `previous.next_id`, which is carried forward
/// monotonically.
///
/// Claude: built-in devices come first and win over a config entry with the
/// same UID. A surviving instance keeps its original format and volume mode
/// even if its entry changed them (reallocating a ring buffer would break
/// in-flight IO).
pub fn rebuild_from_entries(
    previous: &DeviceRegistry,
    builtin: &[DeviceConfigEntry],
    entries: &[DeviceConfigEntry],
) -> DeviceRegistry {
    let mut reg = DeviceRegistry::empty();
    reg.next_id = previous.next_id;

    for entry in builtin.iter().chain(entries) {
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

    reg.insert_device(entry, RING_BUFFER_FRAMES);
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
    use spaghetti_common::device_config::Volume;

    const BUILTIN: &str = "builtin";

    fn entry(uid: &str) -> DeviceConfigEntry {
        DeviceConfigEntry {
            uid: uid.into(),
            name: uid.into(),
            channels: 2,
            sample_rate: 48_000,
            volume: Volume::Applied,
        }
    }

    fn rebuild(previous: &DeviceRegistry, entries: &[DeviceConfigEntry]) -> DeviceRegistry {
        rebuild_from_entries(previous, &[entry(BUILTIN)], entries)
    }

    #[test]
    fn builtin_device_is_first_even_when_listed_later() {
        let reg = rebuild(&DeviceRegistry::empty(), &[entry("a"), entry(BUILTIN)]);
        assert_eq!(reg.by_uid[BUILTIN], FIRST_OBJECT_ID);
        assert_eq!(reg.by_uid["a"], FIRST_OBJECT_ID + IDS_PER_DEVICE);
        assert_eq!(reg.device(BUILTIN).unwrap().uid, BUILTIN);
    }

    #[test]
    fn builtin_device_survives_omission() {
        let reg = rebuild(&DeviceRegistry::empty(), &[entry("a")]);
        assert_eq!(reg.by_id.len(), 2);
        assert!(reg.device(BUILTIN).is_some());
    }

    #[test]
    fn builtin_device_wins_over_a_config_entry_with_its_uid() {
        let mut listed = entry(BUILTIN);
        listed.volume = Volume::Reported;
        let reg = rebuild(&DeviceRegistry::empty(), &[listed]);
        assert_eq!(reg.device(BUILTIN).unwrap().volume_mode, Volume::Applied);
    }

    #[test]
    fn without_builtin_devices_only_config_entries_are_published() {
        let reg = rebuild_from_entries(&DeviceRegistry::empty(), &[], &[entry("a")]);
        assert_eq!(reg.devices().map(|d| d.uid.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn a_device_keeps_the_volume_mode_of_its_entry() {
        let mut reported = entry("a");
        reported.volume = Volume::Reported;
        let reg = rebuild(&DeviceRegistry::empty(), &[reported]);
        assert_eq!(reg.device("a").unwrap().volume_mode, Volume::Reported);
    }

    #[test]
    fn rebuild_keeps_surviving_instances_and_retires_removed_ids() {
        let first = rebuild(&DeviceRegistry::empty(), &[entry("a"), entry("b")]);
        let a_before = Arc::clone(&first.by_id[&first.by_uid["a"]]);

        let second = rebuild(&first, &[entry("a"), entry("c")]);
        let a_after = &second.by_id[&second.by_uid["a"]];
        assert!(Arc::ptr_eq(&a_before, a_after));
        assert!(!second.by_uid.contains_key("b"));
        assert!(second.by_uid["c"] > first.by_uid["b"]);
        assert!(second.resolve(first.by_uid["b"] + 2).is_none());
    }

    #[test]
    fn duplicate_uids_publish_one_device() {
        let reg = rebuild(&DeviceRegistry::empty(), &[entry("a"), entry("a")]);
        assert_eq!(reg.by_id.len(), 2);
        assert_eq!(reg.devices().count(), 2);
    }
}
