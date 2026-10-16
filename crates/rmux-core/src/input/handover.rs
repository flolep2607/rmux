//! The parser's handover state.
//!
//! What is carried, and what is rebuilt by replaying the unfinished sequence,
//! is explained on [`crate::handover::ParserV1`].

use super::cell::GridCell;
use super::kitty::KittyScreens;
use super::{CellState, InputParser, InputState, SavedState, INPUT_LAST};
use crate::handover::{to_usize, CellStateV1, ParserV1, SavedStateV1};
use crate::program_status::ProgramStatusRecords;

pub(crate) fn cell_state_to_handover(state: &CellState) -> CellStateV1 {
    let CellState {
        cell,
        set,
        g0set,
        g1set,
    } = state;
    let GridCell {
        attr,
        fg,
        bg,
        us,
        link,
    } = cell;
    CellStateV1 {
        attr: *attr,
        fg: *fg,
        bg: *bg,
        us: *us,
        link: *link,
        set: *set,
        g0set: *g0set,
        g1set: *g1set,
    }
}

pub(crate) fn cell_state_from_handover(dto: CellStateV1) -> CellState {
    let CellStateV1 {
        attr,
        fg,
        bg,
        us,
        link,
        set,
        g0set,
        g1set,
    } = dto;
    CellState {
        cell: GridCell {
            attr,
            fg,
            bg,
            us,
            link,
        },
        set,
        g0set,
        g1set,
    }
}

pub(crate) fn saved_state_to_handover(state: &SavedState) -> SavedStateV1 {
    let SavedState {
        cell,
        cx,
        cy,
        mode_origin,
    } = state;
    SavedStateV1 {
        cell: cell_state_to_handover(cell),
        cx: *cx,
        cy: *cy,
        mode_origin: *mode_origin,
    }
}

pub(crate) fn saved_state_from_handover(dto: SavedStateV1) -> SavedState {
    let SavedStateV1 {
        cell,
        cx,
        cy,
        mode_origin,
    } = dto;
    SavedState {
        cell: cell_state_from_handover(cell),
        cx,
        cy,
        mode_origin,
    }
}

impl InputParser {
    pub(crate) fn to_handover(&self) -> ParserV1 {
        let Self {
            // The VT state machine is not carried: `pending_bytes` below
            // replays into a ground-state parser and rebuilds all of it.
            state: _,
            flags,
            ch: _,
            interm_buf: _,
            interm_len: _,
            param_buf: _,
            param_len: _,
            input_buf: _,
            input_buf_max,
            input_end: _,
            param_list: _,
            cell,
            saved,
            utf8_buf: _,
            utf8_len: _,
            utf8_expected: _,
            utf8_started: _,
            kitty,
            last_char,
            // Per-call scratch, reset at the start of every `parse`.
            printed_in_current_parse: _,
            // A signal the reader drains after every `parse`; quiesced, it is
            // already taken.
            recovery_rebase_required: _,
            // The source of `pending_bytes` while not in the ground state, and
            // rebuilt by replaying them.
            since_ground: _,
            // Set again by the replay when the unfinished sequence is a
            // string; the timer itself is the server's and restarts there.
            ground_timer_active: _,
            reply_buf,
            // A count the reader drains after every `parse`, for clients;
            // attached clients do not survive a handover.
            terminal_passthrough_dropped_count: _,
            osc_colours,
            program_status,
        } = self;
        let (kitty_main, kitty_alternate) = kitty.to_handover();
        ParserV1 {
            cell: cell_state_to_handover(cell),
            saved: saved_state_to_handover(saved),
            input_buffer_limit: *input_buf_max as u64,
            last_char: *last_char,
            last_char_repeatable: flags & INPUT_LAST != 0,
            kitty_main,
            kitty_alternate,
            osc_colours: osc_colours.to_vec(),
            program_status: program_status.to_handover(),
            pending_bytes: self.pending_bytes_ref().to_vec(),
            pending_replies: reply_buf.clone(),
        }
    }

    /// Rebuilds the parser in the ground state, and returns it with the bytes
    /// that must then be fed through it, with its screen, to finish the job.
    pub(crate) fn from_handover(
        dto: ParserV1,
    ) -> Result<(Self, Vec<u8>), crate::handover::HandoverError> {
        let ParserV1 {
            cell,
            saved,
            input_buffer_limit,
            last_char,
            last_char_repeatable,
            kitty_main,
            kitty_alternate,
            osc_colours,
            program_status,
            pending_bytes,
            pending_replies,
        } = dto;
        let osc_colours: [Option<String>; 3] = osc_colours.try_into().map_err(|_| {
            crate::handover::HandoverError::new("parser state needs exactly three OSC colours")
        })?;
        let mut parser = Self::new();
        debug_assert_eq!(parser.state, InputState::Ground);
        parser.set_input_buffer_limit(to_usize(input_buffer_limit, "input buffer limit")?);
        parser.cell = cell_state_from_handover(cell);
        parser.saved = saved_state_from_handover(saved);
        parser.last_char = last_char;
        if last_char_repeatable {
            parser.flags |= INPUT_LAST;
        }
        parser.kitty = KittyScreens::from_handover(kitty_main, kitty_alternate);
        parser.osc_colours = osc_colours;
        parser.program_status = ProgramStatusRecords::from_handover(program_status)?;
        parser.reply_buf = pending_replies;
        Ok((parser, pending_bytes))
    }
}
