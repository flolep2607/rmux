//! Handover DTOs: the daemon's state as plain data, for a live handover to a
//! new binary (`docs/design/live-handover.md`).
//!
//! # Why a mirror and not `#[derive(Serialize)]` on the real types
//!
//! The real types (`Screen`, `Grid`, `Session`, the stores) change whenever
//! the daemon changes, and a handover crosses exactly such a change: an old
//! binary writes, a new one reads. If the format were the internals, every
//! refactor would silently change it. These types change only when someone
//! means to change the format, and every change is a new schema version in
//! `rmux-server`'s envelope.
//!
//! The conversions live next to each real type (`to_handover` /
//! `from_handover`), and they destructure every field with no `..`. A field
//! added to a real type then fails to compile until the conversion says
//! whether it is carried, which is the design's guard against a field being
//! lost silently. A field that is deliberately not carried is still named in
//! the destructuring, with the reason beside it.
//!
//! # What is plain here
//!
//! Only primitives, strings, vectors and these structs: no `rmux_proto` wire
//! type appears, because those move with the wire version and a handover is
//! how a daemon crosses a wire change. Names that have a protocol spelling
//! (option names, hook names, layouts, program states) are carried by that
//! spelling rather than by an enum index, so reordering an enum cannot shift
//! what an old state means. Maps are vectors of pairs, which also makes the
//! encoding independent of hash order.
//!
//! The V1 suffix is the schema these types belong to. A V2 copies what it
//! changes and keeps V1 decodable for one release, so the next binary can
//! still read this one's state.

use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "handover/tests.rs"]
mod tests;

/// A state that cannot be restored, and why.
///
/// Restoring refuses rather than guesses: a refusal during verification
/// leaves the old daemon running, while a guess would hand the panes to a
/// daemon whose idea of them is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverError(String);

impl HandoverError {
    /// Creates an error with the given reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }

    /// Returns the reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HandoverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HandoverError {}

/// Restores a session name exactly as it was stored.
///
/// `SessionName::new` sanitises, and sanitising is not idempotent (it escapes
/// a backslash it already wrote), so the stored, already-sanitised spelling
/// goes through the wire type's deserializer, which only rewrites what could
/// never have been stored. A name that comes back different is refused.
pub(crate) fn session_name(value: &str) -> Result<rmux_proto::SessionName, HandoverError> {
    use serde::de::IntoDeserializer;
    let name =
        <rmux_proto::SessionName as Deserialize>::deserialize(IntoDeserializer::<
            serde::de::value::Error,
        >::into_deserializer(value))
        .map_err(|error| HandoverError::new(format!("invalid session name {value:?}: {error}")))?;
    if name.as_str() != value {
        return Err(HandoverError::new(format!(
            "session name {value:?} does not restore as itself"
        )));
    }
    Ok(name)
}

/// A window target, `session:index`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowTargetV1 {
    /// The session name, as stored.
    pub session: String,
    /// The window index.
    pub window_index: u32,
}

impl WindowTargetV1 {
    /// Mirrors a wire window target.
    #[must_use]
    pub fn from_target(target: &rmux_proto::WindowTarget) -> Self {
        Self {
            session: target.session_name().as_str().to_owned(),
            window_index: target.window_index(),
        }
    }

    /// Rebuilds the wire window target.
    pub fn to_target(&self) -> Result<rmux_proto::WindowTarget, HandoverError> {
        let Self {
            session,
            window_index,
        } = self;
        Ok(rmux_proto::WindowTarget::with_window(
            session_name(session)?,
            *window_index,
        ))
    }
}

/// A pane target, `session:window.pane`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTargetV1 {
    /// The session name, as stored.
    pub session: String,
    /// The window index.
    pub window_index: u32,
    /// The pane index.
    pub pane_index: u32,
}

impl PaneTargetV1 {
    /// Mirrors a wire pane target.
    #[must_use]
    pub fn from_target(target: &rmux_proto::PaneTarget) -> Self {
        Self {
            session: target.session_name().as_str().to_owned(),
            window_index: target.window_index(),
            pane_index: target.pane_index(),
        }
    }

    /// Rebuilds the wire pane target.
    pub fn to_target(&self) -> Result<rmux_proto::PaneTarget, HandoverError> {
        let Self {
            session,
            window_index,
            pane_index,
        } = self;
        Ok(rmux_proto::PaneTarget::with_window(
            session_name(session)?,
            *window_index,
            *pane_index,
        ))
    }
}

