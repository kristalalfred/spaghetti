#ifndef TAP_BRIDGE_H
#define TAP_BRIDGE_H

#include <CoreAudio/CoreAudio.h>
#include <AudioToolbox/AudioToolbox.h>

/// Create a process tap targeting the given PIDs.
/// Returns the tap's AudioObjectID via out_tap_id.
OSStatus spaghetti_create_tap(
    const pid_t *pids,
    uint32_t pid_count,
    AudioObjectID *out_tap_id
);

/// Destroy a previously created process tap.
OSStatus spaghetti_destroy_tap(AudioObjectID tap_id);

/// Create a private aggregate device that captures from the given tap.
/// The aggregate device acts as an input-only device the daemon can read from.
OSStatus spaghetti_create_aggregate_for_tap(
    AudioObjectID tap_id,
    const char *aggregate_uid,
    AudioObjectID *out_aggregate_id
);

/// Destroy an aggregate device.
OSStatus spaghetti_destroy_aggregate(AudioObjectID aggregate_id);

#endif
