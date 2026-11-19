use std::io;
use std::time::Duration;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as os;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

#[cfg(test)]
pub(crate) use os::second_loopback;
pub(crate) use os::{
    AsRawSocket, WakeSocket, connect_in_progress, error, ignore_unreachable, wake_pair,
};

const MOST_INTERESTS: usize = 8;

/// Claude: a socket to wait on, for reading or for writing, or nothing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Interest {
    socket: Option<os::RawSocket>,
    write: bool,
}

impl Interest {
    pub(crate) const NONE: Interest = Interest {
        socket: None,
        write: false,
    };

    pub(crate) fn readable(socket: &impl AsRawSocket) -> Interest {
        Interest {
            socket: Some(os::raw(socket)),
            write: false,
        }
    }

    pub(crate) fn writable(socket: &impl AsRawSocket) -> Interest {
        Interest {
            socket: Some(os::raw(socket)),
            write: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn waits(&self) -> bool {
        self.socket.is_some()
    }
}

/// Claude: which of `interests` are ready for what they wait on, or have
/// failed or hung up; none when the wait itself fails.
pub(crate) fn poll<const N: usize>(interests: [Interest; N], timeout: Duration) -> [bool; N] {
    let mut ready = [false; N];
    match wait(&interests, &mut ready, timeout) {
        Ok(_) => ready,
        Err(_) => [false; N],
    }
}

/// Claude: waits up to `timeout`, rounded up to whole milliseconds, and marks
/// in `ready` which of `interests` are ready for what they wait on, or have
/// failed or hung up. Returns how many sockets were ready.
pub(crate) fn wait(
    interests: &[Interest],
    ready: &mut [bool],
    timeout: Duration,
) -> io::Result<usize> {
    assert!(interests.len() <= MOST_INTERESTS && ready.len() == interests.len());
    let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    os::wait(interests, ready, ms)
}
