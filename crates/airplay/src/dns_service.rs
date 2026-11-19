use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use windows_sys::Win32::Foundation::ERROR_CANCELLED;
use windows_sys::Win32::NetworkManagement::Dns::{
    DNS_QUERY_REQUEST_VERSION1, DNS_RECORDW, DNS_SERVICE_BROWSE_REQUEST,
    DNS_SERVICE_BROWSE_REQUEST_0, DNS_SERVICE_CANCEL, DNS_SERVICE_INSTANCE,
    DNS_SERVICE_RESOLVE_REQUEST, DNS_TYPE_A, DNS_TYPE_AAAA, DNS_TYPE_PTR, DNS_TYPE_SRV,
    DNS_TYPE_TEXT, DnsFree, DnsFreeRecordList, DnsServiceBrowse, DnsServiceBrowseCancel,
    DnsServiceFreeInstance, DnsServiceResolve,
};

use crate::browse::{Event, InstanceKey, Kind, Shared};
use crate::receiver::Txt;

const DNS_REQUEST_PENDING: i32 = 9506;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Claude: how often the browses start over. A running browse queries only
/// as it starts: it reports goodbyes, but not services announced after it
/// started, and it refreshes nothing it found. Starting over reports what
/// Windows has cached at once and queries the network again, which finds new
/// receivers and refreshes the records of those still there before their TTL
/// lapses.
const REQUERY: Duration = Duration::from_secs(15);

/// Claude: browses until `stop`, reporting to `shared`, and starts over when
/// Windows' DNS-SD fails.
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

/// Claude: what a DNS-SD callback reports, sent to the browsing thread.
enum Reply {
    Records(Kind, Vec<Record>),
    BrowseFailed(Kind, u32),
    Resolved(InstanceKey, Result<Resolution, u32>),
}

struct Record {
    name: String,
    ttl: u32,
    data: Data,
}

enum Data {
    Pointer(String),
    Service { host: String, port: u16 },
    Text(Vec<String>),
    Address(IpAddr),
}

struct Resolution {
    host: String,
    port: u16,
    txt: Txt,
    addresses: Vec<IpAddr>,
    interface: u32,
}

/// Claude: one service instance as the browses and resolves have reported
/// it, each record kept until its TTL lapses.
struct Instance {
    expires: Instant,
    service: Option<(String, u16)>,
    txt: Option<Txt>,
    addresses: HashMap<IpAddr, Instant>,
    /// Claude: the interface a resolve reported the instance on, which scopes
    /// its link-local IPv6 addresses; the browses report none.
    interface: Option<u32>,
    resolving: bool,
    resolve_started: Option<Instant>,
    reported: Option<(String, u16, Txt)>,
    reported_addresses: Vec<(IpAddr, u32)>,
}

impl Instance {
    fn new(expires: Instant) -> Instance {
        Instance {
            expires,
            service: None,
            txt: None,
            addresses: HashMap::new(),
            interface: None,
            resolving: false,
            resolve_started: None,
            reported: None,
            reported_addresses: Vec::new(),
        }
    }

    /// Claude: a new host's addresses are not known yet.
    fn set_service(&mut self, host: &str, port: u16) {
        if self
            .host()
            .is_some_and(|known| !known.eq_ignore_ascii_case(host))
        {
            self.addresses.clear();
        }
        self.service = Some((host.to_owned(), port));
    }

    fn host(&self) -> Option<&str> {
        self.service.as_ref().map(|(host, _)| host.as_str())
    }

    fn scoped(&self, address: IpAddr) -> Option<(IpAddr, u32)> {
        match address {
            IpAddr::V4(_) => Some((address, 0)),
            IpAddr::V6(ip) if ip.is_unicast_link_local() => Some((address, self.interface?)),
            IpAddr::V6(_) => Some((address, 0)),
        }
    }

