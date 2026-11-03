use super::ClockIdentity;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct PortIdentity {
    pub clock: ClockIdentity,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Timestamp {
    pub seconds: u64,
    pub nanos: u32,
}

impl Timestamp {
    pub const ZERO: Self = Self {
        seconds: 0,
        nanos: 0,
    };

    pub fn from_nanos(nanos: u64) -> Self {
        Self {
            seconds: nanos / 1_000_000_000,
            nanos: (nanos % 1_000_000_000) as u32,
        }
    }

    pub fn nanos(self) -> Option<u64> {
        self.seconds
            .checked_mul(1_000_000_000)?
            .checked_add(u64::from(self.nanos))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Dataset {
    pub priority1: u8,
    pub class: u8,
    pub accuracy: u8,
    pub variance: u16,
    pub priority2: u8,
    pub grandmaster: ClockIdentity,
    pub steps: u16,
}

impl Dataset {
    pub fn local(identity: ClockIdentity, priority1: u8) -> Self {
        Self {
            priority1,
            class: 248,
            accuracy: 0x21,
            variance: 17258,
            priority2: 247,
            grandmaster: identity,
            steps: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FollowInfo {
    pub rate_offset: i32,
    pub timebase: u16,
    pub phase_change: [u8; 12],
    pub frequency_change: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Tlv {
    Follow(FollowInfo),
    Interval {
        link: i8,
        sync: i8,
        announce: i8,
        flags: u8,
        reserved: u16,
    },
    Path(Vec<ClockIdentity>),
    Organization {
        oui: [u8; 3],
        subtype: [u8; 3],
        data: Vec<u8>,
    },
    Unknown {
        kind: u16,
        data: Vec<u8>,
    },
}

pub(super) fn apple_signaling() -> Vec<Tlv> {
    vec![
        Tlv::Organization {
            oui: [0, 0x0d, 0x93],
            subtype: [0, 0, 1],
            data: vec![0, 3, 0, 0, 0, 0, 0x27, 0x10, 0, 0, 0x27, 0x10, 0, 0, 0, 0],
        },
        Tlv::Organization {
            oui: [0, 0x0d, 0x93],
            subtype: [0, 0, 5],
            data: vec![
                0, 0x0f, 0, 0, 0, 0, 0, 0, 0x27, 0x10, 0, 0, 0x27, 0x10, 0, 0, 0x27, 0x10, 0, 0,
                0x27, 0x10, 0, 0, 0, 0,
            ],
        },
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Body {
    Sync(Timestamp),
    DelayReq(Timestamp),
    FollowUp(Timestamp),
    DelayResp {
        received: Timestamp,
        requester: PortIdentity,
    },
    Announce {
        origin: Timestamp,
        utc_offset: i16,
        reserved: u8,
        dataset: Dataset,
        time_source: u8,
    },
    Signaling {
        target: PortIdentity,
    },
}

impl Body {
    pub fn kind(&self) -> u8 {
        match self {
            Self::Sync(_) => 0,
            Self::DelayReq(_) => 1,
            Self::FollowUp(_) => 8,
            Self::DelayResp { .. } => 9,
            Self::Announce { .. } => 11,
            Self::Signaling { .. } => 12,
        }
    }

    pub fn event(&self) -> bool {
        self.kind() < 8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Message {
    pub source: PortIdentity,
    pub sequence: u16,
    pub flags: u16,
    pub correction: i64,
    pub control: u8,
    pub log_interval: i8,
    pub reserved: [u8; 4],
    pub body: Body,
    pub tlvs: Vec<Tlv>,
}

impl Message {
    pub fn new(source: PortIdentity, sequence: u16, log_interval: i8, body: Body) -> Self {
        let control = match body {
            Body::Sync(_) | Body::DelayReq(_) => 0,
            Body::FollowUp(_) => 2,
            Body::DelayResp { .. } => 3,
            _ => 5,
        };
        let flags = if matches!(body, Body::Sync(_)) {
            0x0608
        } else {
            0x0408
        };
        Self {
            source,
            sequence,
            flags,
            correction: 0,
            control,
            log_interval,
            reserved: [0; 4],
            body,
            tlvs: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0x10 | self.body.kind(), 2, 0, 0, 0, 0];
        out.extend(self.flags.to_be_bytes());
        out.extend(self.correction.to_be_bytes());
        out.extend(self.reserved);
        put_port(&mut out, self.source);
        out.extend(self.sequence.to_be_bytes());
        out.extend([self.control, self.log_interval as u8]);
        match &self.body {
            Body::Sync(t) | Body::DelayReq(t) | Body::FollowUp(t) => put_time(&mut out, *t),
            Body::DelayResp {
                received,
                requester,
            } => {
                put_time(&mut out, *received);
                put_port(&mut out, *requester);
            }
            Body::Signaling { target } => put_port(&mut out, *target),
            Body::Announce {
                origin,
                utc_offset,
                reserved,
                dataset: d,
                time_source,
            } => {
                put_time(&mut out, *origin);
                out.extend(utc_offset.to_be_bytes());
                out.extend([*reserved, d.priority1, d.class, d.accuracy]);
                out.extend(d.variance.to_be_bytes());
                out.push(d.priority2);
                out.extend(d.grandmaster.0);
                out.extend(d.steps.to_be_bytes());
                out.push(*time_source);
            }
        }
        for tlv in &self.tlvs {
            let (kind, data) = match tlv {
                Tlv::Follow(f) => {
                    let mut data = vec![0, 0x80, 0xc2, 0, 0, 1];
                    data.extend(f.rate_offset.to_be_bytes());
                    data.extend(f.timebase.to_be_bytes());
                    data.extend(f.phase_change);
                    data.extend(f.frequency_change.to_be_bytes());
                    (3, data)
                }
                Tlv::Interval {
                    link,
                    sync,
                    announce,
                    flags,
                    reserved,
                } => {
                    let mut data = vec![
                        0,
                        0x80,
                        0xc2,
                        0,
                        0,
                        2,
                        *link as u8,
                        *sync as u8,
                        *announce as u8,
                        *flags,
                    ];
                    data.extend(reserved.to_be_bytes());
                    (3, data)
                }
                Tlv::Organization { oui, subtype, data } => {
                    let mut value = Vec::new();
                    value.extend(oui);
                    value.extend(subtype);
                    value.extend(data);
                    (3, value)
                }
                Tlv::Path(clocks) => (8, clocks.iter().flat_map(|c| c.0).collect()),
                Tlv::Unknown { kind, data } => (*kind, data.clone()),
            };
            out.extend(kind.to_be_bytes());
            out.extend((data.len() as u16).to_be_bytes());
            out.extend(data);
        }
        let length = out.len() as u16;
        out[2..4].copy_from_slice(&length.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        let kind = r.u8()?;
        if kind >> 4 != 1 || r.u8()? != 2 || usize::from(r.u16()?) != bytes.len() || r.u16()? != 0 {
            return None;
        }
        let flags = r.u16()?;
        if flags & 0x0400 == 0 {
            return None;
        }
        let correction = i64::from_be_bytes(r.array()?);
        let reserved = r.array()?;
        let source = r.port()?;
        let sequence = r.u16()?;
        let control = r.u8()?;
        let log_interval = r.u8()? as i8;
        let body = match kind & 15 {
            0 => Body::Sync(r.time()?),
            1 => Body::DelayReq(r.time()?),
            8 => Body::FollowUp(r.time()?),
            9 => Body::DelayResp {
                received: r.time()?,
                requester: r.port()?,
            },
            12 => Body::Signaling { target: r.port()? },
            11 => Body::Announce {
                origin: r.time()?,
                utc_offset: r.u16()? as i16,
                reserved: r.u8()?,
                dataset: Dataset {
                    priority1: r.u8()?,
                    class: r.u8()?,
                    accuracy: r.u8()?,
                    variance: r.u16()?,
                    priority2: r.u8()?,
                    grandmaster: ClockIdentity(r.array()?),
                    steps: r.u16()?,
                },
                time_source: r.u8()?,
            },
            _ => return None,
        };
        let mut tlvs = Vec::new();
        while !r.0.is_empty() {
            let kind = r.u16()?;
            let length = usize::from(r.u16()?);
            if length % 2 != 0 {
                return None;
            }
            let data = r.take(length)?;
            let mut v = Reader(data);
            let tlv = match kind {
                3 => {
                    let oui = v.array()?;
                    let subtype = v.array()?;
                    match (oui, subtype) {
                        ([0, 0x80, 0xc2], [0, 0, 1]) => {
                            if length != 28 {
                                return None;
                            }
                            Tlv::Follow(FollowInfo {
                                rate_offset: i32::from_be_bytes(v.array()?),
                                timebase: v.u16()?,
                                phase_change: v.array()?,
                                frequency_change: i32::from_be_bytes(v.array()?),
                            })
                        }
                        ([0, 0x80, 0xc2], [0, 0, 2]) => {
                            if length != 12 {
                                return None;
                            }
                            Tlv::Interval {
                                link: v.u8()? as i8,
                                sync: v.u8()? as i8,
                                announce: v.u8()? as i8,
                                flags: v.u8()?,
                                reserved: v.u16()?,
                            }
                        }
                        _ => Tlv::Organization {
                            oui,
                            subtype,
                            data: v.0.to_vec(),
                        },
                    }
                }
                8 => {
                    if length % 8 != 0 {
                        return None;
                    }
                    let mut clocks = Vec::new();
                    while !v.0.is_empty() {
                        clocks.push(ClockIdentity(v.array()?));
                    }
                    Tlv::Path(clocks)
                }
                _ => Tlv::Unknown {
                    kind,
                    data: data.to_vec(),
                },
            };
            tlvs.push(tlv);
        }
        Some(Self {
            source,
            sequence,
            flags,
            correction,
            control,
            log_interval,
            reserved,
            body,
            tlvs,
        })
    }
}

fn put_port(out: &mut Vec<u8>, port: PortIdentity) {
    out.extend(port.clock.0);
    out.extend(port.port.to_be_bytes());
}
fn put_time(out: &mut Vec<u8>, time: Timestamp) {
    out.extend(&time.seconds.to_be_bytes()[2..]);
    out.extend(time.nanos.to_be_bytes());
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(length)?;
        self.0 = rest;
        Some(head)
    }
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.array()?))
    }
    fn port(&mut self) -> Option<PortIdentity> {
        Some(PortIdentity {
            clock: ClockIdentity(self.array()?),
            port: self.u16()?,
        })
    }
    fn time(&mut self) -> Option<Timestamp> {
        let mut seconds = [0; 8];
        seconds[2..].copy_from_slice(self.take(6)?);
        let nanos = u32::from_be_bytes(self.array()?);
        (nanos < 1_000_000_000).then_some(Timestamp {
            seconds: u64::from_be_bytes(seconds),
            nanos,
        })
    }
}

#[cfg(test)]
mod tests;
