use std::net::SocketAddr;

/// Claude: a receiver's AirPlay device ID, such as `C4:38:75:04:5E:80`, stable
/// across renames and restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReceiverId(String);

impl ReceiverId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Claude: splits a `_raop._tcp` instance name, `C43875045E80@Move 2`,
    /// into the device ID and the receiver's name.
    pub(crate) fn from_raop_instance(instance: &str) -> Option<(ReceiverId, &str)> {
        let (hex, name) = instance.split_once('@')?;
        Some((hex.parse().ok()?, name))
    }
}

impl std::str::FromStr for ReceiverId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("{s:?} is not an AirPlay device ID such as C4:38:75:04:5E:80");
        let hex: String = s.chars().filter(|c| *c != ':').collect();
        let separated = s.contains(':');
        if hex.len() != 12
            || !hex.chars().all(|c| c.is_ascii_hexdigit())
            || (separated && s.split(':').any(|byte| byte.len() != 2))
        {
            return Err(invalid());
        }
        let bytes: Vec<&str> = (0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect();
        Ok(ReceiverId(bytes.join(":").to_ascii_uppercase()))
    }
}

impl std::fmt::Display for ReceiverId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Claude: an AirPlay receiver found on the network, with each service it
/// advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver {
    pub id: ReceiverId,
    pub name: String,
    pub model: Option<String>,
    pub(crate) raop: Option<Service>,
    pub(crate) airplay: Option<Service>,
}

impl Receiver {
    /// Claude: whether a stream can play to it, or why not.
    pub fn support(&self) -> Result<(), String> {
        self.protocol().map(|_| ())
    }

    /// Claude: the protocol a stream speaks to it unless one is forced, or why
    /// no stream can play to it: AirPlay 2 when it pairs transiently and takes
    /// AirPlay audio, and AirPlay 1 otherwise.
    pub fn protocol(&self) -> Result<Protocol, String> {
        let airplay2 = self.service(Protocol::AirPlay2);
        let airplay1 = self.service(Protocol::AirPlay1);
        match (airplay2, airplay1) {
            (Ok(_), _) => Ok(Protocol::AirPlay2),
            (_, Ok(_)) => Ok(Protocol::AirPlay1),
            (Err(two), Err(one)) => Err(match (self.airplay.is_some(), self.raop.is_some()) {
                (true, false) => two,
                (false, true) => one,
                _ if two == one => one,
                _ => format!("{two}, and {one}"),
            }),
        }
    }

    /// Claude: the service a stream speaking `protocol` connects to, or why it
    /// cannot.
    pub(crate) fn service(&self, protocol: Protocol) -> Result<&Service, String> {
        self.access()?;
        match protocol {
            Protocol::AirPlay1 => self.raop_service(),
            Protocol::AirPlay2 => self.airplay2_service(),
        }
    }

    /// Claude: whether a stream that picks its protocol should give browsing
    /// time to resolve this receiver's `_airplay` service: its `_raop` record
    /// says, by the same feature bits, that it takes AirPlay 2, and a receiver
    /// advertising both may resolve either first.
    pub(crate) fn awaits_airplay2(&self) -> bool {
        self.airplay.is_none()
            && self
                .raop
                .as_ref()
                .is_some_and(|raop| raop.txt.features("ft") & AIRPLAY_2 == AIRPLAY_2)
    }

    /// Claude: whether `protocol`'s service has only gone from browsing, as
    /// while a receiver announces itself again, with nothing it still
    /// advertises turning a stream away.
    pub(crate) fn missing(&self, protocol: Protocol) -> bool {
        let service = match protocol {
            Protocol::AirPlay1 => &self.raop,
            Protocol::AirPlay2 => &self.airplay,
        };
        service.is_none() && self.access().is_ok()
    }