    /// Claude: reports what changed since the last report.
    fn report(&mut self, key: &InstanceKey, events: &mut Vec<Event>) {
        let (Some((host, port)), Some(txt)) = (&self.service, &self.txt) else {
            return;
        };
        let resolved = (host.clone(), *port, txt.clone());
        let mut addresses: Vec<(IpAddr, u32)> = self
            .addresses
            .keys()
            .filter_map(|address| self.scoped(*address))
            .collect();
        addresses.sort();
        let host_changed = self
            .reported
            .as_ref()
            .is_none_or(|(reported, _, _)| *reported != resolved.0);
        if self.reported.as_ref() != Some(&resolved) {
            events.push(Event::Resolved {
                key: key.clone(),
                host: resolved.0.clone(),
                port: resolved.1,
                txt: resolved.2.clone(),
            });
            self.reported = Some(resolved);
        }
        if host_changed {
            self.reported_addresses.clear();
        }
        for (address, _) in &self.reported_addresses {
            if !addresses.iter().any(|(a, _)| a == address) {
                events.push(Event::AddressRemoved {
                    key: key.clone(),
                    address: *address,
                });
            }
        }
        for &(address, scope) in &addresses {
            if !self.reported_addresses.contains(&(address, scope)) {
                events.push(Event::AddressAdded {
                    key: key.clone(),
                    address,
                    scope,
                });
            }
        }
        self.reported_addresses = addresses;
    }
}

/// Claude: a browse per service type, restarted every `REQUERY`, a resolve
/// for each instance the browses report without a host or an address, and
/// the instances found.
struct Browsing {
    sender: Sender<Reply>,
    inbox: mpsc::Receiver<Reply>,
    browses: Vec<Browse>,
    queried_at: Instant,
    instances: HashMap<InstanceKey, Instance>,
}

impl Browsing {
    fn start() -> Result<Browsing, String> {
        let (sender, inbox) = mpsc::channel();
        let browses = Kind::ALL
            .into_iter()
            .map(|kind| Browse::start(kind, sender.clone()))
            .collect::<Result<_, _>>()?;
        Ok(Browsing {
            sender,
            inbox,
            browses,
            queried_at: Instant::now(),
            instances: HashMap::new(),
        })
    }

