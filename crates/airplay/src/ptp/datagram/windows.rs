use socket2::SockAddr;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::ptr;
use std::time::Instant;

use windows_sys::Win32::Networking::WinSock::{
    CMSGHDR, IN_ADDR, IN_ADDR_0, IN_PKTINFO, IN6_ADDR, IN6_ADDR_0, IN6_PKTINFO, IP_PKTINFO,
    IPPROTO_IP, IPPROTO_IPV6, IPV6_PKTINFO, SOCKET, SOCKET_ERROR, WSABUF, WSAMSG, WSASendMsg,
};

/// Claude: `WSA_CMSG_*` alignment: a control message's header and data each
/// start on a boundary of the header's own alignment.
const fn aligned(len: usize) -> usize {
    let align = align_of::<CMSGHDR>();
    (len + align - 1) & !(align - 1)
}

pub(in crate::ptp) fn send(
    socket: &UdpSocket,
    local: SocketAddr,
    destination: SocketAddr,
    bytes: &[u8],
) -> io::Result<Instant> {
    let address = SockAddr::from(destination);
    let mut buffer = WSABUF {
        len: bytes.len() as u32,
        buf: bytes.as_ptr().cast_mut(),
    };
    let mut control = [0_usize; 16];
    let data = aligned(size_of::<CMSGHDR>());
    let (level, kind, length) = match local {
        SocketAddr::V4(local) => {
            let info = IN_PKTINFO {
                ipi_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(local.ip().octets()),
                    },
                },
                ipi_ifindex: 0,
            };
            write(&mut control, data, info);
            (IPPROTO_IP, IP_PKTINFO, size_of_val(&info))
        }
        SocketAddr::V6(local) => {
            let info = IN6_PKTINFO {
                ipi6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: local.ip().octets(),
                    },
                },
                ipi6_ifindex: local.scope_id(),
            };
            write(&mut control, data, info);
            (IPPROTO_IPV6, IPV6_PKTINFO, size_of_val(&info))
        }
    };
    write(
        &mut control,
        0,
        CMSGHDR {
            cmsg_len: data + length,
            cmsg_level: level,
            cmsg_type: kind,
        },
    );
    let message = WSAMSG {
        name: address.as_ptr().cast_mut().cast(),
        namelen: address.len(),
        lpBuffers: &mut buffer,
        dwBufferCount: 1,
        Control: WSABUF {
            len: (data + aligned(length)) as u32,
            buf: control.as_mut_ptr().cast(),
        },
        dwFlags: 0,
    };
    let mut sent_bytes = 0;
    let sent = Instant::now();
    let result = unsafe {
        WSASendMsg(
            socket.as_raw_socket() as SOCKET,
            &message,
            0,
            &mut sent_bytes,
            ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    if sent_bytes as usize != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial PTP datagram",
        ));
    }
    Ok(sent)
}

fn write<T>(control: &mut [usize; 16], at: usize, value: T) {
    assert!(at + size_of::<T>() <= size_of_val(control));
    unsafe { ptr::write_unaligned(control.as_mut_ptr().cast::<u8>().add(at).cast(), value) };
}