/// Restores a session name stored in a DTO. Public for `rmux-server`'s
/// conversions, which hold names of their own.
pub fn restore_session_name(value: &str) -> Result<rmux_proto::SessionName, HandoverError> {
    session_name(value)
}

// ---------------------------------------------------------------------------
// Terminal state
// ---------------------------------------------------------------------------

/// One pane's terminal: the screen and what the parser holds beyond it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStateV1 {
    /// The screen, in full.
    pub screen: ScreenV1,
    /// The parser's carried state.
    pub parser: ParserV1,
}

/// A `Screen`, field for field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenV1 {
    /// The grid in use: the main screen's, or the alternate's while it is up.
    pub grid: GridV1,
    /// Cursor column.
    pub cursor_x: u32,
    /// Cursor row.
    pub cursor_y: u32,
    /// Whether the next printable character wraps first.
    pub pending_wrap: bool,
    /// Cursor column saved on entering the alternate screen.
    pub saved_cursor_x: Option<u32>,
    /// Cursor row saved on entering the alternate screen.
    pub saved_cursor_y: Option<u32>,
    /// Pending wrap saved on entering the alternate screen.
    pub saved_cursor_pending_wrap: bool,
    /// DECSC state saved on entering the alternate screen.
    pub saved_state: SavedStateV1,
    /// The main screen's grid while the alternate screen is up.
    pub saved_grid: Option<SavedGridV1>,
    /// Scroll region top.
    pub rupper: u32,
    /// Scroll region bottom.
    pub rlower: u32,
    /// `MODE_*` flags.
    pub mode: u32,
    /// DECSCUSR cursor style.
    pub cursor_style: u32,
    /// Title (OSC 0/2).
    pub title: String,
    /// Window name requested by the program.
    pub window_name: String,
    /// OSC 7 working directory.
    pub path: String,
    /// XTPUSHTITLE stack.
    pub title_stack: Vec<String>,
    /// Whether output may change the title.
    pub title_rename_enabled: bool,
    /// Tab stops, one per column.
    pub tabs: Vec<bool>,
    /// OSC 8 hyperlink table shared by both grids.
    pub hyperlinks: HyperlinksV1,
    /// Inner id of the hyperlink being written, 0 for none.
    pub active_hyperlink: u32,
    /// Counter that moves when title, path or modes change.
    pub metadata_revision: u64,
    /// Bells rung so far.
    pub bell_count: u64,
    /// Whether any cell carries the selection flag.
    pub has_selected_cells: bool,
    /// Width overrides from `codepoint-widths` and friends.
    pub utf8_config: Utf8ConfigV1,
    /// Whether DEC alternate-screen entry is allowed.
    pub alternate_screen_enabled: bool,
    /// Whether leaving the alternate screen keeps its cursor.
    pub preserve_alternate_screen_cursor: bool,
}

/// The main grid kept aside while the alternate screen is up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedGridV1 {
    /// The saved grid.
    pub grid: GridV1,
    /// Whether it collected history before it was put aside.
    pub history_enabled: bool,
}

/// A `Grid`: scrollback and visible rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridV1 {
    /// Width in columns.
    pub sx: u32,
    /// Height in rows.
    pub sy: u32,
    /// `history-limit` in force.
    pub hlimit: u64,
    /// History rows a reflow may still use beyond the limit.
    pub reflow_history_capacity: u64,
    /// History rows a growth can pull back.
    pub hscrolled: u64,
    /// Whether scrolled-off rows go to history.
    pub history_enabled: bool,
    /// Timestamp stamped on rows entering history.
    pub history_stamp: i64,
    /// Rows left before the stamp is refreshed.
    pub history_stamp_remaining: u16,
    /// Scrollback, oldest first.
    pub history: Vec<GridLineV1>,
    /// Visible rows, top first.
    pub visible: Vec<GridLineV1>,
}

/// A `GridLine`.
///
/// A line of plain default-attribute ASCII is already stored compactly as its
/// text with no cells, and stays so here: that is most scrollback, and it is
/// what keeps a 50 000-line state small without a run-length scheme of our own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridLineV1 {
    /// Cells, when the line is not compact.
    pub cells: Vec<GridCellV1>,
    /// The compact text, when it is.
    pub plain_text: Option<String>,
    /// Width in columns.
    pub width: u32,
    /// End of the compact data, trailing spaces included.
    pub plain_text_data_end: u16,
    /// `GRID_LINE_*` flags.
    pub flags: u8,
    /// When the line entered history.
    pub time: i64,
}

