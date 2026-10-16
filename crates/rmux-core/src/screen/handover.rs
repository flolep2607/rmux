//! `Screen` to and from its handover DTO.

use super::{SavedGrid, Screen};
use crate::grid::Grid;
use crate::handover::{HandoverError, SavedGridV1, ScreenV1};
use crate::hyperlinks::Hyperlinks;
use crate::input::handover::{saved_state_from_handover, saved_state_to_handover};
use crate::utf8::Utf8Config;

impl Screen {
    pub(crate) fn to_handover(&self) -> ScreenV1 {
        let Self {
            grid,
            cursor_x,
            cursor_y,
            pending_wrap,
            saved_cursor_x,
            saved_cursor_y,
            saved_cursor_pending_wrap,
            saved_state,
            saved_grid,
            rupper,
            rlower,
            mode,
            cursor_style,
            title,
            window_name,
            path,
            title_stack,
            title_rename_enabled,
            tabs,
            hyperlinks,
            active_hyperlink,
            metadata_revision,
            bell_count,
            // Not carried: graphics, clipboard and palette events queued for
            // attached clients, and the count of those dropped. The reader
            // drains both after every read, and the clients they were for do
            // not survive a handover.
            terminal_passthrough: _,
            dropped_terminal_passthrough_count: _,
            has_selected_cells,
            utf8_config,
            alternate_screen_enabled,
            preserve_alternate_screen_cursor,
        } = self;
        ScreenV1 {
            grid: grid.to_handover(),
            cursor_x: *cursor_x,
            cursor_y: *cursor_y,
            pending_wrap: *pending_wrap,
            saved_cursor_x: *saved_cursor_x,
            saved_cursor_y: *saved_cursor_y,
            saved_cursor_pending_wrap: *saved_cursor_pending_wrap,
            saved_state: saved_state_to_handover(saved_state),
            saved_grid: saved_grid.as_ref().map(|saved| {
                let SavedGrid {
                    grid,
                    history_enabled,
                } = saved;
                SavedGridV1 {
                    grid: grid.to_handover(),
                    history_enabled: *history_enabled,
                }
            }),
            rupper: *rupper,
            rlower: *rlower,
            mode: *mode,
            cursor_style: *cursor_style,
            title: title.clone(),
            window_name: window_name.clone(),
            path: path.clone(),
            title_stack: title_stack.clone(),
            title_rename_enabled: *title_rename_enabled,
            tabs: tabs.clone(),
            hyperlinks: hyperlinks.to_handover(),
            active_hyperlink: *active_hyperlink,
            metadata_revision: *metadata_revision,
            bell_count: *bell_count,
            has_selected_cells: *has_selected_cells,
            utf8_config: utf8_config.to_handover(),
            alternate_screen_enabled: *alternate_screen_enabled,
            preserve_alternate_screen_cursor: *preserve_alternate_screen_cursor,
        }
    }

    pub(crate) fn from_handover(dto: ScreenV1) -> Result<Self, HandoverError> {
        let ScreenV1 {
            grid,
            cursor_x,
            cursor_y,
            pending_wrap,
            saved_cursor_x,
            saved_cursor_y,
            saved_cursor_pending_wrap,
            saved_state,
            saved_grid,
            rupper,
            rlower,
            mode,
            cursor_style,
            title,
            window_name,
            path,
            title_stack,
            title_rename_enabled,
            tabs,
            hyperlinks,
            active_hyperlink,
            metadata_revision,
            bell_count,
            has_selected_cells,
            utf8_config,
            alternate_screen_enabled,
            preserve_alternate_screen_cursor,
        } = dto;
        let grid = Grid::from_handover(grid)?;
        // The writer indexes rows and columns by the cursor and the scroll
        // region without checking them again; out-of-range values would be a
        // panic in the new daemon, so they are refused here.
        if cursor_x >= grid.sx() || cursor_y >= grid.sy() {
            return Err(HandoverError::new(format!(
                "cursor {cursor_x},{cursor_y} is outside a {}x{} screen",
                grid.sx(),
                grid.sy()
            )));
        }
        if rupper > rlower || rlower >= grid.sy() {
            return Err(HandoverError::new(format!(
                "scroll region {rupper}..{rlower} does not fit {} rows",
                grid.sy()
            )));
        }
        let saved_grid = saved_grid
            .map(|saved| {
                let SavedGridV1 {
                    grid,
                    history_enabled,
                } = saved;
                Ok::<_, HandoverError>(SavedGrid {
                    grid: Grid::from_handover(grid)?,
                    history_enabled,
                })
            })
            .transpose()?;
        Ok(Self {
            grid,
            cursor_x,
            cursor_y,
            pending_wrap,
            saved_cursor_x,
            saved_cursor_y,
            saved_cursor_pending_wrap,
            saved_state: saved_state_from_handover(saved_state),
            saved_grid,
            rupper,
            rlower,
            mode,
            cursor_style,
            title,
            window_name,
            path,
            title_stack,
            title_rename_enabled,
            tabs,
            hyperlinks: Hyperlinks::from_handover(hyperlinks),
            active_hyperlink,
            metadata_revision,
            bell_count,
            terminal_passthrough: Vec::new(),
            dropped_terminal_passthrough_count: 0,
            has_selected_cells,
            utf8_config: Utf8Config::from_handover(utf8_config),
            alternate_screen_enabled,
            preserve_alternate_screen_cursor,
        })
    }
}
