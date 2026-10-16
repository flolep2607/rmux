//! OSC 7501, the Program Status Protocol.
//!
//! A program running in a pane reports what it is doing — working, blocked
//! on the person, done, idle, failed — as `OSC 7501 ; key=value:… ST`, after
//! asking with `OSC 7501 ; ? ST` whether the terminal listens. The spec is
//! <https://www.superlogical.com/rex/docs/build/program-status>.
//!
//! The daemon is the terminal here, not a relay. Reports are consumed and
//! kept per pane; none reaches an attached client's terminal, because records
//! from every pane of a session would collide in the one outer terminal, and
//! the consumer is whoever asks the daemon (cctop, through pane formats such
//! as `#{pane_program_state}`).
//!
//! Parsing is strict in the direction the spec asks: a report that breaks a
//! length limit, fails base64, or decodes to a control character is
//! discarded whole, so nothing a program sends half-right is ever shown. The
//! looser rules — a malformed pair skipped, an unknown key ignored, an
//! unknown `kind` or out-of-range `progress` treated as absent — are the
//! spec's own, so that newer programs keep working against this terminal.

use std::fmt;

/// The OSC number of the Program Status Protocol.
pub const PROGRAM_STATUS_OSC: u32 = 7501;

/// The longest whole sequence, `ESC ]` through the terminator, the spec
/// allows. A longer one is discarded rather than truncated: a cut `msg`
/// would be a different message.
pub const PROGRAM_STATUS_MAX_SEQUENCE_BYTES: usize = 4096;

/// How many records one pane keeps. The spec asks for at least 64; more
/// would only let a program grow the daemon's memory with ids it invents.
pub const PROGRAM_STATUS_MAX_RECORDS: usize = 64;

const MAX_ID_BYTES: usize = 128;
const MAX_ID_DEPTH: usize = 8;
const MAX_NAME_SEGMENT_BYTES: usize = 32;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG_DECODED: usize = 2048;

/// What a program says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// Waiting for the person to start something.
    Idle,
    /// Busy on the person's behalf.
    Working,
    /// Finished what it was asked to do.
    Done,
    /// Cannot go on until the person answers.
    Blocked,
    /// Stopped on a failure.
    Error,
}

impl ProgramState {
    /// The protocol's name for the state, which is also what the pane
    /// formats print.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            _ => return None,
        })
    }

    /// Whether the state describes a program that is still running. These are
    /// the records a process exit or a returning shell prompt makes stale.
    const fn is_live(self) -> bool {
        matches!(self, Self::Idle | Self::Working | Self::Blocked)
    }
}

impl fmt::Display for ProgramState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a blocked program is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramBlockedKind {
    /// Leave to do something, such as run a tool.
    Permission,
    /// An answer to a question.
    Question,
    /// A login or other credential.
    Auth,
}

impl ProgramBlockedKind {
    /// The protocol's name for the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "permission" => Self::Permission,
            "question" => Self::Question,
            "auth" => Self::Auth,
            _ => return None,
        })
    }
}

impl fmt::Display for ProgramBlockedKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One status record, as the last report for its id left it.
///
/// Each report replaces its record whole: a field the report leaves out is
/// absent afterwards, not carried over from the previous report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatusRecord {
    state: ProgramState,
    kind: Option<ProgramBlockedKind>,
    progress: Option<u8>,
    app: Option<String>,
    title: Option<String>,
    msg: Option<String>,
}

impl ProgramStatusRecord {
    /// The reported state.
    #[must_use]
    pub const fn state(&self) -> ProgramState {
        self.state
    }

    /// What a blocked program waits for; always `None` for any other state.
    #[must_use]
    pub const fn kind(&self) -> Option<ProgramBlockedKind> {
        self.kind
    }

    /// Percent complete, 0 to 100, only ever set while working or blocked.
    #[must_use]
    pub const fn progress(&self) -> Option<u8> {
        self.progress
    }

    /// The program's name for itself, such as `claude-code`, as this record
    /// carried it. A child record without one inherits its ancestor's, which
    /// [`ProgramStatusRecords::app_for`] resolves.
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }

    /// The decoded title, free of control characters and of invisible
    /// formatting characters such as text-direction overrides.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The decoded message, cleaned as [`Self::title`] is. Text to show, never
    /// to interpret: the spec forbids reading meaning into it.
    #[must_use]
    pub fn msg(&self) -> Option<&str> {
        self.msg.as_deref()
    }
}

