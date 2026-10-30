use crate::{Browser, ReceiverId, Render, Status};

/// Claude: a session playing to one receiver. It connects in the background:
/// it waits a bounded time for the browser to find the receiver, then for any
/// earlier stream through the same browser to that receiver to finish, then
/// calls `render` for each packet `LATENCY` before the packet's play time.
/// Dropping it tears the session down at once; `finish` lets the receiver play
/// what it holds first.
pub struct Stream {}

impl Stream {
    pub fn start<R>(browser: &Browser, receiver: ReceiverId, render: R) -> Self
    where
        R: FnMut(Render<'_>) + Send + 'static,
    {
        todo!()
    }

    pub fn status(&self) -> Status {
        todo!()
    }

    /// Claude: stops rendering and returns at once. The session keeps answering
    /// the receiver until the end of the last packet it sent has played, then
    /// tears down, within a bounded time.
    pub fn finish(self) -> Finishing {
        todo!()
    }
}

/// Claude: a stream playing out its last packets. The receiver is still taken
/// until it is finished.
pub struct Finishing {}

impl Finishing {
    pub fn is_finished(&self) -> bool {
        todo!()
    }

    /// Claude: blocks until the session has torn down, at most `LATENCY` plus
    /// the teardown timeout.
    pub fn wait(self) {
        todo!()
    }
}