    fn run(&mut self, shared: &Shared, stop: &AtomicBool) -> Result<(), String> {
        let mut events = Vec::new();
        while !stop.load(Ordering::Acquire) {
            match self.inbox.recv_timeout(POLL_INTERVAL) {
                Ok(reply) => {
                    self.handle(reply, &mut events)?;
                    while let Ok(reply) = self.inbox.try_recv() {
                        self.handle(reply, &mut events)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => unreachable!("the browsing holds a sender"),
            }
            let now = Instant::now();
            self.expire(now, &mut events);
            if !events.is_empty() {
                shared.apply(events.drain(..));
            }
            if now >= self.queried_at + REQUERY {
                for browse in &mut self.browses {
                    browse.restart(self.sender.clone())?;
                }
                self.queried_at = now;
            }
        }
        Ok(())
    }

    fn handle(&mut self, reply: Reply, events: &mut Vec<Event>) -> Result<(), String> {
        match reply {
            Reply::Records(kind, records) => self.apply(kind, &records, events),
            Reply::BrowseFailed(kind, code) => {
                return Err(format!(
                    "browsing {} failed (error {code})",
                    kind.service_type()
                ));
            }
            Reply::Resolved(key, resolved) => {
                let Some(instance) = self.instances.get_mut(&key) else {
                    return Ok(());
                };
                instance.resolving = false;
                match resolved {
                    Ok(resolution) => {
                        let expires = instance.expires;
                        instance.set_service(&resolution.host, resolution.port);
                        instance.txt = Some(resolution.txt);
                        instance.interface =
                            (resolution.interface != 0).then_some(resolution.interface);
                        for address in resolution.addresses {
                            instance.addresses.insert(address, expires);
                        }
                        instance.report(&key, events);
                    }
                    Err(code) => debug!(?key, code, "AirPlay resolve failed"),
                }
            }
        }
        Ok(())
    }

    /// Claude: applies one browse callback's records: the pointers to `kind`'s
    /// instances, and the service, text and address records of instances
    /// known, whichever records came with them.
    fn apply(&mut self, kind: Kind, records: &[Record], events: &mut Vec<Event>) {
        let now = Instant::now();
        let service_type = format!("{}.local", kind.service_type());
        for record in records {
            let Data::Pointer(target) = &record.data else {
                continue;
            };
            if !record.name.eq_ignore_ascii_case(&service_type) {
                continue;
            }
            let Some(name) = instance_name(target, &service_type) else {
                continue;
            };
            let key = InstanceKey {
                kind,
                name: name.to_owned(),
                interface: 0,
            };
            if record.ttl == 0 {
                if self.instances.remove(&key).is_some() {
                    debug!(?key, "AirPlay service gone");
                    events.push(Event::Removed(key));
                }
                continue;
            }
            let expires = now + Duration::from_secs(u64::from(record.ttl));
            match self.instances.get_mut(&key) {
                Some(instance) => instance.expires = expires,
                None => {
                    debug!(?key, "AirPlay service found");
                    self.instances.insert(key, Instance::new(expires));
                }
            }
        }

        let mut touched = Vec::new();
        for (key, instance) in &mut self.instances {
            let full = format!("{}.{}.local", key.name, key.kind.service_type());
            let mut changed = false;
            for record in records {
                if record.ttl == 0 || !record.name.eq_ignore_ascii_case(&full) {
                    continue;
                }
                match &record.data {
                    Data::Service { host, port } => {
                        instance.set_service(host, *port);
                        changed = true;
                    }
                    Data::Text(strings) => {
                        instance.txt = Some(Txt::from_strings(strings.iter().map(String::as_str)));
                        changed = true;
                    }
                    _ => {}
                }
            }
            for record in records {
                let Data::Address(address) = record.data else {
                    continue;
                };
                if !instance
                    .host()
                    .is_some_and(|host| host.eq_ignore_ascii_case(&record.name))
                {
                    continue;
                }
                if record.ttl == 0 {
                    instance.addresses.remove(&address);
                } else {
                    let expires = now + Duration::from_secs(u64::from(record.ttl));
                    instance.addresses.insert(address, expires);
                }
                changed = true;
            }
            if changed {
                touched.push(key.clone());
            }
        }
        for key in touched {
            if let Some(instance) = self.instances.get_mut(&key) {
                instance.report(&key, events);
            }
        }
        self.resolve_incomplete();
    }

    /// Claude: resolves each instance the browses left without a host, text
    /// or address, one resolve at a time per instance and at most one per
    /// `REQUERY`.
    fn resolve_incomplete(&mut self) {
        let now = Instant::now();
        for (key, instance) in &mut self.instances {
            let complete = instance.service.is_some()
                && instance.txt.is_some()
                && !instance.addresses.is_empty();
            let recent = instance
                .resolve_started
                .is_some_and(|started| now < started + REQUERY);
            if complete || instance.resolving || recent {
                continue;
            }
            instance.resolve_started = Some(now);
            match resolve(key, self.sender.clone()) {
                Ok(()) => instance.resolving = true,
                Err(code) => warn!(?key, code, "could not resolve an AirPlay service"),
            }
        }
    }

    fn expire(&mut self, now: Instant, events: &mut Vec<Event>) {
        let mut gone = Vec::new();
        for (key, instance) in &mut self.instances {
            if now >= instance.expires {
                gone.push(key.clone());
                continue;
            }
            let before = instance.addresses.len();
            instance.addresses.retain(|_, expires| now < *expires);
            if instance.addresses.len() != before {
                instance.report(key, events);
            }
        }
        for key in gone {
            debug!(?key, "AirPlay service expired");
            self.instances.remove(&key);
            events.push(Event::Removed(key));
        }
        self.resolve_incomplete();
    }
}

fn instance_name<'a>(target: &'a str, service_type: &str) -> Option<&'a str> {
    let split = target.len().checked_sub(service_type.len() + 1)?;
    let (name, suffix) = (target.get(..split)?, target.get(split..)?);
    (suffix.starts_with('.') && suffix[1..].eq_ignore_ascii_case(service_type)).then_some(name)
}

