use crate::{Receiver, ReceiverId};

/// Claude: a live view of the AirPlay receivers on the local network, kept by
/// browsing `_raop._tcp` and `_airplay._tcp` through the system's mDNSResponder.
/// A receiver advertising both services is one receiver.
pub struct Browser {}

impl Browser {
    /// Claude: starts browsing and returns at once.
    pub fn start() -> Self {
        todo!()
    }

    /// Claude: the receivers found so far. Waits until the browser has run long
    /// enough for receivers already on the network to have answered, and fails
    /// when browsing failed, so that no receivers is never a failure in disguise.
    pub fn receivers(&self) -> Result<Vec<Receiver>, String> {
        todo!()
    }

    pub fn find(&self, id: &ReceiverId) -> Result<Option<Receiver>, String> {
        todo!()
    }
}
