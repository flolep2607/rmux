//! `Window` to and from its handover DTO.

use rmux_proto::{LayoutName, TerminalSize};

use super::{AlertFlags, Window};
use crate::handover::{HandoverError, WindowV1};
use crate::layout::LayoutTree;
use crate::{Pane, WindowId};

fn layout_name(value: &str) -> Result<LayoutName, HandoverError> {
    value
        .parse()
        .map_err(|_| HandoverError::new(format!("unknown layout {value:?}")))
}

impl Window {
    pub(crate) fn to_handover(&self) -> WindowV1 {
        let Self {
            id,
            panes,
            next_pane_index,
            active_pane,
            last_pane,
            layout,
            last_layout,
            layout_tree,
            custom_layout,
            old_layout,
            size,
            name,
            automatic_rename,
            zoomed,
            zoom_restore_pending,
            alert_flags,
            alerts_queued,
            created_at,
            activity_at,
            requested_main_width,
            requested_main_height,
        } = self;
        let TerminalSize { cols, rows } = *size;
        WindowV1 {
            id: id.as_u32(),
            panes: panes.iter().map(Pane::to_handover).collect(),
            next_pane_index: *next_pane_index,
            active_pane: *active_pane,
            last_pane: *last_pane,
            layout: layout.to_string(),
            last_layout: last_layout.map(|layout| layout.to_string()),
            layout_tree: layout_tree.as_ref().map(LayoutTree::to_handover),
            custom_layout: *custom_layout,
            old_layout: old_layout.clone(),
            cols,
            rows,
            name: name.clone(),
            automatic_rename: *automatic_rename,
            zoomed: *zoomed,
            zoom_restore_pending: *zoom_restore_pending,
            alert_flags: alert_flags.bits(),
            alerts_queued: *alerts_queued,
            created_at: *created_at,
            activity_at: *activity_at,
            requested_main_width: *requested_main_width,
            requested_main_height: *requested_main_height,
        }
    }

    pub(crate) fn from_handover(dto: WindowV1) -> Result<Self, HandoverError> {
        let WindowV1 {
            id,
            panes,
            next_pane_index,
            active_pane,
            last_pane,
            layout,
            last_layout,
            layout_tree,
            custom_layout,
            old_layout,
            cols,
            rows,
            name,
            automatic_rename,
            zoomed,
            zoom_restore_pending,
            alert_flags,
            alerts_queued,
            created_at,
            activity_at,
            requested_main_width,
            requested_main_height,
        } = dto;
        let panes: Vec<Pane> = panes.into_iter().map(Pane::from_handover).collect();
        // A window always has a pane, and its active pane is one of them:
        // `active_pane()` is relied on everywhere without a fallback.
        if !panes.iter().any(|pane| pane.index() == active_pane) {
            return Err(HandoverError::new(format!(
                "window @{id} has no active pane {active_pane}"
            )));
        }
        let layout_tree = layout_tree
            .map(|tree| LayoutTree::from_handover(tree, panes.len()))
            .transpose()?;
        Ok(Self {
            id: WindowId::new(id),
            panes,
            next_pane_index,
            active_pane,
            last_pane,
            layout: layout_name(&layout)?,
            last_layout: last_layout.as_deref().map(layout_name).transpose()?,
            layout_tree,
            custom_layout,
            old_layout,
            size: TerminalSize { cols, rows },
            name,
            automatic_rename,
            zoomed,
            zoom_restore_pending,
            alert_flags: AlertFlags::from_bits(alert_flags),
            alerts_queued,
            created_at,
            activity_at,
            requested_main_width,
            requested_main_height,
        })
    }
}
