#[cfg(target_os = "linux")]
mod linux;
#[cfg(unix)]
mod unix_io;

#[cfg(target_os = "linux")]
pub(crate) use linux::*;
