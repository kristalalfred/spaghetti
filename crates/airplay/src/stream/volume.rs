use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::rtsp::Response;

pub(super) const CONTENT_TYPE: &str = "text/parameters";
const MUTED_DB: f32 = -144.0;
const QUIETEST_DB: f32 = -30.0;
const READ_EVERY: Duration = Duration::from_secs(2);

/// Claude: what the stream's owner asked the receiver to play at, and what
/// the receiver last said it plays at.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Volumes {
    pub(super) wanted: Option<f32>,
    pub(super) reported: Option<f32>,
}

/// Claude: AirPlay's volume in decibels for `level`, from 0, muted, to 1:
/// senders map their volume control linearly onto -30 to 0 dB, and send
/// -144 for muted.
pub(super) fn decibels(level: f32) -> f32 {
    if level <= 0.0 {
        MUTED_DB
    } else {
        (level.min(1.0) - 1.0) * -QUIETEST_DB
    }
}

/// Claude: the level a receiver plays at when it reports `db`.
fn level(db: f32) -> f32 {
    if db <= QUIETEST_DB {
        0.0
    } else {
        (1.0 - db / QUIETEST_DB).min(1.0)
    }
}

pub(super) fn set_body(level: f32) -> String {
    format!("volume: {:.6}\r\n", decibels(level))
}

pub(super) const GET_BODY: &str = "volume\r\n";

