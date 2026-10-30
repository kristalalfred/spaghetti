use crate::{Receiver, Render, Status};

/// Claude: a session playing to one receiver. It connects in the background,
/// then calls `render` for each packet `LATENCY` before the packet's play time.
/// Dropping it returns at once; the session stops rendering, lets the receiver
/// play what it already holds, then tears down.
pub struct Stream {}

impl Stream {
    pub fn start<R>(receiver: &Receiver, render: R) -> Self
    where
        R: FnMut(Render<'_>) + Send + 'static,
    {
        todo!()
    }

    pub fn status(&self) -> Status {
        todo!()
    }
}
