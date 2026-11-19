@import AVFoundation;
@import Foundation;

#include <stdatomic.h>
#include <stdbool.h>

static atomic_bool asking = false;

int spaghetti_microphone_access(void) {
    return (int)[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
}

bool spaghetti_ask_for_microphone(void) {
    if (![[NSBundle mainBundle] objectForInfoDictionaryKey:@"NSMicrophoneUsageDescription"]) {
        return false;
    }
    bool expected = false;
    if (atomic_compare_exchange_strong(&asking, &expected, true)) {
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio
                                 completionHandler:^(BOOL granted) {
            (void)granted;
            atomic_store(&asking, false);
        }];
    }
    return true;
}