/// A `GridCell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridCellV1 {
    /// The grapheme.
    pub text: String,
    /// Display width.
    pub width: u8,
    /// `GRID_FLAG_*` flags.
    pub flags: u8,
    /// `GRID_ATTR_*` attributes.
    pub attr: u16,
    /// Foreground colour.
    pub fg: i32,
    /// Background colour.
    pub bg: i32,
    /// Underline colour.
    pub us: i32,
    /// Hyperlink inner id.
    pub link: u32,
}

/// The parser's cell state (`input_cell`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellStateV1 {
    /// `GRID_ATTR_*` attributes.
    pub attr: u16,
    /// Foreground colour.
    pub fg: i32,
    /// Background colour.
    pub bg: i32,
    /// Underline colour.
    pub us: i32,
    /// Hyperlink inner id.
    pub link: u32,
    /// Active set: 0 for G0, 1 for G1.
    pub set: i32,
    /// G0: 0 normal, 1 ACS.
    pub g0set: i32,
    /// G1: 0 normal, 1 ACS.
    pub g1set: i32,
}

/// DECSC state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedStateV1 {
    /// Saved rendition and character sets.
    pub cell: CellStateV1,
    /// Saved cursor column.
    pub cx: u32,
    /// Saved cursor row.
    pub cy: u32,
    /// Whether origin mode was on.
    pub mode_origin: bool,
}

/// The OSC 8 hyperlink table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperlinksV1 {
    /// Next inner id.
    pub next_inner: u32,
    /// Next external id.
    pub next_external: u64,
    /// Eviction order, oldest first.
    pub order: Vec<u32>,
    /// Entries by inner id.
    pub entries: Vec<HyperlinkEntryV1>,
    /// Deduplication index: `(internal id, uri)` to inner id.
    pub by_uri: Vec<(String, String, u32)>,
}

/// One hyperlink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperlinkEntryV1 {
    /// The id the grid stores.
    pub inner_id: u32,
    /// The program's id, when it gave one.
    pub internal_id: String,
    /// The id rmux shows terminals.
    pub external_id: String,
    /// The target.
    pub uri: String,
}

/// The UTF-8 width configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utf8ConfigV1 {
    /// Whether VS16 always makes a glyph wide.
    pub variation_selector_always_wide: bool,
    /// `(first, last, width)` overrides.
    pub overrides: Vec<(u32, u32, u8)>,
}

/// What the parser holds that the screen does not.
///
/// The VT state machine itself (state, intermediates, parameters, string and
/// UTF-8 buffers) is not here. The parser resumes in the ground state and is
/// fed `pending_bytes`, the raw bytes of the unfinished sequence, which puts
/// it back in the same state through the same transitions. The state machine's
/// shape can then change between versions without the format noticing, and
/// the recovery keyframe already relies on the same replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserV1 {
    /// Current rendition and character sets.
    pub cell: CellStateV1,
    /// DECSC state.
    pub saved: SavedStateV1,
    /// `input-buffer-size` in force; set before the replay, since it bounds it.
    pub input_buffer_limit: u64,
    /// Last printed character, for REP.
    pub last_char: Option<char>,
    /// Whether REP may repeat it (`INPUT_LAST`).
    pub last_char_repeatable: bool,
    /// Kitty keyboard flags and stack, main screen.
    pub kitty_main: KittyKeyboardV1,
    /// Kitty keyboard flags and stack, alternate screen.
    pub kitty_alternate: KittyKeyboardV1,
    /// OSC 10, 11 and 12 colours, in that order.
    pub osc_colours: Vec<Option<String>>,
    /// OSC 7501 records.
    pub program_status: ProgramStatusRecordsV1,
    /// The unfinished sequence, replayed on restore.
    pub pending_bytes: Vec<u8>,
    /// Replies not yet written back to the pty.
    pub pending_replies: Vec<u8>,
}

/// One screen's Kitty keyboard negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittyKeyboardV1 {
    /// Flags in force.
    pub flags: u8,
    /// Pushed flags, oldest first.
    pub stack: Vec<u8>,
}

