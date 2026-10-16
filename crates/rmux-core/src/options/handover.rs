//! `OptionStore` to and from its handover DTO.

use super::storage::OptionNode;
use super::OptionStore;
use crate::handover::{
    restore_session_name, HandoverError, OptionStoreV1, PaneTargetV1, WindowTargetV1,
};

impl OptionStore {
    /// The options at every scope, user `@options` included, as handover data.
    #[must_use]
    pub fn to_handover(&self) -> OptionStoreV1 {
        let Self {
            server_global,
            session_global,
            window_global,
            sessions,
            windows,
            panes,
        } = self;
        // Hash order would make two encodings of one state differ.
        let mut sessions: Vec<_> = sessions
            .iter()
            .map(|(name, node)| (name.as_str().to_owned(), node.to_handover()))
            .collect();
        sessions.sort_by(|left, right| left.0.cmp(&right.0));
        let mut windows: Vec<_> = windows
            .iter()
            .map(|(target, node)| (WindowTargetV1::from_target(target), node.to_handover()))
            .collect();
        windows.sort_by(|left, right| {
            (&left.0.session, left.0.window_index).cmp(&(&right.0.session, right.0.window_index))
        });
        let mut panes: Vec<_> = panes
            .iter()
            .map(|(target, node)| (PaneTargetV1::from_target(target), node.to_handover()))
            .collect();
        panes.sort_by(|left, right| {
            (&left.0.session, left.0.window_index, left.0.pane_index).cmp(&(
                &right.0.session,
                right.0.window_index,
                right.0.pane_index,
            ))
        });
        OptionStoreV1 {
            server_global: server_global.to_handover(),
            session_global: session_global.to_handover(),
            window_global: window_global.to_handover(),
            sessions,
            windows,
            panes,
        }
    }

    /// Rebuilds the options from handover data, typing every value by the
    /// reading binary's registry.
    pub fn from_handover(dto: OptionStoreV1) -> Result<Self, HandoverError> {
        let OptionStoreV1 {
            server_global,
            session_global,
            window_global,
            sessions,
            windows,
            panes,
        } = dto;
        Ok(Self {
            server_global: OptionNode::from_handover(server_global)?,
            session_global: OptionNode::from_handover(session_global)?,
            window_global: OptionNode::from_handover(window_global)?,
            sessions: sessions
                .into_iter()
                .map(|(name, node)| {
                    Ok((
                        restore_session_name(&name)?,
                        OptionNode::from_handover(node)?,
                    ))
                })
                .collect::<Result<_, HandoverError>>()?,
            windows: windows
                .into_iter()
                .map(|(target, node)| Ok((target.to_target()?, OptionNode::from_handover(node)?)))
                .collect::<Result<_, HandoverError>>()?,
            panes: panes
                .into_iter()
                .map(|(target, node)| Ok((target.to_target()?, OptionNode::from_handover(node)?)))
                .collect::<Result<_, HandoverError>>()?,
        })
    }
}
