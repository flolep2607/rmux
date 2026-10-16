//! The verify subprocess: the new binary reads the old daemon's state before
//! the old daemon commits to becoming it.
//!
//! It is everything a resume does that can fail without touching an fd:
//! split the memfd, check the record, decode the state, restore it into an
//! empty daemon, and check what the restore cannot (each process has a real
//! pid and its own fd). The resume's remaining steps, adopting fds and
//! starting readers, have no failure mode the state can cause.

use std::collections::HashSet;

use super::decode;
use super::record::split;
use crate::pane_terminals::HandlerState;

/// Checks a handover memfd's bytes; on success, a one-line summary.
pub(crate) fn verify_bytes(bytes: &[u8]) -> Result<String, String> {
    let (state, record) = split(bytes).map_err(|error| error.to_string())?;
    if record.listener_fd < 0 {
        return Err(format!(
            "the resume record names fd {} for the listener",
            record.listener_fd
        ));
    }
    if record.rollback_argv.is_empty() {
        return Err("the resume record has no command line to roll back with".to_owned());
    }
    let state = decode(state).map_err(|error| error.to_string())?;
    let panes = state.panes.len();
    let sessions = state.sessions.sessions.len();
    let mut restored = HandlerState::default();
    let processes = restored
        .restore_handover(state)
        .map_err(|error| format!("the state does not restore: {error}"))?;
    let mut fds = HashSet::new();
    fds.insert(record.listener_fd);
    for process in &processes {
        if process.process.pid == 0 {
            return Err(format!("pane %{} has pid 0", process.pane_id.as_u32()));
        }
        if process.master_fd < 0 || !fds.insert(process.master_fd) {
            return Err(format!(
                "pane %{} has fd {}, which is negative or not its own",
                process.pane_id.as_u32(),
                process.master_fd
            ));
        }
    }
    Ok(format!(
        "handover state verified: {sessions} sessions, {panes} panes, {} processes",
        processes.len()
    ))
}
