use socket2::SockAddr;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Instant;

pub(super) fn send(
    socket: &UdpSocket,
    local: SocketAddr,
    destination: SocketAddr,
    bytes: &[u8],
) -> io::Result<Instant> {
    let address = SockAddr::from(destination);
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0_usize; 16];
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_name = address.as_ptr().cast_mut().cast();
        message.msg_namelen = address.len();
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control) as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        let length = match local {
            SocketAddr::V4(local) => {
                let info = libc::in_pktinfo {
                    ipi_ifindex: 0,
                    ipi_spec_dst: libc::in_addr {
                        s_addr: u32::from_ne_bytes(local.ip().octets()),
                    },
                    ipi_addr: libc::in_addr { s_addr: 0 },
                };
                (*header).cmsg_level = libc::IPPROTO_IP;
                (*header).cmsg_type = libc::IP_PKTINFO;
                std::ptr::write_unaligned(libc::CMSG_DATA(header).cast(), info);
                std::mem::size_of_val(&info) as u32
            }
            SocketAddr::V6(local) => {
                let info = libc::in6_pktinfo {
                    ipi6_addr: libc::in6_addr {
                        s6_addr: local.ip().octets(),
                    },
                    ipi6_ifindex: local.scope_id(),
                };
                (*header).cmsg_level = libc::IPPROTO_IPV6;
                (*header).cmsg_type = libc::IPV6_PKTINFO;
                std::ptr::write_unaligned(libc::CMSG_DATA(header).cast(), info);
                std::mem::size_of_val(&info) as u32
            }
        };
        (*header).cmsg_len = libc::CMSG_LEN(length) as _;
        message.msg_controllen = libc::CMSG_SPACE(length) as _;
        let sent = Instant::now();
        let result = libc::sendmsg(socket.as_raw_fd(), &message, 0);
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial PTP datagram",
            ));
        }
        Ok(sent)
    }
}
