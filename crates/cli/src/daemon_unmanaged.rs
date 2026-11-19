//! Claude: off macOS no service manager runs the daemon; it runs as
//! `spaghettid` in a terminal of its own.

use crate::DaemonAction;
use crate::output::Output;

pub const START: &str = "spaghettid";
pub const STOP: &str = "stop spaghettid";

pub fn run(_action: DaemonAction, _out: Output) -> Result<(), String> {
    Err("spaghetti manages the daemon only on macOS; here, run spaghettid in a terminal and stop it with Ctrl-C".into())
}
