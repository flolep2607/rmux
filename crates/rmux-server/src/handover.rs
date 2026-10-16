//! The handover state format: everything a daemon holds, as one versioned
//! blob a new binary can read (`docs/design/live-handover.md`, §2.2).
//!
//! This is phase 1 of the handover: the format, the conversions and their
//! tests. Nothing at runtime writes or reads it yet. Phase 3 (quiesce and
//! resume) calls `HandlerState::to_handover` with the readers parked and
//! `HandlerState::restore_handover` in the new image, and adopts the pane
//! processes the restore hands back.
//!
//! # The envelope
//!
//! ```text
//! magic     8 bytes   "RMUXHAND"
//! schema    u32 LE    HANDOVER_SCHEMA of the writer
//! length    u64 LE    payload bytes
//! payload             bincode (varint) of HandoverStateV<schema>
//! ```
//!
//! The schema sits outside the payload so a reader can refuse a schema it
//! does not know before it tries to parse anything; a payload of an unknown
//! schema is never handed to a decoder that would misread it. The length
//! catches a truncated memfd. The design's admission step negotiates the
//! schema up front (`accept_schema`), so a refusal here is the second line.
//!
//! # Why bincode
//!
//! It is already how this crate frames the wire, it is pure Rust (nothing for
//! a static musl build to link), and the DTOs are plain enough that its
//! positional encoding is unambiguous. Its fragility — a reordered field
//! decodes as garbage — is exactly what the schema number and the checked-in
//! fixture of every released schema guard against.
//!
//! # What the DTOs carry
//!
//! The terminal, session-tree and store DTOs are `rmux_core::handover`'s;
//! this module adds what only the server holds. The conversions destructure
//! every field of the real types, so a field added later does not compile
//! until it is either carried or named here as not carried, with the reason.

#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};

use rmux_core::handover::{
    BufferStoreV1, EnvironmentStoreV1, HookStoreV1, KeyBindingStoreV1, OptionStoreV1, PaneTargetV1,
    SessionStoreV1, TerminalStateV1,
};

#[cfg(test)]
#[path = "handover/tests.rs"]
mod tests;

/// The schema this binary writes.
pub(crate) const HANDOVER_SCHEMA: u32 = 1;

/// The schemas this binary reads. A release that moves to schema N+1 keeps N
/// here for one release, so the binary that replaces it can still read it.
pub(crate) const READABLE_SCHEMAS: &[u32] = &[1];

const MAGIC: [u8; 8] = *b"RMUXHAND";
const HEADER_LEN: usize = MAGIC.len() + 4 + 8;

/// Why a blob could not be read.
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// It does not start with the handover magic.
    NotAHandoverState,
    /// It is a handover state of a schema this binary does not read.
    UnsupportedSchema(u32),
    /// It is shorter than its header says.
    Truncated,
    /// Its payload does not parse as the schema it claims.
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAHandoverState => formatter.write_str("not a handover state"),
            Self::UnsupportedSchema(schema) => write!(
                formatter,
                "handover schema {schema} is not one this binary reads ({READABLE_SCHEMAS:?})"
            ),
            Self::Truncated => formatter.write_str("handover state is truncated"),
            Self::Malformed(reason) => write!(formatter, "handover state is malformed: {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn bincode_options() -> impl bincode::Options {
    use bincode::Options;
    // Varint keeps the many small integers of a grid small; trailing bytes
    // are refused so a payload is exactly one state.
    bincode::DefaultOptions::new()
        .with_varint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
}

/// Writes the state into the envelope.
pub(crate) fn encode(state: &HandoverStateV1) -> Vec<u8> {
    use bincode::Options;
    let payload = bincode_options()
        .serialize(state)
        .expect("plain DTOs always serialize");
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&HANDOVER_SCHEMA.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Reads the schema from an envelope without parsing the payload.
pub(crate) fn peek_schema(bytes: &[u8]) -> Result<u32, DecodeError> {
    if bytes.len() < HEADER_LEN {
        return if bytes.starts_with(&MAGIC[..bytes.len().min(MAGIC.len())]) {
            Err(DecodeError::Truncated)
        } else {
            Err(DecodeError::NotAHandoverState)
        };
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(DecodeError::NotAHandoverState);
    }
    let schema = u32::from_le_bytes(
        bytes[MAGIC.len()..MAGIC.len() + 4]
            .try_into()
            .expect("four bytes"),
    );
    Ok(schema)
}

/// Reads an envelope. Only a schema in [`READABLE_SCHEMAS`] is parsed.
pub(crate) fn decode(bytes: &[u8]) -> Result<HandoverStateV1, DecodeError> {
    use bincode::Options;
    let schema = peek_schema(bytes)?;
    if !READABLE_SCHEMAS.contains(&schema) {
        return Err(DecodeError::UnsupportedSchema(schema));
    }
    let length = u64::from_le_bytes(
        bytes[MAGIC.len() + 4..HEADER_LEN]
            .try_into()
            .expect("eight bytes"),
    );
    let payload = &bytes[HEADER_LEN..];
    if (payload.len() as u64) < length {
        return Err(DecodeError::Truncated);
    }
    if payload.len() as u64 != length {
        return Err(DecodeError::Malformed("bytes after the payload".to_owned()));
    }
    match schema {
        1 => bincode_options()
            .deserialize::<HandoverStateV1>(payload)
            .map_err(|error| DecodeError::Malformed(error.to_string())),
        _ => unreachable!("READABLE_SCHEMAS lists only schemas decoded here"),
    }
}

/// The whole daemon, schema 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HandoverStateV1 {
    pub(crate) header: HeaderV1,
    pub(crate) sessions: SessionStoreV1,
    pub(crate) options: OptionStoreV1,
    pub(crate) environment: EnvironmentStoreV1,
    pub(crate) hooks: HookStoreV1,
    pub(crate) buffers: BufferStoreV1,
    pub(crate) key_bindings: KeyBindingStoreV1,
    pub(crate) message_log: Vec<MessageV1>,
    pub(crate) next_message_number: u64,
    pub(crate) startup_config_files: String,
    pub(crate) marked_pane: Option<MarkedPaneV1>,
    /// `(session, window index)` of windows named by `automatic-rename`.
    pub(crate) auto_named_windows: Vec<(String, u32)>,
    pub(crate) window_links: WindowLinksV1,
    pub(crate) input_disabled_panes: Vec<u32>,
    pub(crate) pane_lifecycles: Vec<PaneLifecycleV1>,
    /// Exit details of dead panes kept by `remain-on-exit`.
    pub(crate) dead_panes: Vec<DeadPaneV1>,
    /// Each session's base environment, from the pane that created it.
    pub(crate) session_environments: Vec<(String, RawEnvironmentV1)>,
    /// Every pane's terminal and runtime-adjacent data.
    pub(crate) panes: Vec<PaneStateV1>,
}