/// A pane's OSC 7501 records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatusRecordsV1 {
    /// Records in stored order.
    pub entries: Vec<ProgramStatusEntryV1>,
    /// The update clock.
    pub clock: u64,
}

/// One OSC 7501 record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatusEntryV1 {
    /// Record id, empty for the root.
    pub id: String,
    /// `idle`, `working`, `done`, `blocked` or `error`.
    pub state: String,
    /// `permission`, `question` or `auth`.
    pub kind: Option<String>,
    /// Progress percentage.
    pub progress: Option<u8>,
    /// Application name.
    pub app: Option<String>,
    /// Title.
    pub title: Option<String>,
    /// Message.
    pub msg: Option<String>,
    /// Clock value of the last update.
    pub updated: u64,
}

// ---------------------------------------------------------------------------
// Sessions, windows, panes
// ---------------------------------------------------------------------------

/// The `SessionStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStoreV1 {
    /// Sessions, in name order.
    pub sessions: Vec<SessionV1>,
    /// Next `$N`.
    pub next_session_id: u32,
    /// Next `%N`.
    pub next_pane_id: u32,
    /// Next `@N`, shared by every session in the store.
    pub next_window_id: u32,
    /// Session group name to the session that owns the group's panes.
    pub group_runtime_owners: Vec<(String, String)>,
}

/// A `Session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionV1 {
    /// `$N`.
    pub id: u32,
    /// Name, as stored.
    pub name: String,
    /// Group, when grouped.
    pub group_name: Option<String>,
    /// Size in columns.
    pub cols: u16,
    /// Size in rows.
    pub rows: u16,
    /// Windows by index.
    pub windows: Vec<(u32, WindowV1)>,
    /// Winlink alert flags by index.
    pub winlink_alert_flags: Vec<(u32, u8)>,
    /// Current window index.
    pub active_window: u32,
    /// Previous window index.
    pub last_window: Option<u32>,
    /// First grouped-peer winlink, by window id.
    pub group_initial_window_id: Option<u32>,
    /// Next pane id this session hands out.
    pub next_pane_id: u32,
    /// Creation time.
    pub created_at: i64,
    /// Last activity.
    pub activity_at: i64,
    /// Last attach.
    pub last_attached_at: Option<i64>,
    /// Position in the daemon-wide recency order.
    pub recency: u64,
    /// Start directory, as raw bytes.
    pub cwd: Option<Vec<u8>>,
}

/// A `Window`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowV1 {
    /// `@N`.
    pub id: u32,
    /// Panes, in layout order.
    pub panes: Vec<PaneV1>,
    /// Next pane index.
    pub next_pane_index: u32,
    /// Active pane index.
    pub active_pane: u32,
    /// Previous pane index.
    pub last_pane: Option<u32>,
    /// Layout name (`main-vertical`, `tiled`, ...).
    pub layout: String,
    /// Previous layout name.
    pub last_layout: Option<String>,
    /// The layout tree.
    pub layout_tree: Option<LayoutTreeV1>,
    /// Whether the layout was set by hand.
    pub custom_layout: bool,
    /// Layout string from before the last change.
    pub old_layout: Option<String>,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Explicit name.
    pub name: Option<String>,
    /// Whether `automatic-rename` is on for it.
    pub automatic_rename: bool,
    /// Whether a pane is zoomed.
    pub zoomed: bool,
    /// Whether a zoom restore is due.
    pub zoom_restore_pending: bool,
    /// Queued alert flags.
    pub alert_flags: u8,
    /// Whether alerts are queued.
    pub alerts_queued: bool,
    /// Creation time.
    pub created_at: i64,
    /// Last activity.
    pub activity_at: i64,
    /// Main-pane width from `resize-pane -x`.
    pub requested_main_width: Option<u16>,
    /// Main-pane height from `resize-pane -y`.
    pub requested_main_height: Option<u16>,
}

/// A `Pane`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneV1 {
    /// `%N`.
    pub id: u32,
    /// Index in its window.
    pub index: u32,
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Activation counter.
    pub active_point: u64,
    /// Creation time.
    pub created_at: i64,
    /// Last activity.
    pub activity_at: i64,
}

/// A `LayoutTree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutTreeV1 {
    /// The root cell.
    pub root: LayoutCellV1,
    /// Pane position to leaf position, for mirrored layouts.
    pub leaf_map: Option<Vec<u64>>,
}

