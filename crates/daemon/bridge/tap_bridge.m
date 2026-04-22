@import Foundation;
@import CoreAudio;
@import AudioToolbox;

#import <CoreAudio/CATapDescription.h>
#import "tap_bridge.h"

/// Resolve a PID to a CoreAudio process AudioObjectID.
/// Returns kAudioObjectUnknown if the process is not known to CoreAudio.
static AudioObjectID process_object_for_pid(pid_t pid) {
    AudioObjectPropertyAddress prop = {
        .mSelector = kAudioHardwarePropertyProcessObjectList,
        .mScope    = kAudioObjectPropertyScopeGlobal,
        .mElement  = kAudioObjectPropertyElementMain,
    };

    UInt32 dataSize = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(
        kAudioObjectSystemObject, &prop, 0, NULL, &dataSize
    );
    if (status != noErr || dataSize == 0) {
        return kAudioObjectUnknown;
    }

    UInt32 count = dataSize / sizeof(AudioObjectID);
    AudioObjectID *objects = calloc(count, sizeof(AudioObjectID));
    if (!objects) return kAudioObjectUnknown;

    status = AudioObjectGetPropertyData(
        kAudioObjectSystemObject, &prop, 0, NULL, &dataSize, objects
    );
    if (status != noErr) {
        free(objects);
        return kAudioObjectUnknown;
    }

    AudioObjectPropertyAddress pidProp = {
        .mSelector = kAudioProcessPropertyPID,
        .mScope    = kAudioObjectPropertyScopeGlobal,
        .mElement  = kAudioObjectPropertyElementMain,
    };

    AudioObjectID result = kAudioObjectUnknown;
    for (UInt32 i = 0; i < count; i++) {
        pid_t objPid = 0;
        UInt32 pidSize = sizeof(pid_t);
        status = AudioObjectGetPropertyData(
            objects[i], &pidProp, 0, NULL, &pidSize, &objPid
        );
        if (status == noErr && objPid == pid) {
            result = objects[i];
            break;
        }
    }

    free(objects);
    return result;
}

OSStatus spaghetti_create_tap(
    const pid_t *pids,
    uint32_t pid_count,
    AudioObjectID *out_tap_id
) {
    if (!pids || pid_count == 0 || !out_tap_id) {
        return kAudio_ParamError;
    }

    // Convert PIDs to CoreAudio process object IDs
    NSMutableArray<NSNumber *> *processObjects = [NSMutableArray arrayWithCapacity:pid_count];
    for (uint32_t i = 0; i < pid_count; i++) {
        AudioObjectID objID = process_object_for_pid(pids[i]);
        if (objID == kAudioObjectUnknown) {
            return kAudio_ParamError;
        }
        [processObjects addObject:@(objID)];
    }

    CATapDescription *desc = [[CATapDescription alloc] initStereoMixdownOfProcesses:processObjects];

    AudioObjectID tapID = kAudioObjectUnknown;
    OSStatus status = AudioHardwareCreateProcessTap(desc, &tapID);
    if (status != noErr) {
        return status;
    }

    *out_tap_id = tapID;
    return noErr;
}

OSStatus spaghetti_destroy_tap(AudioObjectID tap_id) {
    return AudioHardwareDestroyProcessTap(tap_id);
}

/// Query the tap object for its UID string (needed for the aggregate device dict).
static NSString *get_tap_uid(AudioObjectID tap_id) {
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioTapPropertyUID,
        .mScope    = kAudioObjectPropertyScopeGlobal,
        .mElement  = kAudioObjectPropertyElementMain,
    };

    CFStringRef uidRef = NULL;
    UInt32 dataSize = sizeof(CFStringRef);
    OSStatus status = AudioObjectGetPropertyData(
        tap_id, &addr, 0, NULL, &dataSize, &uidRef
    );
    if (status != noErr || uidRef == NULL) {
        return nil;
    }
    return (__bridge_transfer NSString *)uidRef;
}

OSStatus spaghetti_create_aggregate_for_tap(
    AudioObjectID tap_id,
    const char *aggregate_uid,
    AudioObjectID *out_aggregate_id
) {
    if (!aggregate_uid || !out_aggregate_id) {
        return kAudio_ParamError;
    }

    NSString *tapUID = get_tap_uid(tap_id);
    if (!tapUID) {
        return kAudio_ParamError;
    }

    NSDictionary *aggDesc = @{
        @(kAudioAggregateDeviceUIDKey):          @(aggregate_uid),
        @(kAudioAggregateDeviceNameKey):         @"Spaghetti Tap",
        @(kAudioAggregateDeviceIsPrivateKey):    @YES,
        @(kAudioAggregateDeviceTapAutoStartKey): @YES,
        @(kAudioAggregateDeviceTapListKey):      @[tapUID],
    };

    AudioObjectID aggregateID = kAudioObjectUnknown;
    OSStatus status = AudioHardwareCreateAggregateDevice(
        (__bridge CFDictionaryRef)aggDesc, &aggregateID
    );
    if (status != noErr) {
        return status;
    }

    *out_aggregate_id = aggregateID;
    return noErr;
}

OSStatus spaghetti_destroy_aggregate(AudioObjectID aggregate_id) {
    return AudioHardwareDestroyAggregateDevice(aggregate_id);
}
