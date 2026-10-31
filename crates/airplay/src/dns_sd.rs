use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use crate::browse::{Event, InstanceKey, Kind, Shared};
use crate::receiver::Txt;

type ServiceRef = *mut c_void;
type Flags = u32;
type ErrorCode = i32;

const FLAG_ADD: Flags = 0x2;
const FLAG_SHARE_CONNECTION: Flags = 0x4000;
const PROTOCOL_IPV4: u32 = 0x1;
const PROTOCOL_IPV6: u32 = 0x2;
const ERROR_NO_SUCH_RECORD: ErrorCode = -65554;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

type BrowseReply = extern "C" fn(
    ServiceRef,
    Flags,
    u32,
    ErrorCode,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveReply = extern "C" fn(
    ServiceRef,
    Flags,
    u32,
    ErrorCode,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);
type AddrInfoReply = extern "C" fn(
    ServiceRef,
    Flags,
    u32,
    ErrorCode,
    *const c_char,
    *const libc::sockaddr,
    u32,
    *mut c_void,
);

unsafe extern "C" {
    fn DNSServiceCreateConnection(service: *mut ServiceRef) -> ErrorCode;
    fn DNSServiceRefSockFD(service: ServiceRef) -> c_int;
    fn DNSServiceProcessResult(service: ServiceRef) -> ErrorCode;
    fn DNSServiceRefDeallocate(service: ServiceRef);
    fn DNSServiceBrowse(
        service: *mut ServiceRef,
        flags: Flags,
        interface: u32,
        service_type: *const c_char,
        domain: *const c_char,
        reply: BrowseReply,
        context: *mut c_void,
    ) -> ErrorCode;
    fn DNSServiceResolve(
        service: *mut ServiceRef,
        flags: Flags,
        interface: u32,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        reply: ResolveReply,
        context: *mut c_void,
    ) -> ErrorCode;
    fn DNSServiceGetAddrInfo(
        service: *mut ServiceRef,
        flags: Flags,
        interface: u32,
        protocols: u32,
        host: *const c_char,
        reply: AddrInfoReply,
        context: *mut c_void,
    ) -> ErrorCode;
}

/// Claude: browses until `stop`, reporting to `shared`, and starts over when
/// mDNSResponder fails.
pub(crate) fn browse(shared: &Shared, stop: &AtomicBool) {
    loop {
        let reason = match Browsing::start() {
            Ok(mut browsing) => {
                shared.restart();
                match browsing.run(shared, stop) {
                    Ok(()) => return,
                    Err(reason) => reason,
                }
            }
            Err(reason) => reason,
        };
        warn!(%reason, "AirPlay browsing failed; retrying");
        shared.fail(reason);
        let mut waited = Duration::ZERO;
        while waited < RETRY_INTERVAL {
            if stop.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(POLL_INTERVAL);
            waited += POLL_INTERVAL;
        }
    }
}

/// Claude: what an mDNSResponder callback reports, queued until
/// `DNSServiceProcessResult` returns.
enum Reply {
    Browsed {
        kind: Kind,
        name: String,
        interface: u32,
        domain: String,
        added: bool,
    },
    BrowseFailed(ErrorCode),
    Resolved {
        key: InstanceKey,
        host: String,
        port: u16,
        txt: Txt,
    },
    Address {
        key: InstanceKey,
        address: IpAddr,
        scope: u32,
        added: bool,
    },
    OperationFailed {
        key: InstanceKey,
        code: ErrorCode,
    },
}

type Replies = RefCell<Vec<Reply>>;

struct Context {
    replies: *const Replies,
    target: Target,
}

enum Target {
    Browse(Kind),
    Instance(InstanceKey),
}

/// Claude: one operation on the shared connection, with the context its
/// callbacks point at.
struct Operation {
    service: ServiceRef,
    _context: Box<Context>,
}

impl Drop for Operation {
    fn drop(&mut self) {
        unsafe { DNSServiceRefDeallocate(self.service) };
    }
}

/// Claude: one connection to mDNSResponder with a browse per service type, a
/// resolve per advertisement and an address lookup per resolved host.
struct Browsing {
    connection: ServiceRef,
    replies: Box<Replies>,
    browses: Vec<Operation>,
    resolves: HashMap<InstanceKey, Operation>,
    lookups: HashMap<InstanceKey, (String, Operation)>,
}

impl Drop for Browsing {
    fn drop(&mut self) {
        self.lookups.clear();
        self.resolves.clear();
        self.browses.clear();
        unsafe { DNSServiceRefDeallocate(self.connection) };
    }
}

impl Browsing {
    fn start() -> Result<Browsing, String> {
        let mut connection = ptr::null_mut();
        let code = unsafe { DNSServiceCreateConnection(&mut connection) };
        if code != 0 {
            return Err(format!("could not reach mDNSResponder (error {code})"));
        }
        let mut browsing = Browsing {
            connection,
            replies: Box::default(),
            browses: Vec::new(),
            resolves: HashMap::new(),
            lookups: HashMap::new(),
        };
        for kind in Kind::ALL {
            let operation = browsing
                .operation(Target::Browse(kind), |service, context| {
                    let service_type = CString::new(kind.service_type()).unwrap_or_default();
                    unsafe {
                        DNSServiceBrowse(
                            service,
                            FLAG_SHARE_CONNECTION,
                            0,
                            service_type.as_ptr(),
                            ptr::null(),
                            on_browse,
                            context,
                        )
                    }
                })
                .map_err(|code| {
                    format!("could not browse {} (error {code})", kind.service_type())
                })?;
            browsing.browses.push(operation);
        }
        Ok(browsing)
    }

    fn operation(
        &self,
        target: Target,
        start: impl FnOnce(*mut ServiceRef, *mut c_void) -> ErrorCode,
    ) -> Result<Operation, ErrorCode> {
        let context = Box::new(Context {
            replies: &*self.replies,
            target,
        });
        let mut service = self.connection;
        let code = start(&mut service, &*context as *const Context as *mut c_void);
        if code != 0 {
            return Err(code);
        }
        Ok(Operation {
            service,
            _context: context,
        })
    }

    fn run(&mut self, shared: &Shared, stop: &AtomicBool) -> Result<(), String> {
        let fd = unsafe { DNSServiceRefSockFD(self.connection) };
        if fd < 0 {
            return Err("mDNSResponder gave no socket".into());
        }
        while !stop.load(Ordering::Acquire) {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, POLL_INTERVAL.as_millis() as c_int) };
            if ready <= 0 {
                continue;
            }
            let code = unsafe { DNSServiceProcessResult(self.connection) };
            if code != 0 {
                return Err(format!("mDNSResponder failed (error {code})"));
            }
            let replies: Vec<Reply> = self.replies.borrow_mut().drain(..).collect();
            let mut events = Vec::new();
            for reply in replies {
                self.handle(reply, &mut events)?;
            }
            if !events.is_empty() {
                shared.apply(events);
            }
        }
        Ok(())
    }

    fn handle(&mut self, reply: Reply, events: &mut Vec<Event>) -> Result<(), String> {
        match reply {
            Reply::Browsed {
                kind,
                name,
                interface,
                domain,
                added: true,
            } => {
                let key = InstanceKey {
                    kind,
                    name,
                    interface,
                };
                if self.resolves.contains_key(&key) {
                    return Ok(());
                }
                debug!(?key, "AirPlay service found");
                let resolve = self.operation(Target::Instance(key.clone()), |service, context| {
                    let name = CString::new(key.name.as_str()).unwrap_or_default();
                    let service_type = CString::new(kind.service_type()).unwrap_or_default();
                    let domain = CString::new(domain).unwrap_or_default();
                    unsafe {
                        DNSServiceResolve(
                            service,
                            FLAG_SHARE_CONNECTION,
                            interface,
                            name.as_ptr(),
                            service_type.as_ptr(),
                            domain.as_ptr(),
                            on_resolve,
                            context,
                        )
                    }
                });
                match resolve {
                    Ok(resolve) => {
                        self.resolves.insert(key, resolve);
                    }
                    Err(code) => warn!(?key, code, "could not resolve an AirPlay service"),
                }
            }
            Reply::Browsed {
                kind,
                name,
                interface,
                added: false,
                ..
            } => {
                let key = InstanceKey {
                    kind,
                    name,
                    interface,
                };
                debug!(?key, "AirPlay service gone");
                self.resolves.remove(&key);
                self.lookups.remove(&key);
                events.push(Event::Removed(key));
            }
            Reply::BrowseFailed(code) => return Err(format!("browsing failed (error {code})")),
            Reply::Resolved {
                key,
                host,
                port,
                txt,
            } => {
                if self
                    .lookups
                    .get(&key)
                    .is_none_or(|(looked_up, _)| *looked_up != host)
                {
                    self.lookups.remove(&key);
                    let lookup =
                        self.operation(Target::Instance(key.clone()), |service, context| {
                            let host = CString::new(host.as_str()).unwrap_or_default();
                            unsafe {
                                DNSServiceGetAddrInfo(
                                    service,
                                    FLAG_SHARE_CONNECTION,
                                    key.interface,
                                    PROTOCOL_IPV4 | PROTOCOL_IPV6,
                                    host.as_ptr(),
                                    on_address,
                                    context,
                                )
                            }
                        });
                    match lookup {
                        Ok(lookup) => {
                            self.lookups.insert(key.clone(), (host.clone(), lookup));
                        }
                        Err(code) => {
                            warn!(?key, %host, code, "could not look up an AirPlay receiver's address")
                        }
                    }
                }
                events.push(Event::Resolved {
                    key,
                    host,
                    port,
                    txt,
                });
            }
            Reply::Address {
                key,
                address,
                scope,
                added: true,
            } => {
                events.push(Event::AddressAdded {
                    key,
                    address,
                    scope,
                });
            }
            Reply::Address {
                key,
                address,
                added: false,
                ..
            } => {
                events.push(Event::AddressRemoved { key, address });
            }
            Reply::OperationFailed {
                code: ERROR_NO_SUCH_RECORD,
                ..
            } => {}
            Reply::OperationFailed { key, code } => {
                debug!(?key, code, "AirPlay resolve or lookup failed");
            }
        }
        Ok(())
    }
}