/// Claude: the volume in a GET_PARAMETER answer, as `volume: -15.000000`.
fn reported(body: &[u8]) -> Option<f32> {
    let text = std::str::from_utf8(body).ok()?;
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let db: f32 = (name.trim() == "volume").then(|| value.trim().parse().ok())??;
        db.is_finite().then(|| level(db))
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Ask {
    Set(f32),
    Get,
}

/// Claude: one volume request at a time on the session's RTSP connection:
/// the latest wanted volume once it differs from the one sent, otherwise a
/// reading of the receiver's volume `READ_EVERY` after the last request while
/// it answers them. A Sonos One answered a reading right after a change with
/// the volume from before it, so no reading follows a change sooner. A volume
/// request refused or unanswered never ends the stream.
pub(super) struct VolumeSync {
    sent: Option<f32>,
    pending: Option<(u32, Instant, Ask)>,
    next_read: Instant,
    reads: bool,
}

impl VolumeSync {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            sent: None,
            pending: None,
            next_read: now,
            reads: true,
        }
    }

    /// Claude: what to ask the receiver now, if anything, giving up on an
    /// answer later than `timeout`.
    pub(super) fn next(
        &mut self,
        wanted: Option<f32>,
        now: Instant,
        timeout: Duration,
    ) -> Option<Ask> {
        if let Some((_, at, ask)) = self.pending {
            if now.duration_since(at) <= timeout {
                return None;
            }
            self.pending = None;
            match ask {
                Ask::Set(level) => {
                    warn!(level, "the AirPlay receiver did not answer a volume change");
                    self.sent = Some(level);
                }
                Ask::Get => {
                    debug!(
                        "the AirPlay receiver did not answer a volume reading; no longer reading it"
                    );
                    self.reads = false;
                }
            }
        }
        match wanted {
            Some(level) if self.sent != Some(level) => Some(Ask::Set(level)),
            _ if self.reads && now >= self.next_read => Some(Ask::Get),
            _ => None,
        }
    }

    pub(super) fn sent(&mut self, cseq: u32, now: Instant, ask: Ask) {
        self.next_read = now + READ_EVERY;
        self.pending = Some((cseq, now, ask));
    }

    /// Claude: takes the answer to the pending request, and returns the
    /// receiver's volume when the answer says what it is.
    pub(super) fn answer(&mut self, response: &Response) -> Option<f32> {
        let (cseq, _, ask) = self.pending?;
        if response.cseq() != Some(cseq) {
            return None;
        }
        self.pending = None;
        match ask {
            Ask::Set(level) => {
                self.sent = Some(level);
                if response.status == 200 {
                    return Some(level);
                }
                warn!(
                    level,
                    status = response.status,
                    reason = response.reason,
                    "the AirPlay receiver refused a volume change"
                );
                None
            }
            Ask::Get => {
                let level = (response.status == 200)
                    .then(|| reported(&response.body))
                    .flatten();
                if level.is_none() {
                    debug!(
                        status = response.status,
                        "the AirPlay receiver does not report its volume; no longer reading it"
                    );
                    self.reads = false;
                }
                level
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(cseq: u32, status: u16, body: &str) -> Response {
        Response {
            status,
            reason: String::new(),
            headers: vec![("CSeq".into(), cseq.to_string())],
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn levels_map_linearly_onto_minus_30_to_0_db_and_zero_mutes() {
        assert_eq!(decibels(1.0), 0.0);
        assert_eq!(decibels(0.5), -15.0);
        assert_eq!(decibels(0.0), -144.0);
        assert_eq!(decibels(-1.0), -144.0);
        assert_eq!(decibels(2.0), 0.0);
        assert_eq!(set_body(0.25), "volume: -22.500000\r\n");
        assert_eq!(set_body(1.0), "volume: 0.000000\r\n");
        for db in [-30.0, -144.0, -200.0] {
            assert_eq!(level(db), 0.0, "{db}");
        }
        assert_eq!(level(-15.0), 0.5);
        assert_eq!(level(3.0), 1.0);
    }

    #[test]
    fn a_reported_volume_is_read_from_its_line() {
        assert_eq!(reported(b"volume: -15.000000\r\n"), Some(0.5));
        assert_eq!(reported(b"other: 1\r\nvolume:-7.5\r\n"), Some(0.75));
        assert_eq!(reported(b"volume: -144.000000\r\n"), Some(0.0));
        for body in [&b""[..], b"volume: loud\r\n", b"volume: NaN\r\n", b"\xff"] {
            assert_eq!(reported(body), None, "{body:?}");
        }
    }

    #[test]
    fn the_latest_wanted_volume_goes_once_before_any_reading() {
        let start = Instant::now();
        let timeout = Duration::from_secs(4);
        let mut sync = VolumeSync::new(start);
        assert_eq!(sync.next(Some(0.5), start, timeout), Some(Ask::Set(0.5)));
        sync.sent(1, start, Ask::Set(0.5));
        assert_eq!(sync.next(Some(0.75), start, timeout), None, "one at a time");
        assert_eq!(sync.answer(&response(1, 200, "")), Some(0.5));
        assert_eq!(sync.next(Some(0.75), start, timeout), Some(Ask::Set(0.75)));
        sync.sent(2, start, Ask::Set(0.75));
        assert_eq!(sync.answer(&response(2, 200, "")), Some(0.75));
        assert_eq!(
            sync.next(Some(0.75), start, timeout),
            None,
            "no reading right after"
        );
        assert_eq!(
            sync.next(Some(0.75), start + READ_EVERY, timeout),
            Some(Ask::Get)
        );
    }

    #[test]
    fn readings_repeat_while_answered_and_stop_once_not() {
        let start = Instant::now();
        let timeout = Duration::from_secs(4);
        let mut sync = VolumeSync::new(start);
        assert_eq!(sync.next(None, start, timeout), Some(Ask::Get));
        sync.sent(1, start, Ask::Get);
        assert_eq!(
            sync.answer(&response(9, 200, "volume: 0\r\n")),
            None,
            "not its answer"
        );
        assert_eq!(
            sync.answer(&response(1, 200, "volume: -6.000000\r\n")),
            Some(0.8)
        );
        assert_eq!(sync.next(None, start + READ_EVERY / 2, timeout), None);
        let later = start + READ_EVERY;
        assert_eq!(sync.next(None, later, timeout), Some(Ask::Get));
        sync.sent(2, later, Ask::Get);
        assert_eq!(sync.answer(&response(2, 501, "")), None);
        assert_eq!(sync.next(None, later + READ_EVERY * 10, timeout), None);
    }

    #[test]
    fn an_unanswered_request_is_given_up_without_ending_anything() {
        let start = Instant::now();
        let timeout = Duration::from_secs(4);
        let mut sync = VolumeSync::new(start);
        sync.sent(1, start, Ask::Set(0.5));
        assert_eq!(sync.next(Some(0.5), start + timeout, timeout), None);
        let late = start + timeout + Duration::from_millis(1);
        assert_eq!(
            sync.next(Some(0.5), late, timeout),
            Some(Ask::Get),
            "not sent again"
        );
        sync.sent(2, late, Ask::Get);
        let later = late + timeout + Duration::from_millis(1);
        assert_eq!(
            sync.next(Some(0.5), later + READ_EVERY, timeout),
            None,
            "readings stop"
        );
        assert_eq!(
            sync.next(Some(0.25), later + READ_EVERY, timeout),
            Some(Ask::Set(0.25))
        );
    }

    #[test]
    fn a_refused_change_is_not_sent_again_until_the_volume_changes() {
        let start = Instant::now();
        let timeout = Duration::from_secs(4);
        let mut sync = VolumeSync::new(start);
        sync.sent(1, start, Ask::Set(0.5));
        assert_eq!(sync.answer(&response(1, 400, "")), None);
        assert_eq!(
            sync.next(Some(0.5), start + READ_EVERY, timeout),
            Some(Ask::Get)
        );
    }
}
