use std::io;
#[cfg(test)]
use std::net::Ipv4Addr;
use std::net::UdpSocket;
pub(crate) use std::os::fd::{AsRawFd as AsRawSocket, RawFd as RawSocket};
use std::os::unix::net::UnixDatagram;

use super::{Interest, MOST_INTERESTS};

pub(super) fn raw(socket: &impl AsRawSocket) -> RawSocket {
    socket.as_raw_fd()
}

pub(super) fn wait(interests: &[Interest], ready: &mut [bool], ms: i32) -> io::Result<usize> {
    let mut fds = [libc::pollfd {
        fd: -1,
        events: 0,
        revents: 0,
    }; MOST_INTERESTS];
    let fds = &mut fds[..interests.len()];
    for (fd, interest) in fds.iter_mut().zip(interests) {
        fd.fd = interest.socket.unwrap_or(-1);
        fd.events = if interest.write {
            libc::POLLOUT
        } else {
            libc::POLLIN
        };
    }
    let count = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    for (ready, fd) in ready.iter_mut().zip(fds.iter()) {
        *ready = count > 0 && fd.revents & (fd.events | libc::POLLHUP | libc::POLLERR) != 0;
    }
    Ok(count as usize)
}

/// Claude: what one thread writes to wake another polling its pair.
pub(crate) type WakeSocket = UnixDatagram;

pub(crate) fn wake_pair() -> io::Result<(WakeSocket, WakeSocket)> {
    UnixDatagram::pair()
}

/// Claude: keeps ICMP errors for datagrams sent from `socket` out of its
/// receives. An unconnected socket gets none here already.
pub(crate) fn ignore_unreachable(_socket: &UdpSocket) -> io::Result<()> {
    Ok(())
}

/// Claude: whether a non-blocking connect failed only because it has yet to
/// finish.
pub(crate) fn connect_in_progress(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EINPROGRESS)
}

pub(crate) mod error {
    pub(crate) const HOST_DOWN: i32 = libc::EHOSTDOWN;
    #[cfg(test)]
    pub(crate) const CONNECTION_REFUSED: i32 = libc::ECONNREFUSED;
    #[cfg(test)]
    pub(crate) const HOST_UNREACHABLE: i32 = libc::EHOSTUNREACH;
    #[cfg(test)]
    pub(crate) const NETWORK_UNREACHABLE: i32 = libc::ENETUNREACH;
    #[cfg(test)]
    pub(crate) const NETWORK_DOWN: i32 = libc::ENETDOWN;
    #[cfg(test)]
    pub(crate) const TIMED_OUT: i32 = libc::ETIMEDOUT;
    #[cfg(test)]
    pub(crate) const ADDRESS_NOT_AVAILABLE: i32 = libc::EADDRNOTAVAIL;
    #[cfg(test)]
    pub(crate) const ACCESS_DENIED: i32 = libc::EACCES;
    #[cfg(test)]
    pub(crate) const INVALID_ARGUMENT: i32 = libc::EINVAL;
}

/// Claude: an IPv4 loopback address of this host other than 127.0.0.1, if
/// one is configured.
#[cfg(test)]
pub(crate) fn second_loopback() -> Option<Ipv4Addr> {
    let mut addresses = std::ptr::null_mut();
    assert_eq!(unsafe { libc::getifaddrs(&mut addresses) }, 0);
    let mut cursor = addresses;
    let mut local = None;
    while !cursor.is_null() {
        let interface = unsafe { &*cursor };
        if !interface.ifa_addr.is_null()
            && unsafe { (*interface.ifa_addr).sa_family } == libc::AF_INET as _
        {
            let address = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
            if ip.is_loopback() && ip != Ipv4Addr::LOCALHOST {
                local = Some(ip);
            }
        }
        cursor = interface.ifa_next;
    }
    unsafe { libc::freeifaddrs(addresses) };
    local
}