fn text(string: *const c_char) -> String {
    if string.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(string) }
        .to_string_lossy()
        .into_owned()
}

fn push(context: *mut c_void, reply: impl FnOnce(&Target) -> Option<Reply>) {
    let context = unsafe { &*(context as *const Context) };
    if let Some(reply) = reply(&context.target) {
        unsafe { &*context.replies }.borrow_mut().push(reply);
    }
}

extern "C" fn on_browse(
    _: ServiceRef,
    flags: Flags,
    interface: u32,
    code: ErrorCode,
    name: *const c_char,
    _: *const c_char,
    domain: *const c_char,
    context: *mut c_void,
) {
    push(context, |target| {
        let Target::Browse(kind) = *target else {
            return None;
        };
        if code != 0 {
            return Some(Reply::BrowseFailed(code));
        }
        Some(Reply::Browsed {
            kind,
            name: text(name),
            interface,
            domain: text(domain),
            added: flags & FLAG_ADD != 0,
        })
    });
}

extern "C" fn on_resolve(
    _: ServiceRef,
    _: Flags,
    _: u32,
    code: ErrorCode,
    _: *const c_char,
    host: *const c_char,
    port: u16,
    txt_len: u16,
    txt: *const u8,
    context: *mut c_void,
) {
    push(context, |target| {
        let Target::Instance(key) = target else {
            return None;
        };
        let key = key.clone();
        if code != 0 {
            return Some(Reply::OperationFailed { key, code });
        }
        let txt = if txt.is_null() {
            Txt::default()
        } else {
            Txt::parse(unsafe { std::slice::from_raw_parts(txt, usize::from(txt_len)) })
        };
        Some(Reply::Resolved {
            key,
            host: text(host),
            port: u16::from_be(port),
            txt,
        })
    });
}

