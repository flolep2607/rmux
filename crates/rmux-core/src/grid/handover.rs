//! `Grid` to and from its handover DTO.

use std::collections::VecDeque;

use super::{Grid, GridLine};
use crate::handover::{to_usize, GridV1, HandoverError};

impl Grid {
    pub(crate) fn to_handover(&self) -> GridV1 {
        let Self {
            sx,
            sy,
            hlimit,
            reflow_history_capacity,
            hscrolled,
            history_enabled,
            history_stamp,
            history_stamp_remaining,
            history,
            // Not carried: a sum over `history`, recomputed on restore.
            history_content_bytes: _,
            visible,
        } = self;
        GridV1 {
            sx: *sx,
            sy: *sy,
            hlimit: *hlimit as u64,
            reflow_history_capacity: *reflow_history_capacity as u64,
            hscrolled: *hscrolled as u64,
            history_enabled: *history_enabled,
            history_stamp: *history_stamp,
            history_stamp_remaining: *history_stamp_remaining,
            history: history.iter().map(GridLine::to_handover).collect(),
            visible: visible.iter().map(GridLine::to_handover).collect(),
        }
    }

    pub(crate) fn from_handover(dto: GridV1) -> Result<Self, HandoverError> {
        let GridV1 {
            sx,
            sy,
            hlimit,
            reflow_history_capacity,
            hscrolled,
            history_enabled,
            history_stamp,
            history_stamp_remaining,
            history,
            visible,
        } = dto;
        // Every grid operation indexes `visible` by row and assumes a
        // non-empty screen; a state that breaks either is refused here rather
        // than panicking in the new daemon after the exec.
        if sx == 0 || sy == 0 {
            return Err(HandoverError::new(format!("grid size {sx}x{sy} is empty")));
        }
        if visible.len() != sy as usize {
            return Err(HandoverError::new(format!(
                "grid has {} visible rows for a height of {sy}",
                visible.len()
            )));
        }
        let hscrolled = to_usize(hscrolled, "grid hscrolled")?;
        if hscrolled > history.len() {
            return Err(HandoverError::new(
                "grid hscrolled exceeds its history".to_owned(),
            ));
        }
        let history = history
            .into_iter()
            .map(GridLine::from_handover)
            .collect::<Result<VecDeque<_>, _>>()?;
        let visible = visible
            .into_iter()
            .map(GridLine::from_handover)
            .collect::<Result<VecDeque<_>, _>>()?;
        let mut grid = Self {
            sx,
            sy,
            hlimit: to_usize(hlimit, "history limit")?,
            reflow_history_capacity: to_usize(reflow_history_capacity, "reflow capacity")?,
            hscrolled,
            history_enabled,
            history_stamp,
            history_stamp_remaining,
            history,
            history_content_bytes: 0,
            visible,
        };
        grid.recompute_history_content_bytes();
        Ok(grid)
    }
}
