#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(super) use unix::send;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::send;