/// Who wrote the state, and where each pane's pty master is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeaderV1 {
    /// The writer's `CARGO_PKG_VERSION`, for diagnostics.
    pub(crate) writer_version: String,
    /// The wire version the writer spoke.
    pub(crate) wire_version: u32,
    /// `(pane id, fd)`: the fd number each pane's master has across the exec.
    pub(crate) pane_fds: Vec<(u32, i32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MessageV1 {
    pub(crate) time: i64,
    pub(crate) number: u64,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MarkedPaneV1 {
    pub(crate) target: PaneTargetV1,
    pub(crate) pane_id: u32,
}

/// Linked windows (`link-window`): which winlinks share a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WindowLinksV1 {
    pub(crate) groups: Vec<WindowLinkGroupV1>,
    /// `(session, window index, group id)`.
    pub(crate) slots: Vec<(String, u32, u64)>,
    /// `(session, window index, occurrence id)`.
    pub(crate) occurrences: Vec<(String, u32, u64)>,
    pub(crate) next_group_id: u64,
    pub(crate) next_occurrence_id: u64,
}

/// One group of linked winlinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WindowLinkGroupV1 {
    pub(crate) id: u64,
    /// The session whose runtime owns the window's panes.
    pub(crate) runtime_session: String,
    /// `(session, window index)` of every winlink in the group.
    pub(crate) slots: Vec<(String, u32)>,
}

/// An environment as raw `(name, value)` bytes: it need not be UTF-8.
pub(crate) type RawEnvironmentV1 = Vec<(Vec<u8>, Vec<u8>)>;

/// A pane's lifecycle record: how it was started, so it can be respawned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneLifecycleV1 {
    pub(crate) session_id: u32,
    pub(crate) window_id: u32,
    pub(crate) pane_id: u32,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) process_command: Option<ProcessCommandV1>,
    pub(crate) working_directory: Option<Vec<u8>>,
    pub(crate) respawn_shell: Vec<u8>,
    pub(crate) private_environment: Vec<String>,
    pub(crate) respawn_environment: Vec<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) process: LifecycleProcessV1,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) output_sequence: u64,
    pub(crate) exit_state: Option<ExitDetailsV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcessCommandV1 {
    Argv(Vec<String>),
    Shell(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LifecycleProcessV1 {
    Unknown,
    Running { pid: Option<u32> },
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExitDetailsV1 {
    pub(crate) status: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeadPaneV1 {
    pub(crate) session: String,
    pub(crate) pane_id: u32,
    pub(crate) exit: ExitDetailsV1,
}

/// One pane: its terminal, and its process when it has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneStateV1 {
    /// The session whose runtime holds the pane (a group's owner).
    pub(crate) session: String,
    pub(crate) pane_id: u32,
    pub(crate) transcript: TranscriptV1,
    /// The reader generation, so a stale reader event stays stale.
    pub(crate) output_generation: Option<u64>,
    /// The line an attached client last submitted, `(absolute row, text)`.
    pub(crate) attached_submitted_line: Option<(u64, String)>,
    pub(crate) process: Option<PaneProcessV1>,
}

/// A pane's terminal as the transcript holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TranscriptV1 {
    pub(crate) terminal: TerminalStateV1,
    pub(crate) output_sequence: u64,
}

/// The process side of a pane. Its master fd is in the header's table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneProcessV1 {
    pub(crate) pid: u32,
    /// The raw wait status, once the child has been reaped. A reaped child's
    /// pid may already belong to someone else, so this is the only record of
    /// how it ended.
    pub(crate) exit_status: Option<i32>,
    pub(crate) termination_attempted: bool,
    pub(crate) runtime_window_name: Option<String>,
    pub(crate) profile: TerminalProfileV1,
}

/// What a pane's process was started with: directory, shell, environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TerminalProfileV1 {
    pub(crate) cwd: Vec<u8>,
    pub(crate) shell: Vec<u8>,
    pub(crate) environment: RawEnvironmentV1,
}
