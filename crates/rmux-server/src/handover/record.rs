//! What crosses the exec besides the state: the resume record, and the
//! command-line flags that point the next image at it.
//!
//! The memfd holds the state envelope (`RMUXHAND`, see the parent module)
//! followed by a second, smaller one:
//!
//! ```text
//! magic     8 bytes   "RMUXRSUM"
//! version   u32 LE    RESUME_RECORD_VERSION of the writer
//! length    u64 LE    payload bytes
//! payload             bincode (varint) of ResumeRecordV<version>
//! ```
//!
//! The state is what a daemon *is*; this record is how the exec was done:
//! which fd is the listener, which socket it is bound to, and the command
//! line to roll back with. It is separate so the state format, which has a
//! checked-in fixture and a schema negotiation of its own, does not change
//! every time the runtime learns to carry something new, and so the verify
//! subprocess can parse the state without caring how it will be resumed.
//!
//! Like the state, it is read by a binary other than its writer (the new
//! image going forward, the old one on rollback), so it is versioned and the
//! reader refuses a version it does not know.

use serde::{Deserialize, Serialize};

use super::{bincode_options, DecodeError, HEADER_LEN, MAGIC};

/// The flag that tells a daemon entry point to resume from a handover memfd
/// instead of binding a socket. Its value is the memfd's fd number.
pub const RESUME_FD_FLAG: &str = "--resume-fd";
/// The fd number of the executable to roll back into. Present only on the
/// forward exec: a daemon that is itself a rollback has nowhere to go back to.
pub const ROLLBACK_FD_FLAG: &str = "--handover-rollback-fd";
/// The flag that runs the verify subprocess: parse and restore the state on
/// the given fd, report, and exit without serving.
pub const VERIFY_FLAG: &str = "--handover-verify";

/// The resume record version this binary writes.
pub(crate) const RESUME_RECORD_VERSION: u32 = 1;
const RECORD_MAGIC: [u8; 8] = *b"RMUXRSUM";

/// How a handover's exec was done, for the image that resumes from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeRecordV1 {
    /// The listening socket, inherited with its backlog.
    pub(crate) listener_fd: i32,
    /// The path that socket is bound to. A resumed daemon refuses to serve a
    /// socket it was not configured for, rather than silently taking over
    /// another path's clients.
    pub(crate) socket_path: Vec<u8>,
    /// The command line the old image was running, to roll back with. The
    /// new binary's own command line is the embedder's choice and may carry
    /// flags the old binary has never heard of.
    pub(crate) rollback_argv: Vec<Vec<u8>>,
}

/// Appends the record's envelope to an encoded state envelope.
pub(crate) fn append_record(state_envelope: &mut Vec<u8>, record: &ResumeRecordV1) {
    use bincode::Options;
    let payload = bincode_options()
        .serialize(record)
        .expect("plain DTOs always serialize");
    state_envelope.extend_from_slice(&RECORD_MAGIC);
    state_envelope.extend_from_slice(&RESUME_RECORD_VERSION.to_le_bytes());
    state_envelope.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    state_envelope.extend_from_slice(&payload);
}

/// Splits a handover memfd's bytes into the state envelope and the record.
///
/// The state is returned as bytes for `decode`, which checks its schema; the
/// record is decoded here.
pub(crate) fn split(bytes: &[u8]) -> Result<(&[u8], ResumeRecordV1), DecodeError> {
    let state_len = envelope_len(bytes, &MAGIC)?;
    let (state, rest) = bytes.split_at(state_len);
    let record_len = envelope_len(rest, &RECORD_MAGIC).map_err(|error| match error {
        DecodeError::NotAHandoverState => {
            DecodeError::Malformed("no resume record after the state".to_owned())
        }
        other => other,
    })?;
    if rest.len() != record_len {
        return Err(DecodeError::Malformed(
            "bytes after the resume record".to_owned(),
        ));
    }
    let version = u32::from_le_bytes(rest[8..12].try_into().expect("four bytes"));
    let payload = &rest[HEADER_LEN..];
    let record = match version {
        1 => {
            use bincode::Options;
            bincode_options()
                .deserialize::<ResumeRecordV1>(payload)
                .map_err(|error| DecodeError::Malformed(format!("resume record: {error}")))?
        }
        other => {
            return Err(DecodeError::Malformed(format!(
                "resume record version {other} is not one this binary reads"
            )))
        }
    };
    Ok((state, record))
}

/// The whole length of the envelope at the start of `bytes`.
fn envelope_len(bytes: &[u8], magic: &[u8; 8]) -> Result<usize, DecodeError> {
    if bytes.len() < HEADER_LEN {
        return if bytes.starts_with(&magic[..bytes.len().min(magic.len())]) && !bytes.is_empty() {
            Err(DecodeError::Truncated)
        } else {
            Err(DecodeError::NotAHandoverState)
        };
    }
    if bytes[..8] != *magic {
        return Err(DecodeError::NotAHandoverState);
    }
    let length = u64::from_le_bytes(bytes[12..HEADER_LEN].try_into().expect("eight bytes"));
    let total = usize::try_from(length)
        .ok()
        .and_then(|length| length.checked_add(HEADER_LEN))
        .ok_or(DecodeError::Truncated)?;
    if bytes.len() < total {
        return Err(DecodeError::Truncated);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ResumeRecordV1 {
        ResumeRecordV1 {
            listener_fd: 7,
            socket_path: b"/tmp/rmux-test/default".to_vec(),
            rollback_argv: vec![b"rmux".to_vec(), b"--__internal-daemon".to_vec()],
        }
    }

    #[test]
    fn a_record_follows_the_state_and_splits_back_off() {
        let mut bytes = b"RMUXHAND".to_vec();
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u64.to_le_bytes());
        bytes.extend_from_slice(b"abc");
        let state_len = bytes.len();
        append_record(&mut bytes, &record());

        let (state, decoded) = split(&bytes).expect("splits");
        assert_eq!(state.len(), state_len);
        assert_eq!(decoded, record());
    }

    #[test]
    fn a_missing_truncated_or_padded_record_is_refused() {
        let mut bytes = b"RMUXHAND".to_vec();
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        assert!(split(&bytes).is_err(), "no record");

        let mut full = bytes.clone();
        append_record(&mut full, &record());
        assert!(split(&full[..full.len() - 1]).is_err(), "truncated");

        let mut padded = full.clone();
        padded.push(0);
        assert!(split(&padded).is_err(), "trailing byte");

        let mut unknown = bytes;
        unknown.extend_from_slice(&RECORD_MAGIC);
        unknown.extend_from_slice(&2_u32.to_le_bytes());
        unknown.extend_from_slice(&0_u64.to_le_bytes());
        assert!(split(&unknown).is_err(), "unknown version");
    }
}