    fn access(&self) -> Result<(), String> {
        let raop = self.raop.as_ref().map(|s| &s.txt);
        let flags = raop.map_or(0, |txt| txt.flags("sf"))
            | self.airplay.as_ref().map_or(0, |s| s.txt.flags("flags"));
        if matches!(raop.and_then(|txt| txt.get("pw")), Some("true" | "1"))
            || flags & PASSWORD_REQUIRED != 0
        {
            return Err(
                "it requires a password; turn the password off in its AirPlay settings".into(),
            );
        }
        if flags & (PIN_REQUIRED | PAIRING_REQUIRED) != 0 {
            return Err(
                "it requires pairing with a PIN; allow AirPlay without a PIN in its settings"
                    .into(),
            );
        }
        Ok(())
    }

    fn raop_service(&self) -> Result<&Service, String> {
        let Some(raop) = &self.raop else {
            return Err("it does not offer AirPlay 1 (RAOP)".into());
        };
        let txt = &raop.txt;
        if !txt.list("tp").any(|t| t.eq_ignore_ascii_case("UDP")) {
            return Err(format!(
                "it does not take RAOP audio over UDP (tp={})",
                txt.get("tp").unwrap_or("")
            ));
        }
        if !txt.list("et").any(|e| e == "0") {
            return Err(format!(
                "it accepts only encrypted audio (et={})",
                txt.get("et").unwrap_or("")
            ));
        }
        if !txt.list("cn").any(|c| c == "1") {
            return Err(format!(
                "it does not accept Apple Lossless audio (cn={})",
                txt.get("cn").unwrap_or("")
            ));
        }
        Ok(raop)
    }

    /// Claude: AirPlay 2 needs a receiver that takes AirPlay audio, pairs
    /// transiently and encrypts, as OwnTone decides from the same feature bit
    /// since receivers that pair without encrypting failed it, and whose access
    /// control lets any sender pair.
    fn airplay2_service(&self) -> Result<&Service, String> {
        let Some(airplay) = &self.airplay else {
            return Err("it does not offer AirPlay 2".into());
        };
        let txt = &airplay.txt;
        let features = txt.features("features");
        let advertised = || txt.get("features").unwrap_or("");
        if features & AUDIO == 0 {
            return Err(format!(
                "it does not take AirPlay 2 audio (features={})",
                advertised()
            ));
        }
        if features & CORE_UTILS_PAIRING_AND_ENCRYPTION == 0 {
            return Err(format!(
                "it does not pair transiently over AirPlay 2 (features={})",
                advertised()
            ));
        }
        if txt.get("acl").is_some_and(|acl| acl != "0") {
            return Err(format!(
                "it lets only some senders pair over AirPlay 2 (acl={})",
                txt.get("acl").unwrap_or("")
            ));
        }
        Ok(airplay)
    }
}

/// Claude: the protocol a stream speaks to its receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// Claude: RAOP, unencrypted and unauthenticated.
    AirPlay1,
    /// Claude: realtime audio, encrypted after transient pairing.
    AirPlay2,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Protocol::AirPlay1 => "AirPlay 1",
            Protocol::AirPlay2 => "AirPlay 2",
        })
    }
}

const AUDIO: u64 = 1 << 9;
const CORE_UTILS_PAIRING_AND_ENCRYPTION: u64 = 1 << 48;
const AIRPLAY_2: u64 = AUDIO | CORE_UTILS_PAIRING_AND_ENCRYPTION;
const PIN_REQUIRED: u64 = 0x8;
const PASSWORD_REQUIRED: u64 = 0x80;
const PAIRING_REQUIRED: u64 = 0x200;

/// Claude: one advertised service: where it listens, as resolved on each
/// interface (IPv6 link-local addresses keep their scope), and its TXT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Service {
    pub(crate) addresses: Vec<SocketAddr>,
    pub(crate) txt: Txt,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Txt(pub(crate) Vec<(String, String)>);

