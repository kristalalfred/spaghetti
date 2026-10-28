#ifndef TAP_BRIDGE_H
#define TAP_BRIDGE_H

#include <CoreAudio/CoreAudio.h>
#include <AudioToolbox/AudioToolbox.h>

/// Create a process tap targeting the given PIDs, which may be none.
/// mute_behavior: 0 = CATapUnmuted (mirror), 1 = CATapMutedWhenTapped.
/// Returns the tap's AudioObjectID via out_tap_id.
OSStatus spaghetti_create_tap(
    const pid_t *pids,
    uint32_t pid_count,
    int mute_behavior,
    AudioObjectID *out_tap_id
);

/// Destroy a previously created process tap.
OSStatus spaghetti_destroy_tap(AudioObjectID tap_id);

/// Replace the processes a running tap mixes. PIDs Core Audio does not know are
/// skipped; it fails when none are left.
OSStatus spaghetti_set_tap_processes(AudioObjectID tap_id, const pid_t *pids, uint32_t pid_count);

/// Create a private aggregate device that captures from the given tap.
/// The aggregate device acts as an input-only device the daemon can read from.
OSStatus spaghetti_create_aggregate_for_tap(
    AudioObjectID tap_id,
    const char *aggregate_uid,
    AudioObjectID *out_aggregate_id
);

/// Destroy an aggregate device.
OSStatus spaghetti_destroy_aggregate(AudioObjectID aggregate_id);

/// Point a tap at other processes, which may be none, and set how it mutes them.
/// Unlike changing an aggregate's taps, this does not interrupt the aggregate's IO.
OSStatus spaghetti_retarget_tap(
    AudioObjectID tap_id,
    const pid_t *pids,
    uint32_t pid_count,
    int mute_behavior
);

/// Create a private aggregate device whose clock and output are the given
/// device and whose inputs are the given taps, each drift-compensated to it.
OSStatus spaghetti_create_bus_aggregate(
    const char *device_uid,
    const AudioObjectID *taps,
    uint32_t tap_count,
    const char *aggregate_uid,
    AudioObjectID *out_aggregate_id
);

/// Turn off all but the last `count` input streams for an IOProc, so the
/// aggregate does not run inputs of its sub-device that the IOProc ignores.
OSStatus spaghetti_read_last_inputs_only(
    AudioObjectID device,
    AudioDeviceIOProcID ioproc,
    uint32_t count
);

#endif
