use crate::Receiver;

/// Claude: a live view of the AirPlay receivers on the local network, kept by
/// browsing `_raop._tcp` and `_airplay._tcp` through the system's mDNSResponder.
/// A receiver advertising both services is one receiver. Streams started
/// through one browser take turns at each receiver.
pub struct Browser {}

impl Browser {
    /// Claude: starts browsing and returns at once.
    pub fn start() -> Self {
        todo!()
    }

    /// Claude: a live snapshot of the receivers found and resolved so far, taken
    /// once the browser has run for a bounded warm-up. Fails when browsing
    /// failed, so that no receivers is never a failure in disguise.
    pub fn receivers(&self) -> Result<Vec<Receiver>, String> {
        todo!()
    }
}
