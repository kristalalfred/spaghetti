@import Foundation;
@import CoreAudio;

// Claude: HAL property listeners whose context outlives every call to them.
// AudioHardware.h documents that a listener block is Block_copy'd and kept until
// AudioObjectRemovePropertyListenerBlock, and that it is dispatched
// asynchronously onto the queue it was added with; it does not say whether a
// dispatch already under way can still run after removal returns. The block
// here holds the context's owner, so a call runs with its context alive
// whenever the HAL makes it, and the owner releases the context only once the
// last copy of the block is gone. Ending a watch also drains its private
// serial queue, so calls queued before removal have run when it returns.

typedef void (*spaghetti_watch_notify)(void *context);
typedef void (*spaghetti_watch_release)(void *context);

@interface SpaghettiWatchOwner : NSObject
@end

@implementation SpaghettiWatchOwner {
    void *_context;
    spaghetti_watch_notify _notify;
    spaghetti_watch_release _release;
}

- (instancetype)initWithContext:(void *)context
                         notify:(spaghetti_watch_notify)notify
                        release:(spaghetti_watch_release)release {
    if ((self = [super init])) {
        _context = context;
        _notify = notify;
        _release = release;
    }
    return self;
}

- (void)fire {
    _notify(_context);
}

- (void)dealloc {
    _release(_context);
}
@end

typedef struct {
    AudioObjectID object;
    AudioObjectPropertyAddress address;
} SpaghettiWatched;

@interface SpaghettiWatch : NSObject
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, copy) AudioObjectPropertyListenerBlock block;
@property(nonatomic, strong) NSMutableData *watched;
@end

@implementation SpaghettiWatch
@end

void *spaghetti_watch_new(
    spaghetti_watch_notify notify,
    spaghetti_watch_release release,
    void *context
) {
    SpaghettiWatchOwner *owner = [[SpaghettiWatchOwner alloc] initWithContext:context
                                                                       notify:notify
                                                                      release:release];
    SpaghettiWatch *watch = [SpaghettiWatch new];
    watch.queue = dispatch_queue_create("spaghetti.watch", DISPATCH_QUEUE_SERIAL);
    watch.block = ^(UInt32 count, const AudioObjectPropertyAddress *addresses) {
        (void)count;
        (void)addresses;
        [owner fire];
    };
    watch.watched = [NSMutableData data];
    return (__bridge_retained void *)watch;
}

OSStatus spaghetti_watch_add(
    void *handle,
    AudioObjectID object,
    AudioObjectPropertySelector selector,
    AudioObjectPropertyScope scope
) {
    SpaghettiWatch *watch = (__bridge SpaghettiWatch *)handle;
    SpaghettiWatched watched = {
        .object = object,
        .address = {
            .mSelector = selector,
            .mScope = scope,
            .mElement = kAudioObjectPropertyElementMain,
        },
    };
    OSStatus status =
        AudioObjectAddPropertyListenerBlock(object, &watched.address, watch.queue, watch.block);
    if (status == noErr) {
        [watch.watched appendBytes:&watched length:sizeof watched];
    }
    return status;
}

void spaghetti_watch_end(void *handle) {
    SpaghettiWatch *watch = (__bridge_transfer SpaghettiWatch *)handle;
    const SpaghettiWatched *all = watch.watched.bytes;
    NSUInteger count = watch.watched.length / sizeof(SpaghettiWatched);
    for (NSUInteger i = 0; i < count; i++) {
        AudioObjectRemovePropertyListenerBlock(
            all[i].object, &all[i].address, watch.queue, watch.block
        );
    }
    dispatch_sync(watch.queue, ^{});
}