/// One OSC 7501 sequence, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProgramStatusSequence {
    /// `?`: the program asks whether the terminal listens.
    Query,
    /// A report that replaces the record at `id` (the root when `None`).
    Report {
        id: Option<String>,
        record: ProgramStatusRecord,
    },
    /// `state=clear`: removes the record at `id` and every record beneath it,
    /// or every record when `id` is `None`.
    Clear { id: Option<String> },
}

/// Parses the payload of an OSC 7501 sequence (what follows `7501;`).
///
/// `sequence_len` is the whole sequence's length, `ESC ]` through the
/// terminator, which is what the spec's 4096-byte limit counts. `None` means
/// the sequence is to be ignored, whether it was malformed or broke a limit.
pub(crate) fn parse_program_status(
    payload: &[u8],
    sequence_len: usize,
) -> Option<ProgramStatusSequence> {
    if sequence_len > PROGRAM_STATUS_MAX_SEQUENCE_BYTES {
        return None;
    }
    // The query may carry fields after the `?` in some future revision; the
    // answer is the same.
    if payload.first() == Some(&b'?') {
        return Some(ProgramStatusSequence::Query);
    }

    let mut state = None;
    let mut id = None;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut title = None;
    let mut msg = None;
    for pair in payload.split(|&byte| byte == b':') {
        let Some(equals) = pair.iter().position(|&byte| byte == b'=') else {
            continue;
        };
        let (key, value) = (&pair[..equals], &pair[equals + 1..]);
        if key.is_empty()
            || !key.iter().all(u8::is_ascii_lowercase)
            || !value.iter().copied().all(is_value_byte)
        {
            continue;
        }
        // Every byte checked above is ASCII.
        let value = std::str::from_utf8(value).ok()?;
        let slot = match key {
            b"state" => &mut state,
            b"id" => &mut id,
            b"kind" => &mut kind,
            b"progress" => &mut progress,
            b"app" => &mut app,
            b"title" => &mut title,
            b"msg" => &mut msg,
            _ => continue,
        };
        *slot = Some(value);
    }

    let id = match id {
        Some(id) if is_valid_id(id) => Some(id.to_owned()),
        Some(_) => return None,
        None => None,
    };
    let state = state?;
    if state == "clear" {
        return Some(ProgramStatusSequence::Clear { id });
    }
    let state = ProgramState::parse(state)?;

    let title = match title {
        Some(encoded) => decode_text(encoded, MAX_TITLE_ENCODED, MAX_TITLE_DECODED)?,
        None => None,
    };
    let msg = match msg {
        Some(encoded) => decode_text(encoded, MAX_MSG_ENCODED, MAX_MSG_DECODED)?,
        None => None,
    };
    let kind = kind
        .filter(|_| state == ProgramState::Blocked)
        .and_then(ProgramBlockedKind::parse);
    let progress = progress
        .filter(|_| matches!(state, ProgramState::Working | ProgramState::Blocked))
        .and_then(parse_progress);
    let app = app.filter(|app| is_name_segment(app)).map(str::to_owned);

    Some(ProgramStatusSequence::Report {
        id,
        record: ProgramStatusRecord {
            state,
            kind,
            progress,
            app,
            title,
            msg,
        },
    })
}

/// The reply to a query: `?` alone, with the terminator the query used.
///
/// The optional `states` and `kinds` fields are left out; their defaults are
/// exactly the sets this parser accepts. Nothing from any report is ever
/// echoed back, so a program cannot read another's records through it.
pub(crate) fn program_status_query_reply(bel: bool) -> &'static str {
    if bel {
        "\x1b]7501;?\x07"
    } else {
        "\x1b]7501;?\x1b\\"
    }
}

/// The status records of one pane.
///
/// One record per id, the root under no id. Only the root is shown through
/// pane formats for now; child records are kept so `clear` and inheritance
/// behave as the spec says, and so exposing them later needs no new state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramStatusRecords {
    entries: Vec<Entry>,
    clock: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Empty for the root record, which no valid id can collide with.
    id: String,
    record: ProgramStatusRecord,
    updated: u64,
}

impl ProgramStatusRecords {
    /// The root record: the one reports without an `id` replace.
    #[must_use]
    pub fn root(&self) -> Option<&ProgramStatusRecord> {
        self.get("")
    }