/// Claude: what a browse's callbacks point at. The browse's last callback,
/// reporting it cancelled, frees it.
struct BrowseContext {
    kind: Kind,
    sender: Sender<Reply>,
    _query: Vec<u16>,
}

/// Claude: one running `DnsServiceBrowse`.
struct Browse {
    kind: Kind,
    cancel: Option<Box<DNS_SERVICE_CANCEL>>,
}

impl Browse {
    fn start(kind: Kind, sender: Sender<Reply>) -> Result<Browse, String> {
        let query = wide(&format!("{}.local", kind.service_type()));
        let context = Box::into_raw(Box::new(BrowseContext {
            kind,
            sender,
            _query: query,
        }));
        let request = DNS_SERVICE_BROWSE_REQUEST {
            Version: DNS_QUERY_REQUEST_VERSION1,
            InterfaceIndex: 0,
            QueryName: unsafe { (*context)._query.as_ptr() },
            Anonymous: DNS_SERVICE_BROWSE_REQUEST_0 {
                pBrowseCallback: Some(on_browse),
            },
            pQueryContext: context.cast(),
        };
        let mut cancel = Box::new(DNS_SERVICE_CANCEL::default());
        let code = unsafe { DnsServiceBrowse(&request, &mut *cancel) };
        if code != DNS_REQUEST_PENDING {
            drop(unsafe { Box::from_raw(context) });
            return Err(format!(
                "could not browse {} (error {code})",
                kind.service_type()
            ));
        }
        Ok(Browse {
            kind,
            cancel: Some(cancel),
        })
    }

    fn restart(&mut self, sender: Sender<Reply>) -> Result<(), String> {
        self.stop()?;
        *self = Browse::start(self.kind, sender)?;
        Ok(())
    }

    /// Claude: cancels the browse. When the cancel fails, the browse's
    /// context is left allocated, since a callback may still use it.
    fn stop(&mut self) -> Result<(), String> {
        let Some(cancel) = self.cancel.take() else {
            return Ok(());
        };
        let code = unsafe { DnsServiceBrowseCancel(&*cancel) };
        if code != 0 {
            return Err(format!(
                "could not stop browsing {} (error {code})",
                self.kind.service_type()
            ));
        }
        Ok(())
    }
}

impl Drop for Browse {
    fn drop(&mut self) {
        if let Err(reason) = self.stop() {
            warn!(%reason, "AirPlay browse left running");
        }
    }
}

unsafe extern "system" fn on_browse(
    status: u32,
    context: *const c_void,
    records: *const DNS_RECORDW,
) {
    let parsed = unsafe { parse_records(records) };
    if !records.is_null() {
        unsafe { DnsFree(records.cast(), DnsFreeRecordList) };
    }
    if status == ERROR_CANCELLED {
        drop(unsafe { Box::from_raw(context.cast_mut().cast::<BrowseContext>()) });
        return;
    }
    let context = unsafe { &*context.cast::<BrowseContext>() };
    let reply = if status == 0 {
        Reply::Records(context.kind, parsed)
    } else {
        Reply::BrowseFailed(context.kind, status)
    };
    let _ = context.sender.send(reply);
}

