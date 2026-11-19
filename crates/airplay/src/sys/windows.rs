use std::io;
use std::net::{Ipv4Addr, UdpSocket};
pub(crate) use std::os::windows::io::{AsRawSocket, RawSocket};
use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{
    INVALID_SOCKET, POLLERR, POLLHUP, POLLRDNORM, POLLWRNORM, SIO_UDP_CONNRESET, SOCKET,
    SOCKET_ERROR, WSAEWOULDBLOCK, WSAIoctl, WSAPOLLFD, WSAPoll,
};

use super::{Interest, MOST_INTERESTS};

pub(super) fn raw(socket: &impl AsRawSocket) -> RawSocket {
    socket.as_raw_socket()
}

/// Claude: `WSAPoll` takes no placeholder entry, as `poll` takes a negative
/// descriptor, and fails given no sockets at all, so only the sockets wanted
/// are passed, and a wait for nothing sleeps.
pub(super) fn wait(interests: &[Interest], ready: &mut [bool], ms: i32) -> io::Result<usize> {
    let mut fds = [WSAPOLLFD {
        fd: INVALID_SOCKET,
        events: 0,
        revents: 0,
    }; MOST_INTERESTS];
    let mut indices = [0; MOST_INTERESTS];
    let mut used = 0;
    for (index, interest) in interests.iter().enumerate() {
        if let Some(socket) = interest.socket {
            fds[used] = WSAPOLLFD {
                fd: socket as SOCKET,
                events: if interest.write {
                    POLLWRNORM
                } else {
                    POLLRDNORM
                },
                revents: 0,
            };
            indices[used] = index;
            used += 1;
        }
    }
    ready.fill(false);
    if used == 0 {
        thread::sleep(Duration::from_millis(ms as u64));
        return Ok(0);
    }
    let count = unsafe { WSAPoll(fds.as_mut_ptr(), used as u32, ms) };
    if count == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    for (fd, &index) in fds[..used].iter().zip(&indices[..used]) {
        ready[index] = fd.revents & (fd.events | POLLHUP | POLLERR) != 0;
    }
    Ok(count as usize)
}

/// Claude: what one thread writes to wake another polling its pair: two
/// loopback UDP sockets connected to each other, which take datagrams from
/// each other only.
pub(crate) type WakeSocket = UdpSocket;

pub(crate) fn wake_pair() -> io::Result<(WakeSocket, WakeSocket)> {
    let one = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    let other = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    one.connect(other.local_addr()?)?;
    other.connect(one.local_addr()?)?;
    Ok((one, other))
}

/// Claude: keeps ICMP errors for datagrams sent from `socket` out of its
/// receives, as on Unix, where an unconnected socket gets none. Windows
/// otherwise fails the socket's next receive with `WSAECONNRESET` after a
/// datagram to a closed port.
pub(crate) fn ignore_unreachable(socket: &UdpSocket) -> io::Result<()> {
    let off: u32 = 0;
    let mut returned = 0;
    let result = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as SOCKET,
            SIO_UDP_CONNRESET,
            (&raw const off).cast(),
            size_of_val(&off) as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Claude: whether a non-blocking connect failed only because it has yet to
/// finish.
pub(crate) fn connect_in_progress(error: &io::Error) -> bool {
    error.raw_os_error() == Some(WSAEWOULDBLOCK)
}

pub(crate) mod error {
    use windows_sys::Win32::Networking::WinSock as ws;

    pub(crate) const HOST_DOWN: i32 = ws::WSAEHOSTDOWN;
    #[cfg(test)]
    pub(crate) const CONNECTION_REFUSED: i32 = ws::WSAECONNREFUSED;
    #[cfg(test)]
    pub(crate) const HOST_UNREACHABLE: i32 = ws::WSAEHOSTUNREACH;
    #[cfg(test)]
    pub(crate) const NETWORK_UNREACHABLE: i32 = ws::WSAENETUNREACH;
    #[cfg(test)]
    pub(crate) const NETWORK_DOWN: i32 = ws::WSAENETDOWN;
    #[cfg(test)]
    pub(crate) const TIMED_OUT: i32 = ws::WSAETIMEDOUT;
    #[cfg(test)]
    pub(crate) const ADDRESS_NOT_AVAILABLE: i32 = ws::WSAEADDRNOTAVAIL;
    #[cfg(test)]
    pub(crate) const ACCESS_DENIED: i32 = ws::WSAEACCES;
    #[cfg(test)]
    pub(crate) const INVALID_ARGUMENT: i32 = ws::WSAEINVAL;
}

/// Claude: none. Windows answers on all of 127.0.0.0/8 but assigns only
/// 127.0.0.1, and a datagram sent from 127.0.0.2 never arrives.
#[cfg(test)]
pub(crate) fn second_loopback() -> Option<Ipv4Addr> {
    None
}