    /// The record at `id`, with `""` for the root.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ProgramStatusRecord> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.record)
    }

    /// The `app` that applies to the record at `id`: its own, or else its
    /// nearest ancestor's, ending at the root.
    #[must_use]
    pub fn app_for(&self, id: &str) -> Option<&str> {
        let mut id = id;
        loop {
            if let Some(app) = self.get(id).and_then(ProgramStatusRecord::app) {
                return Some(app);
            }
            if id.is_empty() {
                return None;
            }
            id = id.rfind('/').map_or("", |slash| &id[..slash]);
        }
    }

    /// Every record as `(id, record)`, the root's id being `""`, in no
    /// particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ProgramStatusRecord)> {
        self.entries
            .iter()
            .map(|entry| (entry.id.as_str(), &entry.record))
    }

    /// How many records the pane holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pane holds no record.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Applies one parsed report or clear; a query changes nothing.
    pub(crate) fn apply(&mut self, sequence: ProgramStatusSequence) {
        match sequence {
            ProgramStatusSequence::Query => {}
            ProgramStatusSequence::Report { id, record } => {
                self.replace(id.unwrap_or_default(), record);
            }
            ProgramStatusSequence::Clear { id: None } => self.entries.clear(),
            ProgramStatusSequence::Clear { id: Some(id) } => {
                self.entries
                    .retain(|entry| !is_same_or_beneath(&entry.id, &id));
            }
        }
    }

    fn replace(&mut self, id: String, record: ProgramStatusRecord) {
        self.clock = self.clock.wrapping_add(1);
        let updated = self.clock;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.record = record;
            entry.updated = updated;
            return;
        }
        if self.entries.len() >= PROGRAM_STATUS_MAX_RECORDS {
            if let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.updated)
                .map(|(index, _)| index)
            {
                self.entries.swap_remove(oldest);
            }
        }
        self.entries.push(Entry {
            id,
            record,
            updated,
        });
    }

    /// The program is gone: its process exited, or a shell prompt (OSC 133 A)
    /// came back. The spec requires dropping working and blocked records,
    /// which nothing will now resolve. Idle goes too — the spec allows it —
    /// since a program that is no longer running is not waiting at its
    /// prompt. Done and error survive: they are results, and a pane kept by
    /// `remain-on-exit` is exactly where someone looks for one.
    pub(crate) fn program_ended(&mut self) {
        self.entries.retain(|entry| !entry.record.state.is_live());
    }

    /// Forgets every record, as a full reset (RIS) does.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

