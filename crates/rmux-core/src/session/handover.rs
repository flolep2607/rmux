//! `Session` to and from its handover DTO.

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use rmux_proto::TerminalSize;

use super::{Session, SessionRecency, WindowIdAllocator};
use crate::handover::{session_name, HandoverError, SessionV1};
use crate::{AlertFlags, SessionId, Window, WindowId};

impl Session {
    pub(crate) fn to_handover(&self) -> SessionV1 {
        let Self {
            id,
            name,
            group_name,
            terminal_size,
            windows,
            winlink_alert_flags,
            active_window,
            last_window,
            group_initial_window_id,
            next_pane_id,
            // Not carried per session: every session in a store shares the
            // store's allocator, so the store carries the one value and
            // rebinds each restored session to it.
            next_window_id: _,
            created_at,
            activity_at,
            last_attached_at,
            recency,
            cwd,
        } = self;
        let TerminalSize { cols, rows } = *terminal_size;
        SessionV1 {
            id: id.as_u32(),
            name: name.as_str().to_owned(),
            group_name: group_name.as_ref().map(|name| name.as_str().to_owned()),
            cols,
            rows,
            windows: windows
                .iter()
                .map(|(index, window)| (*index, window.to_handover()))
                .collect(),
            winlink_alert_flags: winlink_alert_flags
                .iter()
                .map(|(index, flags)| (*index, flags.bits()))
                .collect(),
            active_window: *active_window,
            last_window: *last_window,
            group_initial_window_id: group_initial_window_id.map(WindowId::as_u32),
            next_pane_id: *next_pane_id,
            created_at: *created_at,
            activity_at: *activity_at,
            last_attached_at: *last_attached_at,
            recency: recency.handover_order_key(),
            cwd: cwd.as_ref().map(|cwd| cwd.as_os_str().as_bytes().to_vec()),
        }
    }

    /// Rebuilds a session with the recency token its store minted for it,
    /// and a placeholder allocator the store replaces with its own.
    pub(crate) fn from_handover(
        dto: SessionV1,
        recency: SessionRecency,
    ) -> Result<Self, HandoverError> {
        let SessionV1 {
            id,
            name,
            group_name,
            cols,
            rows,
            windows,
            winlink_alert_flags,
            active_window,
            last_window,
            group_initial_window_id,
            next_pane_id,
            created_at,
            activity_at,
            last_attached_at,
            // Already used by the store to order the minting of `recency`.
            recency: _,
            cwd,
        } = dto;
        let windows = windows
            .into_iter()
            .map(|(index, window)| Ok((index, Window::from_handover(window)?)))
            .collect::<Result<std::collections::BTreeMap<_, _>, HandoverError>>()?;
        if !windows.contains_key(&active_window) {
            return Err(HandoverError::new(format!(
                "session {name:?} has no active window {active_window}"
            )));
        }
        Ok(Self {
            id: SessionId::new(id),
            name: session_name(&name)?,
            group_name: group_name.as_deref().map(session_name).transpose()?,
            terminal_size: TerminalSize { cols, rows },
            windows,
            winlink_alert_flags: winlink_alert_flags
                .into_iter()
                .map(|(index, flags)| (index, AlertFlags::from_bits(flags)))
                .collect(),
            active_window,
            last_window,
            group_initial_window_id: group_initial_window_id.map(WindowId::new),
            next_pane_id,
            next_window_id: WindowIdAllocator::new(0),
            created_at,
            activity_at,
            last_attached_at,
            recency,
            cwd: cwd.map(|cwd| PathBuf::from(std::ffi::OsString::from_vec(cwd))),
        })
    }
}
