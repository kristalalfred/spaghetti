//! Claude: Core Audio asks for microphone access before it runs an IOProc on
//! a device that has an input, even one that only plays to it, and waits for
//! the answer in coreaudiod, which holds up every other process's input until
//! it comes. So the engine asks itself, without waiting, and opens no such
//! device until the question is answered.

use coreaudio_sys::AudioObjectID;

use super::audio;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Access {
    Unasked,
    Answered,
}

unsafe extern "C" {
    unsafe fn spaghetti_microphone_access() -> i32;
    unsafe fn spaghetti_ask_for_microphone() -> bool;
}

/// Claude: `AVAuthorizationStatusNotDetermined`.
const NOT_DETERMINED: i32 = 0;

fn access() -> Access {
    if unsafe { spaghetti_microphone_access() } == NOT_DETERMINED {
        Access::Unasked
    } else {
        Access::Answered
    }
}

/// Claude: whether the process could ask; it cannot without an
/// `NSMicrophoneUsageDescription`.
fn ask() -> bool {
    unsafe { spaghetti_ask_for_microphone() }
}

/// Claude: refused while opening `device` would wait on a microphone prompt.
pub(crate) fn before_opening(device: AudioObjectID) -> Result<(), String> {
    check(audio::has_input(device), access, ask)
}

/// Claude: refused while reading an input would wait on a microphone prompt.
pub(crate) fn before_reading_input() -> Result<(), String> {
    check(true, access, ask)
}

fn check(has_input: bool, access: fn() -> Access, ask: fn() -> bool) -> Result<(), String> {
    if !has_input || access() == Access::Answered {
        return Ok(());
    }
    Err(if ask() {
        "the device has an input, which needs microphone access: asked for it, try again once \
         it is answered"
            .into()
    } else {
        "the device has an input, which needs microphone access, and this program cannot ask \
         for it without an NSMicrophoneUsageDescription"
            .into()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    static ASKED: AtomicBool = AtomicBool::new(false);

    fn unasked() -> Access {
        Access::Unasked
    }

    fn answered() -> Access {
        Access::Answered
    }

    fn can_ask() -> bool {
        ASKED.store(true, Ordering::SeqCst);
        true
    }

    fn cannot_ask() -> bool {
        false
    }

    fn never_asks() -> bool {
        panic!("asked for the microphone")
    }

    #[test]
    fn a_device_without_an_input_opens_without_asking() {
        assert_eq!(check(false, unasked, never_asks), Ok(()));
    }

    #[test]
    fn an_input_opens_once_access_is_answered_either_way() {
        assert_eq!(check(true, answered, never_asks), Ok(()));
    }

    #[test]
    fn an_input_waits_for_an_unasked_question_without_blocking() {
        let refused = check(true, unasked, can_ask).unwrap_err();
        assert!(ASKED.load(Ordering::SeqCst));
        assert!(refused.contains("try again"), "{refused}");
        let refused = check(true, unasked, cannot_ask).unwrap_err();
        assert!(
            refused.contains("NSMicrophoneUsageDescription"),
            "{refused}"
        );
    }
}