/// A layout cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutCellV1 {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// `None` for a pane, else the split.
    pub split: Option<LayoutSplitV1>,
}

/// A split cell's children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSplitV1 {
    /// True for side by side, false for stacked.
    pub left_right: bool,
    /// The children.
    pub children: Vec<LayoutCellV1>,
}

// ---------------------------------------------------------------------------
// Stores
// ---------------------------------------------------------------------------

/// An option scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionScopeV1 {
    /// Server options.
    ServerGlobal,
    /// Global session options.
    SessionGlobal,
    /// Global window options.
    WindowGlobal,
    /// One session.
    Session(String),
    /// One window.
    Window(WindowTargetV1),
    /// One pane.
    Pane(PaneTargetV1),
}

/// The `OptionStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionStoreV1 {
    /// Server options.
    pub server_global: OptionNodeV1,
    /// Global session options.
    pub session_global: OptionNodeV1,
    /// Global window options.
    pub window_global: OptionNodeV1,
    /// Per-session options.
    pub sessions: Vec<(String, OptionNodeV1)>,
    /// Per-window options.
    pub windows: Vec<(WindowTargetV1, OptionNodeV1)>,
    /// Per-pane options.
    pub panes: Vec<(PaneTargetV1, OptionNodeV1)>,
}

/// The options set at one scope.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OptionNodeV1 {
    /// `(key, entry)`, in key order.
    pub entries: Vec<(String, OptionEntryV1)>,
}

/// One set option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionEntryV1 {
    /// Canonical name.
    pub name: String,
    /// Whether it is a known option rather than a user `@option`. The type
    /// is looked up again by name on restore, from the reading binary's own
    /// registry.
    pub known: bool,
    /// Scope it was set at.
    pub scope: OptionScopeV1,
    /// The value.
    pub value: OptionValueV1,
    /// The rendered value, as `show-options` prints it.
    pub rendered: String,
}

/// An option's value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionValueV1 {
    /// One value.
    Scalar(StoredOptionValueV1),
    /// Array items: `(index, value, rendered)`.
    Array(Vec<(u32, StoredOptionValueV1, String)>),
}

/// A typed option value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredOptionValueV1 {
    /// A string.
    String(String),
    /// A number.
    Number(u32),
    /// A key name.
    Key(String),
    /// A colour.
    Colour(String),
    /// A flag.
    Flag(bool),
    /// A choice.
    Choice(String),
    /// A parsed command list.
    Command(ParsedCommandsV1),
}

/// A parsed command list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCommandsV1 {
    /// The commands.
    pub commands: Vec<ParsedCommandV1>,
    /// `NAME=value` assignments before them.
    pub assignments: Vec<EnvironmentAssignmentV1>,
    /// True when the whole list is one queue group, false when by line.
    pub one_group: bool,
}

/// One parsed command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCommandV1 {
    /// Canonical name.
    pub name: String,
    /// Arguments.
    pub arguments: Vec<CommandArgumentV1>,
    /// First source line.
    pub start_line: u64,
    /// Source line.
    pub line: u64,
}

/// One command argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandArgumentV1 {
    /// A string.
    String(String),
    /// A `{ ... }` command list.
    Commands(ParsedCommandsV1),
}

/// One `NAME=value` assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentAssignmentV1 {
    /// Name.
    pub name: String,
    /// Value.
    pub value: String,
    /// Whether it was `%hidden`.
    pub hidden: bool,
}

/// The `EnvironmentStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentStoreV1 {
    /// Global entries.
    pub global: Vec<(String, EnvironmentEntryV1)>,
    /// Per-session entries.
    pub sessions: Vec<(String, Vec<(String, EnvironmentEntryV1)>)>,
    /// Names unset globally.
    pub global_unsets: Vec<String>,
    /// Names unset per session.
    pub session_unsets: Vec<(String, Vec<String>)>,
}

/// One environment entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentEntryV1 {
    /// Value; `None` marks a removal.
    pub value: Option<String>,
    /// Value shown instead, when different.
    pub display_value: Option<String>,
    /// `ENVIRON_*` flags.
    pub flags: u8,
    /// Whether the daemon set it rather than a user.
    pub implicit: bool,
}

