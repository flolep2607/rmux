//! The new image's first steps: read what the old one left, and take the
//! listener over, before the daemon is built around them.
//!
//! Nothing here takes ownership of an inherited fd. The listener is used
//! through a duplicate, so the number the old image recorded stays open and
//! inheritable until the resume is complete: if anything fails before then,
//! the rollback hands the old binary every fd exactly as it was handed this
//! one.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rmux_ipc::LocalListener;

use super::api::HandoverResume;
use super::record::split;
use super::{decode, HandoverStateV1};
use crate::unix_socket::{socket_file_identity, BoundUnixListener};

/// Reads the handover memfd and takes the listener over.
pub(crate) fn prepare(
    resume: &HandoverResume,
    socket_path: &Path,
) -> Result<(BoundUnixListener, ResumeInput), String> {
    // `execve` kept the old image's signal mask; a daemon that resumed with
    // SIGCHLD or SIGTERM blocked would never hear them.
    if let Err(error) = rmux_os::handover::unblock_all_signals() {
        return Err(format!("cannot reset the signal mask: {error}"));
    }
    let memfd = rmux_os::handover::borrow_inherited_fd(resume.state_fd)
        .map_err(|error| format!("the handover memfd: {error}"))?;
    let bytes = rmux_os::handover::read_all_from_start(memfd)
        .map_err(|error| format!("cannot read the handover memfd: {error}"))?;
    let (state, record) = split(&bytes).map_err(|error| error.to_string())?;
    if record.socket_path != socket_path.as_os_str().as_bytes() {
        return Err(format!(
            "the inherited listener is bound to {:?}, not the configured {:?}",
            String::from_utf8_lossy(&record.socket_path),
            socket_path
        ));
    }
    let state = decode(state).map_err(|error| error.to_string())?;
    let inherited = rmux_os::handover::borrow_inherited_fd(record.listener_fd)
        .map_err(|error| format!("the inherited listener: {error}"))?;
    let duplicate = inherited
        .try_clone_to_owned()
        .map_err(|error| format!("cannot duplicate the inherited listener: {error}"))?;
    let listener = LocalListener::from_std(std::os::unix::net::UnixListener::from(duplicate))
        .map_err(|error| format!("cannot serve the inherited listener: {error}"))?;
    let identity = socket_file_identity(socket_path).ok();
    Ok((
        BoundUnixListener { listener, identity },
        ResumeInput {
            state,
            inherited_listener_fd: record.listener_fd,
        },
    ))
}

/// What the accept loop restores before it accepts anything.
pub(crate) struct ResumeInput {
    pub(crate) state: HandoverStateV1,
    /// The listener's number as inherited; the loop serves a duplicate, and
    /// this one is closed once the resume no longer needs a way back.
    pub(crate) inherited_listener_fd: i32,
}

impl std::fmt::Debug for ResumeInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResumeInput")
            .field("panes", &self.state.panes.len())
            .field("inherited_listener_fd", &self.inherited_listener_fd)
            .finish()
    }
}