unsafe fn parse_records(mut cursor: *const DNS_RECORDW) -> Vec<Record> {
    let mut records = Vec::new();
    while let Some(record) = unsafe { cursor.as_ref() } {
        let data = unsafe {
            match record.wType {
                DNS_TYPE_PTR => Some(Data::Pointer(text(record.Data.PTR.pNameHost))),
                DNS_TYPE_SRV => Some(Data::Service {
                    host: text(record.Data.SRV.pNameTarget),
                    port: record.Data.SRV.wPort,
                }),
                DNS_TYPE_TEXT => {
                    let strings = ptr::addr_of!((*cursor).Data.TXT.pStringArray).cast::<*mut u16>();
                    Some(Data::Text(
                        (0..record.Data.TXT.dwStringCount as usize)
                            .map(|n| text(*strings.add(n)))
                            .collect(),
                    ))
                }
                DNS_TYPE_A => Some(Data::Address(IpAddr::V4(Ipv4Addr::from(
                    record.Data.A.IpAddress.to_ne_bytes(),
                )))),
                DNS_TYPE_AAAA => Some(Data::Address(IpAddr::V6(Ipv6Addr::from(
                    record.Data.AAAA.Ip6Address.IP6Byte,
                )))),
                _ => None,
            }
        };
        if let Some(data) = data {
            records.push(Record {
                name: unsafe { text(record.pName) },
                ttl: record.dwTtl,
                data,
            });
        }
        cursor = record.pNext;
    }
    records
}

/// Claude: what a resolve's one callback takes over and frees.
struct ResolveContext {
    key: InstanceKey,
    sender: Sender<Reply>,
    _query: Vec<u16>,
    _cancel: DNS_SERVICE_CANCEL,
}

/// Claude: starts resolving `key`, whose one callback reports to `sender`
/// whether or not the browsing still listens. It is never cancelled, so its
/// context is freed by that callback alone.
fn resolve(key: &InstanceKey, sender: Sender<Reply>) -> Result<(), i32> {
    let name = format!("{}.{}.local", key.name, key.kind.service_type());
    let context = Box::into_raw(Box::new(ResolveContext {
        key: key.clone(),
        sender,
        _query: wide(&name),
        _cancel: DNS_SERVICE_CANCEL::default(),
    }));
    let request = DNS_SERVICE_RESOLVE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1,
        InterfaceIndex: 0,
        QueryName: unsafe { (*context)._query.as_mut_ptr() },
        pResolveCompletionCallback: Some(on_resolve),
        pQueryContext: context.cast(),
    };
    let code = unsafe { DnsServiceResolve(&request, &raw mut (*context)._cancel) };
    if code != DNS_REQUEST_PENDING {
        drop(unsafe { Box::from_raw(context) });
        return Err(code);
    }
    Ok(())
}

unsafe extern "system" fn on_resolve(
    status: u32,
    context: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    let context = unsafe { Box::from_raw(context.cast_mut().cast::<ResolveContext>()) };
    let resolved = match unsafe { instance.as_ref() } {
        Some(found) if status == 0 => Ok(unsafe { resolution(found) }),
        _ => Err(status),
    };
    if !instance.is_null() {
        unsafe { DnsServiceFreeInstance(instance) };
    }
    let _ = context
        .sender
        .send(Reply::Resolved(context.key.clone(), resolved));
}

unsafe fn resolution(instance: &DNS_SERVICE_INSTANCE) -> Resolution {
    let strings: Vec<String> = (0..instance.dwPropertyCount as usize)
        .map(|n| unsafe {
            let key = text(*instance.keys.add(n));
            let value = text(*instance.values.add(n));
            if value.is_empty() {
                key
            } else {
                format!("{key}={value}")
            }
        })
        .collect();
    let mut addresses = Vec::new();
    if let Some(ip) = unsafe { instance.ip4Address.as_ref() } {
        addresses.push(IpAddr::V4(Ipv4Addr::from(ip.to_ne_bytes())));
    }
    if let Some(ip) = unsafe { instance.ip6Address.as_ref() } {
        addresses.push(IpAddr::V6(Ipv6Addr::from(unsafe { ip.IP6Byte })));
    }
    Resolution {
        host: unsafe { text(instance.pszHostName) },
        port: instance.wPort,
        txt: Txt::from_strings(strings.iter().map(String::as_str)),
        addresses,
        interface: instance.dwInterfaceIndex,
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

unsafe fn text(string: *const u16) -> String {
    if string.is_null() {
        return String::new();
    }
    let mut len = 0;
    while unsafe { *string.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(string, len) })
}
