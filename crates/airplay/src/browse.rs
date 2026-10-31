use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::dns_sd;
use crate::receiver::{Service, Txt};
use crate::turns::Turns;
use crate::{Receiver, ReceiverId};

/// Claude: how long a new browser waits for receivers already on the network
/// to answer before it reports what it found.
const WARM_UP: Duration = Duration::from_secs(1);
const ABORT_CHECK: Duration = Duration::from_millis(20);

/// Claude: a live view of the AirPlay receivers on the local network, kept by
/// browsing `_raop._tcp` and `_airplay._tcp` through the system's mDNSResponder.
/// A receiver advertising both services is one receiver. Streams started
/// through one browser take turns at each receiver.
pub struct Browser {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl Browser {
    /// Claude: starts browsing and returns at once.
    pub fn start() -> Self {
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));
        let spawned = thread::Builder::new().name("airplay-browse".into()).spawn({
            let (shared, stop) = (shared.clone(), stop.clone());
            move || dns_sd::browse(&shared, &stop)
        });
        if let Err(e) = spawned {
            shared.fail(format!("could not start browsing ({e})"));
        }
        Browser { shared, stop }
    }

    /// Claude: a live snapshot of the receivers found and resolved so far, taken
    /// once the browser has run for a bounded warm-up. Fails when browsing
    /// failed, so that no receivers is never a failure in disguise.
    pub fn receivers(&self) -> Result<Vec<Receiver>, String> {
        let ready =
            |state: &State, now| (now >= state.ready_at).then(|| state.registry.receivers());
        Ok(self
            .shared
            .wait_for(ready, |state| state.ready_at, &|| false)?
            .unwrap_or_default())
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Claude: a browser that learns only what a test tells it.
    #[cfg(test)]
    pub(crate) fn offline() -> Browser {
        Browser {
            shared: Arc::new(Shared::new()),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Claude: what the browsing thread has learned, shared with the `Browser` and
/// its streams.
pub(crate) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    pub(crate) turns: Arc<Turns>,
}

struct State {
    registry: Registry,
    failure: Option<String>,
    ready_at: Instant,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            state: Mutex::new(State {
                registry: Registry::default(),
                failure: None,
                ready_at: Instant::now() + WARM_UP,
            }),
            changed: Condvar::new(),
            turns: Arc::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claude: browsing works again, from scratch.
    pub(crate) fn restart(&self) {
        let mut state = self.lock();
        state.registry = Registry::default();
        state.failure = None;
        state.ready_at = Instant::now() + WARM_UP;
        self.changed.notify_all();
    }

    pub(crate) fn fail(&self, reason: String) {
        let mut state = self.lock();
        state.registry = Registry::default();
        state.failure = Some(reason);
        self.changed.notify_all();
    }

    pub(crate) fn apply(&self, events: impl IntoIterator<Item = Event>) {
        let mut state = self.lock();
        for event in events {
            state.registry.apply(event);
        }
        self.changed.notify_all();
    }

    /// Claude: waits up to `window` for the receiver to be found and resolved;
    /// `Ok(None)` means it was not found in time, or `abort` turned true.
    pub(crate) fn find(
        &self,
        id: &ReceiverId,
        window: Duration,
        abort: &dyn Fn() -> bool,
    ) -> Result<Option<Receiver>, String> {
        let deadline = Instant::now() + window;
        let found = |state: &State, _| state.registry.receivers().into_iter().find(|r| r.id == *id);
        self.wait_for(found, |_| deadline, abort)
    }

    /// Claude: waits until `found` finds something, browsing fails, the deadline
    /// passes or `abort` turns true.
    fn wait_for<T>(
        &self,
        found: impl Fn(&State, Instant) -> Option<T>,
        deadline: impl Fn(&State) -> Instant,
        abort: &dyn Fn() -> bool,
    ) -> Result<Option<T>, String> {
        let mut state = self.lock();
        loop {
            if let Some(reason) = &state.failure {
                return Err(reason.clone());
            }
            let now = Instant::now();
            if let Some(found) = found(&state, now) {
                return Ok(Some(found));
            }
            let deadline = deadline(&state);
            if now >= deadline || abort() {
                return Ok(None);
            }
            state = self
                .changed
                .wait_timeout(state, (deadline - now).min(ABORT_CHECK))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Kind {
    Raop,
    AirPlay,
}

impl Kind {
    pub(crate) const ALL: [Kind; 2] = [Kind::Raop, Kind::AirPlay];

    pub(crate) fn service_type(self) -> &'static str {
        match self {
            Kind::Raop => "_raop._tcp",
            Kind::AirPlay => "_airplay._tcp",
        }
    }
}

/// Claude: one advertisement of a service instance, as seen on one interface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct InstanceKey {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) interface: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Resolved {
        key: InstanceKey,
        host: String,
        port: u16,
        txt: Txt,
    },
    AddressAdded {
        key: InstanceKey,
        address: IpAddr,
        scope: u32,
    },
    AddressRemoved {
        key: InstanceKey,
        address: IpAddr,
    },
    Removed(InstanceKey),
}

/// Claude: the resolved advertisements, from which receivers are derived.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    instances: HashMap<InstanceKey, Resolution>,
}

#[derive(Debug)]
struct Resolution {
    host: String,
    port: u16,
    txt: Txt,
    addresses: Vec<SocketAddr>,
}

impl Registry {
    pub(crate) fn apply(&mut self, event: Event) {
        match event {
            Event::Resolved {
                key,
                host,
                port,
                txt,
            } => {
                let resolution = self.instances.entry(key).or_insert_with(|| Resolution {
                    host: host.clone(),
                    port,
                    txt: Txt::default(),
                    addresses: Vec::new(),
                });
                if resolution.host != host {
                    resolution.addresses.clear();
                    resolution.host = host;
                }
                resolution.port = port;
                resolution.txt = txt;
            }
            Event::AddressAdded {
                key,
                address,
                scope,
            } => {
                if let Some(resolution) = self.instances.get_mut(&key) {
                    let address = match address {
                        IpAddr::V6(ip) => SocketAddr::V6(SocketAddrV6::new(ip, 0, 0, scope)),
                        IpAddr::V4(_) => SocketAddr::new(address, 0),
                    };
                    if !resolution.addresses.contains(&address) {
                        resolution.addresses.push(address);
                    }
                }
            }
            Event::AddressRemoved { key, address } => {
                if let Some(resolution) = self.instances.get_mut(&key) {
                    resolution.addresses.retain(|a| a.ip() != address);
                }
            }
            Event::Removed(key) => {
                self.instances.remove(&key);
            }
        }
    }

    pub(crate) fn receivers(&self) -> Vec<Receiver> {
        let mut services: BTreeMap<(Kind, &str), Service> = BTreeMap::new();
        let mut keys: Vec<&InstanceKey> = self.instances.keys().collect();
        keys.sort();
        for key in keys {
            let resolution = &self.instances[key];
            if resolution.addresses.is_empty() {
                continue;
            }
            let service = services
                .entry((key.kind, &key.name))
                .or_insert_with(|| Service {
                    addresses: Vec::new(),
                    txt: resolution.txt.clone(),
                });
            for address in &resolution.addresses {
                let mut address = *address;
                address.set_port(resolution.port);
                if !service.addresses.contains(&address) {
                    service.addresses.push(address);
                }
            }
        }

        let mut receivers: BTreeMap<ReceiverId, Receiver> = BTreeMap::new();
        for ((kind, name), mut service) in services {
            service
                .addresses
                .sort_by_key(|a| (a.is_ipv6(), a.ip().is_loopback()));
            let (id, name) = match kind {
                Kind::Raop => match ReceiverId::from_raop_instance(name) {
                    Some(found) => found,
                    None => continue,
                },
                Kind::AirPlay => match service.txt.get("deviceid").and_then(|id| id.parse().ok()) {
                    Some(id) => (id, name),
                    None => continue,
                },
            };
            let receiver = receivers.entry(id.clone()).or_insert_with(|| Receiver {
                id,
                name: name.to_owned(),
                model: None,
                raop: None,
                airplay: None,
            });
            match kind {
                Kind::Raop => {
                    receiver.model = receiver
                        .model
                        .take()
                        .or_else(|| service.txt.get("am").map(str::to_owned));
                    receiver.raop = Some(service);
                }
                Kind::AirPlay => {
                    receiver.name = name.to_owned();
                    receiver.model = service
                        .txt
                        .get("model")
                        .map(str::to_owned)
                        .or(receiver.model.take());
                    receiver.airplay = Some(service);
                }
            }
        }
        let mut receivers: Vec<Receiver> = receivers.into_values().collect();
        receivers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        receivers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::tests::{MOVE_2_AIRPLAY, MOVE_2_RAOP, SHAIRPORT_SYNC_RAOP, txt};

    const RAOP: &str = "C43875045E80@Move 2";

    fn key(kind: Kind, name: &str, interface: u32) -> InstanceKey {
        InstanceKey {
            kind,
            name: name.into(),
            interface,
        }
    }

    fn resolved(kind: Kind, name: &str, interface: u32, record: &str) -> Event {
        Event::Resolved {
            key: key(kind, name, interface),
            host: "Sonos-C43875045E80.local.".into(),
            port: 7000,
            txt: txt(record),
        }
    }

    fn address(kind: Kind, name: &str, interface: u32, address: &str) -> Event {
        Event::AddressAdded {
            key: key(kind, name, interface),
            address: address.parse().unwrap(),
            scope: interface,
        }
    }

    fn move_2(registry: &mut Registry) {
        for event in [
            resolved(Kind::Raop, RAOP, 4, MOVE_2_RAOP),
            address(Kind::Raop, RAOP, 4, "fe80::c638:75ff:fe04:5e80"),
            address(Kind::Raop, RAOP, 4, "192.168.1.20"),
            resolved(Kind::AirPlay, "Move 2", 4, MOVE_2_AIRPLAY),
            address(Kind::AirPlay, "Move 2", 4, "192.168.1.20"),
        ] {
            registry.apply(event);
        }
    }

    #[test]
    fn a_receiver_advertising_both_services_is_one_receiver() {
        let mut registry = Registry::default();
        move_2(&mut registry);
        registry.apply(resolved(Kind::Raop, RAOP, 17, MOVE_2_RAOP));
        registry.apply(address(Kind::Raop, RAOP, 17, "192.168.1.20"));
        let receivers = registry.receivers();
        assert_eq!(receivers.len(), 1);
        let receiver = &receivers[0];
        assert_eq!(receiver.id.as_str(), "C4:38:75:04:5E:80");
        assert_eq!(
            (receiver.name.as_str(), receiver.model.as_deref()),
            ("Move 2", Some("Move 2"))
        );
        assert_eq!(receiver.support(), Ok(()));
        let raop = receiver.raop.as_ref().unwrap();
        let expected: Vec<SocketAddr> = vec![
            "192.168.1.20:7000".parse().unwrap(),
            "[fe80::c638:75ff:fe04:5e80%4]:7000".parse().unwrap(),
        ];
        assert_eq!(raop.addresses, expected);
        assert!(receiver.airplay.is_some());
    }

    #[test]
    fn receivers_keep_what_is_still_advertised_and_resolved() {
        let mut registry = Registry::default();
        move_2(&mut registry);
        registry.apply(Event::Removed(key(Kind::AirPlay, "Move 2", 4)));
        let receivers = registry.receivers();
        assert_eq!(receivers.len(), 1);
        assert!(receivers[0].airplay.is_none());
        assert_eq!(receivers[0].model.as_deref(), Some("Move 2"));

        registry.apply(Event::AddressRemoved {
            key: key(Kind::Raop, RAOP, 4),
            address: "192.168.1.20".parse().unwrap(),
        });
        assert_eq!(
            registry.receivers()[0]
                .raop
                .as_ref()
                .unwrap()
                .addresses
                .len(),
            1
        );
        registry.apply(Event::Resolved {
            key: key(Kind::Raop, RAOP, 4),
            host: "renamed.local.".into(),
            port: 7000,
            txt: txt(MOVE_2_RAOP),
        });
        assert!(
            registry.receivers().is_empty(),
            "a new host's addresses are not known yet"
        );
        registry.apply(address(Kind::Raop, RAOP, 4, "192.168.1.21"));
        assert_eq!(registry.receivers().len(), 1);
        registry.apply(Event::Removed(key(Kind::Raop, RAOP, 4)));
        assert!(registry.receivers().is_empty());
    }

    #[test]
    fn services_without_a_device_id_are_left_out() {
        let mut registry = Registry::default();
        registry.apply(resolved(Kind::Raop, "no id here", 4, SHAIRPORT_SYNC_RAOP));
        registry.apply(address(Kind::Raop, "no id here", 4, "192.168.1.30"));
        registry.apply(resolved(Kind::AirPlay, "Speaker", 4, "model=X"));
        registry.apply(address(Kind::AirPlay, "Speaker", 4, "192.168.1.31"));
        assert!(registry.receivers().is_empty());
    }

    #[test]
    fn the_browser_reports_receivers_found_after_its_warm_up() {
        let browser = Browser::offline();
        let started = Instant::now();
        assert_eq!(browser.receivers(), Ok(vec![]));
        assert!(started.elapsed() >= WARM_UP);

        let id: ReceiverId = "C4:38:75:04:5E:80".parse().unwrap();
        let shared = browser.shared().clone();
        let late = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            shared.apply([
                resolved(Kind::Raop, RAOP, 4, MOVE_2_RAOP),
                address(Kind::Raop, RAOP, 4, "192.168.1.20"),
            ]);
        });
        let found = browser
            .shared()
            .find(&id, Duration::from_secs(2), &|| false)
            .unwrap();
        assert_eq!(found.map(|r| r.id), Some(id.clone()));
        late.join().unwrap();
        assert_eq!(browser.receivers().unwrap().len(), 1);

        let other: ReceiverId = "02:B5:11:B2:28:53".parse().unwrap();
        assert_eq!(
            browser
                .shared()
                .find(&other, Duration::from_millis(50), &|| false),
            Ok(None)
        );

        browser
            .shared()
            .fail("mDNSResponder failed (error -65563)".into());
        assert_eq!(
            browser.receivers(),
            Err("mDNSResponder failed (error -65563)".into())
        );
        assert!(
            browser
                .shared()
                .find(&id, Duration::from_millis(50), &|| false)
                .is_err()
        );
        browser.shared().restart();
        assert_eq!(browser.receivers(), Ok(vec![]));
    }
}