/// The `HookStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookStoreV1 {
    /// Global session hooks.
    pub session_global: HookBindingsV1,
    /// Global window hooks.
    pub window_global: HookBindingsV1,
    /// Per session.
    pub sessions: Vec<(String, HookBindingsV1)>,
    /// Per window target.
    pub windows: Vec<(WindowTargetV1, HookBindingsV1)>,
    /// Per pane target.
    pub panes: Vec<(PaneTargetV1, HookBindingsV1)>,
    /// Per window id.
    pub windows_by_id: Vec<(u32, HookBindingsV1)>,
    /// Per pane id.
    pub panes_by_id: Vec<(u32, HookBindingsV1)>,
    /// Window target to window id.
    pub window_aliases: Vec<(WindowTargetV1, u32)>,
    /// Pane target to `(window id, pane id)`.
    pub pane_aliases: Vec<(PaneTargetV1, u32, u32)>,
}

/// The hooks set at one scope.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HookBindingsV1 {
    /// Hook name to its array.
    pub hooks: Vec<(String, Vec<HookEntryV1>)>,
}

/// One element of a hook array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEntryV1 {
    /// Array index.
    pub index: u32,
    /// The command.
    pub command: String,
    /// Whether it runs once and is removed.
    pub one_shot: bool,
}

/// The `BufferStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferStoreV1 {
    /// Buffers in name order.
    pub buffers: Vec<BufferV1>,
    /// Next `bufferN`.
    pub next_unnamed_id: u32,
    /// Next stack order.
    pub next_order: u64,
}

/// One paste buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferV1 {
    /// Name.
    pub name: String,
    /// Content.
    pub content: Vec<u8>,
    /// Stack order.
    pub order: u64,
    /// Creation time.
    pub created: i64,
    /// Whether it was auto-named.
    pub unnamed: bool,
}

/// The `KeyBindingStore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBindingStoreV1 {
    /// Tables in name order.
    pub tables: Vec<KeyBindingTableV1>,
}

/// One key table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBindingTableV1 {
    /// Name.
    pub name: String,
    /// Reference count.
    pub references: u64,
    /// Bindings in force.
    pub active: Vec<KeyBindingV1>,
    /// Default bindings, for `unbind -a` and `list-keys -N`.
    pub defaults: Vec<KeyBindingV1>,
}

/// One binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBindingV1 {
    /// Key code.
    pub key: u64,
    /// Note.
    pub note: Option<String>,
    /// Whether it repeats.
    pub repeat: bool,
    /// Commands.
    pub commands: ParsedCommandsV1,
}

/// Converts a length the format stores as `u64` back to `usize`.
pub(crate) fn to_usize(value: u64, what: &str) -> Result<usize, HandoverError> {
    usize::try_from(value).map_err(|_| HandoverError::new(format!("{what} {value} too large")))
}

pub(crate) fn option_scope_to_handover(
    scope: &rmux_proto::types::OptionScopeSelector,
) -> OptionScopeV1 {
    use rmux_proto::types::OptionScopeSelector as Scope;
    match scope {
        Scope::ServerGlobal => OptionScopeV1::ServerGlobal,
        Scope::SessionGlobal => OptionScopeV1::SessionGlobal,
        Scope::WindowGlobal => OptionScopeV1::WindowGlobal,
        Scope::Session(name) => OptionScopeV1::Session(name.as_str().to_owned()),
        Scope::Window(target) => OptionScopeV1::Window(WindowTargetV1::from_target(target)),
        Scope::Pane(target) => OptionScopeV1::Pane(PaneTargetV1::from_target(target)),
    }
}

pub(crate) fn option_scope_from_handover(
    scope: &OptionScopeV1,
) -> Result<rmux_proto::types::OptionScopeSelector, HandoverError> {
    use rmux_proto::types::OptionScopeSelector as Scope;
    Ok(match scope {
        OptionScopeV1::ServerGlobal => Scope::ServerGlobal,
        OptionScopeV1::SessionGlobal => Scope::SessionGlobal,
        OptionScopeV1::WindowGlobal => Scope::WindowGlobal,
        OptionScopeV1::Session(name) => Scope::Session(session_name(name)?),
        OptionScopeV1::Window(target) => Scope::Window(target.to_target()?),
        OptionScopeV1::Pane(target) => Scope::Pane(target.to_target()?),
    })
}