fn is_same_or_beneath(id: &str, ancestor: &str) -> bool {
    id.strip_prefix(ancestor)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

const fn is_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn is_name_segment(segment: &str) -> bool {
    (1..=MAX_NAME_SEGMENT_BYTES).contains(&segment.len())
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-'))
}

fn is_valid_id(id: &str) -> bool {
    id.len() <= MAX_ID_BYTES
        && id.split('/').count() <= MAX_ID_DEPTH
        && id.split('/').all(is_name_segment)
}

fn parse_progress(value: &str) -> Option<u8> {
    if value.is_empty() || value.len() > 3 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u8>().ok().filter(|progress| *progress <= 100)
}

/// Decodes a base64 `title` or `msg`.
///
/// The outer `None` discards the whole report (a limit, the encoding, or a
/// control character); `Some(None)` is a field present but empty, kept as
/// absent so a format never tells "said nothing" from "said an empty string".
fn decode_text(encoded: &str, max_encoded: usize, max_decoded: usize) -> Option<Option<String>> {
    if encoded.len() > max_encoded {
        return None;
    }
    let bytes = decode_base64(encoded.as_bytes())?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    // Invisible formatting characters are legal text but can reorder or hide
    // what is shown outside the grid; the spec asks for them disarmed.
    let text: String = text
        .chars()
        .filter(|&ch| !is_invisible_format(ch))
        .collect();
    Some((!text.is_empty()).then_some(text))
}

/// Text-direction controls and zero-width formatting characters.
const fn is_invisible_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// Standard-alphabet base64 with optional padding, strictly: a byte outside
/// the alphabet, padding anywhere but the end, a lone trailing symbol, or
/// unused bits that are not zero all fail, so one text has one encoding.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let unpadded = match input {
        [rest @ .., b'=', b'='] if input.len() % 4 == 0 => rest,
        [rest @ .., b'='] if input.len() % 4 == 0 => rest,
        _ => input,
    };
    if unpadded.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(unpadded.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    for &byte in unpadded {
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    (accumulator == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(payload: &str) -> Option<ProgramStatusSequence> {
        parse_program_status(payload.as_bytes(), payload.len() + 9)
    }

    fn report(payload: &str) -> (Option<String>, ProgramStatusRecord) {
        match parse(payload) {
            Some(ProgramStatusSequence::Report { id, record }) => (id, record),
            other => panic!("{payload:?} should be a report, got {other:?}"),
        }
    }

    fn b64(text: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in text.as_bytes().chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    #[test]
    fn a_query_is_recognised_and_answered_with_the_queries_terminator() {
        assert_eq!(parse("?"), Some(ProgramStatusSequence::Query));
        assert_eq!(program_status_query_reply(false), "\x1b]7501;?\x1b\\");
        assert_eq!(program_status_query_reply(true), "\x1b]7501;?\x07");
    }

    #[test]
    fn a_full_report_parses_every_field() {
        let payload = format!(
            "state=blocked:kind=permission:progress=40:app=claude-code:title={}:msg={}",
            b64("Run tests?"),
            b64("cargo test --workspace"),
        );
        let (id, record) = report(&payload);
        assert_eq!(id, None);
        assert_eq!(record.state(), ProgramState::Blocked);
        assert_eq!(record.kind(), Some(ProgramBlockedKind::Permission));
        assert_eq!(record.progress(), Some(40));
        assert_eq!(record.app(), Some("claude-code"));
        assert_eq!(record.title(), Some("Run tests?"));
        assert_eq!(record.msg(), Some("cargo test --workspace"));
    }

    #[test]
    fn the_report_claude_code_sends_at_startup_parses() {
        let (id, record) = report("state=idle:app=claude-code");
        assert_eq!(id, None);
        assert_eq!(record.state(), ProgramState::Idle);
        assert_eq!(record.app(), Some("claude-code"));
    }

    #[test]
    fn the_last_value_of_a_repeated_key_wins() {
        let (_, record) = report("state=working:app=one:state=done:app=two");
        assert_eq!(record.state(), ProgramState::Done);
        assert_eq!(record.app(), Some("two"));
    }

    #[test]
    fn malformed_pairs_are_skipped_and_unknown_keys_ignored() {
        let (_, record) = report("noequals:=empty:Bad=1:app=x y:future=1:state=working:app=ok");
        assert_eq!(record.state(), ProgramState::Working);
        assert_eq!(record.app(), Some("ok"));
    }

    #[test]
    fn a_report_without_a_known_state_is_ignored() {
        assert_eq!(parse("app=claude-code"), None);
        assert_eq!(parse("state=sleeping"), None);
        assert_eq!(parse("state=Working"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn fields_that_do_not_apply_are_treated_as_absent() {
        let (_, record) = report("state=working:kind=permission:progress=101");
        assert_eq!(record.kind(), None);
        assert_eq!(record.progress(), None);
        let (_, record) = report("state=done:progress=50");
        assert_eq!(record.progress(), None);
        let (_, record) = report("state=blocked:kind=coffee:progress=-1");
        assert_eq!(record.kind(), None);
        assert_eq!(record.progress(), None);
        let (_, record) = report(&format!("state=idle:app={}", "a".repeat(33)));
        assert_eq!(record.app(), None);
    }

    #[test]
    fn an_invalid_id_discards_the_report() {
        assert!(matches!(
            parse("state=done:id=build/test"),
            Some(ProgramStatusSequence::Report { id: Some(ref id), .. }) if id == "build/test"
        ));
        assert_eq!(parse("state=done:id="), None);
        assert_eq!(parse("state=done:id=a//b"), None);
        assert_eq!(parse("state=done:id=a=b"), None);
        assert_eq!(parse(&format!("state=done:id={}", "a".repeat(33))), None);
        assert_eq!(parse("state=done:id=1/2/3/4/5/6/7/8/9"), None);
        let long = ["abcdefghijklmnop"; 8].join("/");
        assert!(long.len() > MAX_ID_BYTES);
        assert_eq!(parse(&format!("state=done:id={long}")), None);
    }

    #[test]
    fn bad_base64_or_control_characters_discard_the_report() {
        // A byte outside the value set makes the pair malformed, which the
        // spec skips rather than discarding the report.
        assert!(matches!(
            parse("state=done:msg=!!!!"),
            Some(ProgramStatusSequence::Report { ref record, .. }) if record.msg().is_none()
        ));
        assert_eq!(parse("state=done:msg=QR"), None, "unused bits must be zero");
        assert_eq!(parse("state=done:msg=QQ=A"), None);
        assert_eq!(parse("state=done:msg=Q"), None);
        assert_eq!(
            parse(&format!("state=done:msg={}", b64("two\nlines"))),
            None
        );
        assert_eq!(
            parse(&format!("state=done:title={}", b64("tab\there"))),
            None
        );
        assert_eq!(
            parse(&format!("state=done:msg={}", b64("c1 \u{85} char"))),
            None
        );
        // Invalid UTF-8 (a lone continuation byte, "gA==").
        assert_eq!(parse("state=done:msg=gA=="), None);
    }

    #[test]
    fn padding_is_optional() {
        let (_, padded) = report("state=done:msg=aGk=");
        let (_, unpadded) = report("state=done:msg=aGk");
        assert_eq!(padded.msg(), Some("hi"));
        assert_eq!(unpadded.msg(), Some("hi"));
    }

    #[test]
    fn invisible_formatting_is_disarmed() {
        let (_, record) = report(&format!("state=done:msg={}", b64("ab\u{202E}cd\u{200B}")));
        assert_eq!(record.msg(), Some("abcd"));
    }

    #[test]
    fn reports_over_a_limit_are_discarded_whole() {
        let title = b64(&"t".repeat(MAX_TITLE_DECODED));
        assert!(parse(&format!("state=done:title={title}")).is_some());
        let title = b64(&"t".repeat(MAX_TITLE_DECODED + 1));
        assert!(title.len() > MAX_TITLE_ENCODED);
        assert_eq!(parse(&format!("state=done:title={title}")), None);
        // 2732 characters can carry 2049 bytes, one over the decoded limit.
        let msg = b64(&"m".repeat(MAX_MSG_DECODED + 1));
        assert!(msg.len() <= MAX_MSG_ENCODED);
        assert_eq!(parse(&format!("state=done:msg={msg}")), None);
        let msg = b64(&"m".repeat(MAX_MSG_DECODED + 3));
        assert_eq!(parse(&format!("state=done:msg={msg}")), None);
        let msg = b64(&"m".repeat(MAX_MSG_DECODED));
        assert!(parse(&format!("state=done:msg={msg}")).is_some());
        let payload = "state=done";
        assert_eq!(
            parse_program_status(payload.as_bytes(), PROGRAM_STATUS_MAX_SEQUENCE_BYTES + 1),
            None
        );
        assert!(
            parse_program_status(payload.as_bytes(), PROGRAM_STATUS_MAX_SEQUENCE_BYTES).is_some()
        );
    }

    #[test]
    fn each_report_replaces_its_record_whole() {
        let mut records = ProgramStatusRecords::default();
        records.apply(parse("state=working:progress=10:app=a").unwrap());
        records.apply(parse("state=done").unwrap());
        let root = records.root().unwrap();
        assert_eq!(root.state(), ProgramState::Done);
        assert_eq!(root.app(), None);
        assert_eq!(root.progress(), None);
    }

    #[test]
    fn clear_removes_the_record_and_its_children() {
        let mut records = ProgramStatusRecords::default();
        for payload in [
            "state=working:app=claude-code",
            "state=working:id=a",
            "state=blocked:id=a/b",
            "state=done:id=ab",
        ] {
            records.apply(parse(payload).unwrap());
        }
        assert_eq!(records.app_for("a/b"), Some("claude-code"));
        records.apply(parse("state=clear:id=a").unwrap());
        assert!(records.get("a").is_none());
        assert!(records.get("a/b").is_none());
        assert!(
            records.get("ab").is_some(),
            "a sibling sharing a prefix stays"
        );
        assert!(records.root().is_some());
        records.apply(parse("state=clear").unwrap());
        assert!(records.is_empty());
    }

    #[test]
    fn the_least_recently_updated_record_is_evicted() {
        let mut records = ProgramStatusRecords::default();
        records.apply(parse("state=working").unwrap());
        for n in 1..PROGRAM_STATUS_MAX_RECORDS {
            records.apply(parse(&format!("state=working:id=r{n}")).unwrap());
        }
        // Touching the root makes r1 the oldest.
        records.apply(parse("state=blocked").unwrap());
        records.apply(parse("state=working:id=new").unwrap());
        assert_eq!(records.len(), PROGRAM_STATUS_MAX_RECORDS);
        assert!(records.get("r1").is_none());
        assert!(records.root().is_some());
        assert!(records.get("new").is_some());
    }

    #[test]
    fn a_program_ending_keeps_only_results() {
        let mut records = ProgramStatusRecords::default();
        for payload in [
            "state=working",
            "state=blocked:id=b",
            "state=idle:id=i",
            "state=done:id=d",
            "state=error:id=e",
        ] {
            records.apply(parse(payload).unwrap());
        }
        records.program_ended();
        let mut left: Vec<_> = records.iter().map(|(id, _)| id).collect();
        left.sort_unstable();
        assert_eq!(left, ["d", "e"]);
    }
}