impl Txt {
    /// Claude: parses DNS-SD TXT record data: length-prefixed `key=value`
    /// strings, where a key without `=` has an empty value.
    pub(crate) fn parse(mut data: &[u8]) -> Txt {
        let mut entries = Vec::new();
        while let Some((&len, rest)) = data.split_first() {
            let len = usize::from(len).min(rest.len());
            let entry = String::from_utf8_lossy(&rest[..len]);
            data = &rest[len..];
            if entry.is_empty() {
                continue;
            }
            let (key, value) = entry.split_once('=').unwrap_or((&entry, ""));
            entries.push((key.to_owned(), value.to_owned()));
        }
        Txt(entries)
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn list(&self, key: &str) -> impl Iterator<Item = &str> {
        self.get(key)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    fn flags(&self, key: &str) -> u64 {
        self.list(key)
            .filter_map(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .fold(0, |acc, v| acc | v)
    }

    /// Claude: a feature bit field, `features` in `_airplay` and `ft` in
    /// `_raop`, written as its lower 32 bits and, after a comma, its upper 32.
    fn features(&self, key: &str) -> u64 {
        let mut halves = self.list(key).map(|v| {
            u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0) & 0xFFFF_FFFF
        });
        let low = halves.next().unwrap_or(0);
        let high = halves.next().unwrap_or(0);
        high << 32 | low
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Claude: a TXT record as dns-sd prints it; a word without `=` belongs to
    /// the value before it, as in `am=Move 2`.
    pub(crate) fn txt(record: &str) -> Txt {
        let mut entries: Vec<(String, String)> = Vec::new();
        for word in record.split(' ') {
            match (word.split_once('='), entries.last_mut()) {
                (Some((key, value)), _) => entries.push((key.to_owned(), value.to_owned())),
                (None, Some((_, value))) => *value = format!("{value} {word}"),
                (None, None) => entries.push((word.to_owned(), String::new())),
            }
        }
        Txt(entries)
    }

    pub(crate) const MOVE_2_RAOP: &str = "cn=0,1 da=true et=0,4 ft=0x445F8A00,0x801C340 fv=p20.97.2-81220 md=0,1,2 am=Move 2 sf=0x4 tp=UDP vn=65537 vs=366.0 pk=027d3f0dd349d365f2c31728df281b728473713ac5973c289cc0d3449d6cdd74";
    pub(crate) const MOVE_2_AIRPLAY: &str = "acl=0 deviceid=C4:38:75:04:5E:80 features=0x445F8A00,0x801C340 fex=AIpfREDDAQgQIQ rsf=0x0 fv=p20.97.2-81220 flags=0x4 model=Move 2 manufacturer=Sonos serialNumber=C4-38-75-04-5E-80:3 protovers=1.1 srcvers=366.0 pi=74ac565e-7c7f-4b41-b257-47630cb249e3 gid=74ac565e-7c7f-4b41-b257-47630cb249e3 gcgl=0 pk=027d3f0dd349d365f2c31728df281b728473713ac5973c289cc0d3449d6cdd74 bat=BC100";
    pub(crate) const CITATION_AIRPLAY: &str = "acl=0 deviceid=C4:30:18:6E:41:2D features=0x445F8A00,0x1C340 flags=0x4 model=HK Citation MultiBeam 1100 protovers=1.1 srcvers=366.0";
    pub(crate) const SHAIRPORT_SYNC_RAOP: &str = "sf=0x4 fv=5.6.2 am=ShairportSync vs=105.1 tp=TCP,UDP vn=65537 md=0,1,2 ss=16 sr=44100 da=true sv=false et=0,1 ek=1 cn=0,1 ch=2 txtvers=1 pw=false";

    fn receiver(raop: Option<&str>, airplay: Option<&str>) -> Receiver {
        let service = |record: &str| Service {
            addresses: vec!["192.0.2.1:7000".parse().unwrap()],
            txt: txt(record),
        };
        Receiver {
            id: "C4:38:75:04:5E:80".parse().unwrap(),
            name: "Move 2".into(),
            model: None,
            raop: raop.map(service),
            airplay: airplay.map(service),
        }
    }

    #[test]
    fn device_ids_parse_from_either_form_and_print_canonically() {
        let id: ReceiverId = "c4:38:75:04:5e:80".parse().unwrap();
        assert_eq!(id.as_str(), "C4:38:75:04:5E:80");
        assert_eq!("C43875045E80".parse::<ReceiverId>().unwrap(), id);
        assert!("C4:38:75:04:5E".parse::<ReceiverId>().is_err());
        assert!("C4:38:75:04:5E:8G".parse::<ReceiverId>().is_err());
        assert!("C43:875:045:E80".parse::<ReceiverId>().is_err());
    }

    #[test]
    fn raop_instance_names_carry_the_device_id_and_name() {
        let (id, name) = ReceiverId::from_raop_instance("C43875045E80@Move 2").unwrap();
        assert_eq!(id.as_str(), "C4:38:75:04:5E:80");
        assert_eq!(name, "Move 2");
        let (_, name) = ReceiverId::from_raop_instance("1ACFBE65CF31@Alfred’s @ Home").unwrap();
        assert_eq!(name, "Alfred’s @ Home");
        assert!(ReceiverId::from_raop_instance("Move 2").is_none());
        assert!(ReceiverId::from_raop_instance("C438@Move 2").is_none());
    }

    #[test]
    fn txt_records_parse_from_wire_format() {
        let wire = b"\x06cn=0,1\x02da\x00\x06et=0,4\x09sf=";
        let parsed = Txt::parse(wire);
        assert_eq!(parsed.get("cn"), Some("0,1"));
        assert_eq!(parsed.get("DA"), Some(""));
        assert_eq!(parsed.list("et").collect::<Vec<_>>(), ["0", "4"]);
        assert_eq!(parsed.get("sf"), Some(""));
    }

    #[test]
    fn move_2_and_shairport_sync_are_supported() {
        assert_eq!(
            receiver(Some(MOVE_2_RAOP), Some(MOVE_2_AIRPLAY)).support(),
            Ok(())
        );
        assert_eq!(receiver(Some(SHAIRPORT_SYNC_RAOP), None).support(), Ok(()));
    }

    #[test]
    fn the_citation_and_move_2_are_eligible_for_airplay_2_by_their_feature_bits() {
        for airplay in [MOVE_2_AIRPLAY, CITATION_AIRPLAY] {
            let receiver = receiver(None, Some(airplay));
            assert!(receiver.service(Protocol::AirPlay2).is_ok(), "{airplay}");
        }
        let reason = |airplay: &str| {
            receiver(Some(MOVE_2_RAOP), Some(airplay))
                .service(Protocol::AirPlay2)
                .unwrap_err()
        };
        let without = |bit: u32| {
            let features = 0x0001_C340_445F_8A00u64 & !(1 << bit);
            format!(
                "deviceid=C4:30:18:6E:41:2D features={:#X},{:#X} flags=0x4",
                features & 0xFFFF_FFFF,
                features >> 32
            )
        };
        assert!(reason(&without(9)).contains("audio"));
        assert!(reason(&without(48)).contains("transiently"));
        let system_pairing_only = "deviceid=C4:30:18:6E:41:2D features=0x200,0x800 flags=0x4";
        assert!(
            receiver(None, Some(system_pairing_only))
                .service(Protocol::AirPlay2)
                .is_err()
        );
        assert!(reason(&CITATION_AIRPLAY.replace("acl=0", "acl=1")).contains("acl=1"));
        assert!(reason(&CITATION_AIRPLAY.replace("flags=0x4", "flags=0x8")).contains("PIN"));
        assert!(
            receiver(Some(SHAIRPORT_SYNC_RAOP), None)
                .service(Protocol::AirPlay2)
                .unwrap_err()
                .contains("does not offer AirPlay 2")
        );
    }

    #[test]
    fn a_service_is_missing_only_when_gone_and_nothing_else_turns_a_stream_away() {
        let raop_only = receiver(Some(SHAIRPORT_SYNC_RAOP), None);
        assert!(raop_only.missing(Protocol::AirPlay2));
        assert!(!raop_only.missing(Protocol::AirPlay1));
        let pin = CITATION_AIRPLAY.replace("flags=0x4", "flags=0x8");
        assert!(!receiver(None, Some(&pin)).missing(Protocol::AirPlay1));
        assert!(receiver(None, Some(CITATION_AIRPLAY)).missing(Protocol::AirPlay1));
        let refusing = receiver(Some("cn=0,1 et=1 tp=UDP"), None);
        assert!(!refusing.missing(Protocol::AirPlay1));
    }

    #[test]
    fn features_read_their_lower_half_then_their_upper_half() {
        assert_eq!(
            txt("features=0x445F8A00,0x1C340").features("features"),
            0x0001_C340_445F_8A00
        );
        assert_eq!(txt("features=0x5A7FFFF7").features("features"), 0x5A7F_FFF7);
        assert_eq!(txt("x=1").features("features"), 0);
        assert_eq!(txt(MOVE_2_RAOP).features("ft"), 0x0801_C340_445F_8A00);
    }

    #[test]
    fn only_a_receiver_whose_raop_record_offers_airplay_2_awaits_its_airplay_service() {
        assert!(receiver(Some(MOVE_2_RAOP), None).awaits_airplay2());
        assert!(!receiver(Some(MOVE_2_RAOP), Some(MOVE_2_AIRPLAY)).awaits_airplay2());
        assert!(!receiver(Some(SHAIRPORT_SYNC_RAOP), None).awaits_airplay2());
        assert!(!receiver(None, Some(CITATION_AIRPLAY)).awaits_airplay2());
        let without_transient_pairing = MOVE_2_RAOP.replace("0x801C340", "0x800C340");
        assert!(!receiver(Some(&without_transient_pairing), None).awaits_airplay2());
    }

    #[test]
    fn a_receiver_eligible_for_airplay_2_gets_it_and_any_other_airplay_1() {
        let protocol = |raop, airplay| receiver(raop, airplay).protocol();
        assert_eq!(
            protocol(Some(MOVE_2_RAOP), Some(MOVE_2_AIRPLAY)),
            Ok(Protocol::AirPlay2)
        );
        assert_eq!(
            protocol(None, Some(CITATION_AIRPLAY)),
            Ok(Protocol::AirPlay2)
        );
        assert_eq!(
            protocol(Some(SHAIRPORT_SYNC_RAOP), None),
            Ok(Protocol::AirPlay1)
        );
        let without_transient_pairing =
            "deviceid=C4:30:18:6E:41:2D features=0x445F8A00,0x0 flags=0x4";
        assert_eq!(
            protocol(Some(MOVE_2_RAOP), Some(without_transient_pairing)),
            Ok(Protocol::AirPlay1)
        );
        assert!(
            protocol(
                None,
                Some(&CITATION_AIRPLAY.replace("flags=0x4", "flags=0x80"))
            )
            .unwrap_err()
            .contains("password")
        );
    }

    #[test]
    fn unsupported_receivers_say_why() {
        let reason = |raop: Option<&str>, airplay: Option<&str>| {
            receiver(raop, airplay).support().unwrap_err()
        };
        assert!(
            reason(
                None,
                Some("deviceid=C4:30:18:6E:41:2D features=0x200 flags=0x4")
            )
            .contains("does not pair transiently")
        );
        assert_eq!(
            reason(
                Some("cn=0,1 et=0 tp=TCP"),
                Some("deviceid=C4:30:18:6E:41:2D features=0x200 flags=0x4")
            ),
            "it does not pair transiently over AirPlay 2 (features=0x200), and it does not take \
             RAOP audio over UDP (tp=TCP)"
        );
        assert!(reason(Some("cn=0,1 et=0 tp=TCP"), None).contains("UDP"));
        assert!(reason(Some("cn=0,1 et=1,3 tp=UDP"), None).contains("encrypted"));
        assert!(reason(Some("cn=0,1 tp=UDP"), None).contains("encrypted"));
        assert!(reason(Some("cn=0,2 et=0 tp=UDP"), None).contains("Apple Lossless"));
        assert!(reason(Some("cn=0,1 et=0 tp=UDP pw=true"), None).contains("password"));
        assert!(reason(Some("cn=0,1 et=0 tp=UDP sf=0x84"), None).contains("password"));
        assert!(reason(Some("cn=0,1 et=0 tp=UDP"), Some("flags=0x8")).contains("PIN"));
        assert!(reason(Some("cn=0,1 et=0 tp=UDP sf=0x204"), None).contains("PIN"));
    }
}
