use crate::packet::AUDIO_PACKET_LEN;

/// Claude: the packets a stream sent most recently, for retransmission. Packets
/// are numbered from the stream's first; their sequence numbers wrap.
pub(crate) struct Backlog {
    slots: Box<[Slot]>,
    latest: Option<(u64, u16)>,
}

struct Slot {
    packet: Option<u64>,
    bytes: [u8; AUDIO_PACKET_LEN],
}

impl Backlog {
    pub(crate) fn new(capacity: usize) -> Backlog {
        assert!(capacity > 0 && capacity < usize::from(u16::MAX));
        Backlog {
            slots: (0..capacity)
                .map(|_| Slot {
                    packet: None,
                    bytes: [0; AUDIO_PACKET_LEN],
                })
                .collect(),
            latest: None,
        }
    }

    pub(crate) fn push(&mut self, packet: u64, seq: u16, bytes: &[u8; AUDIO_PACKET_LEN]) {
        let len = self.slots.len() as u64;
        let slot = &mut self.slots[(packet % len) as usize];
        slot.packet = Some(packet);
        slot.bytes = *bytes;
        self.latest = Some((packet, seq));
    }

    pub(crate) fn get(&self, seq: u16) -> Option<&[u8; AUDIO_PACKET_LEN]> {
        let (latest, latest_seq) = self.latest?;
        let behind = u64::from(latest_seq.wrapping_sub(seq));
        let len = self.slots.len() as u64;
        if behind >= len {
            return None;
        }
        let packet = latest.checked_sub(behind)?;
        let slot = &self.slots[(packet % len) as usize];
        (slot.packet == Some(packet)).then_some(&slot.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(packet: u64) -> [u8; AUDIO_PACKET_LEN] {
        let mut bytes = [0; AUDIO_PACKET_LEN];
        bytes[..8].copy_from_slice(&packet.to_be_bytes());
        bytes
    }

    #[test]
    fn keeps_the_latest_packets_across_the_sequence_wrap() {
        let mut backlog = Backlog::new(8);
        let first_seq = 65_530u16;
        for packet in 0..20u64 {
            if packet == 15 {
                continue;
            }
            backlog.push(
                packet,
                first_seq.wrapping_add(packet as u16),
                &bytes(packet),
            );
        }
        let seq = |packet: u64| first_seq.wrapping_add(packet as u16);
        assert_eq!(backlog.get(seq(19)), Some(&bytes(19)));
        assert_eq!(backlog.get(seq(12)), Some(&bytes(12)));
        assert_eq!(backlog.get(seq(11)), None);
        assert_eq!(backlog.get(seq(6)), None);
        assert_eq!(backlog.get(seq(15)), None);
        assert_eq!(backlog.get(seq(20)), None);
        assert_eq!(Backlog::new(4).get(0), None);
    }
}