extern "C" fn on_address(
    _: ServiceRef,
    flags: Flags,
    interface: u32,
    code: ErrorCode,
    _: *const c_char,
    address: *const libc::sockaddr,
    _: u32,
    context: *mut c_void,
) {
    push(context, |target| {
        let Target::Instance(key) = target else {
            return None;
        };
        let key = key.clone();
        let parsed = if code != 0 || address.is_null() {
            None
        } else {
            socket_address(address, interface)
        };
        Some(match parsed {
            Some((address, scope)) => Reply::Address {
                key,
                address,
                scope,
                added: flags & FLAG_ADD != 0,
            },
            None => Reply::OperationFailed { key, code },
        })
    });
}

fn socket_address(address: *const libc::sockaddr, interface: u32) -> Option<(IpAddr, u32)> {
    match i32::from(unsafe { (*address).sa_family }) {
        libc::AF_INET => {
            let address = unsafe { &*(address as *const libc::sockaddr_in) };
            Some((
                IpAddr::V4(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr))),
                0,
            ))
        }
        libc::AF_INET6 => {
            let address = unsafe { &*(address as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(address.sin6_addr.s6_addr);
            let scope = match address.sin6_scope_id {
                0 if ip.is_unicast_link_local() => interface,
                scope => scope,
            };
            Some((IpAddr::V6(ip), scope))
        }
        _ => None,
    }
}
