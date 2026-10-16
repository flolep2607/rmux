//! The public face of a live handover: what an embedder passes in, and what
//! comes back when a handover does not happen.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::time::Duration;

use super::record::{split, ResumeRecordV1, RESUME_FD_FLAG, ROLLBACK_FD_FLAG, VERIFY_FLAG};

/// The longest a daemon spends on in-flight commands, parking, the snapshot
/// and the verify before it gives up and carries on.
///
/// Every pane is frozen from the moment the readers park until the new image
/// reads again, so this bounds what an agent can notice. Measured in release
/// (docs/design/live-handover.md, §7), capture and encode plus the verify
/// subprocess's decode and restore take about 0.2 s for 50 000 lines of
/// scrollback and about 0.95 s for four panes full at that limit. Five
/// seconds leaves room for a busy machine and a short command still running,
/// and still lets go of every pane well before an agent's own timeouts.
pub const DEFAULT_HANDOVER_DEADLINE: Duration = Duration::from_secs(5);

/// How to run one handover.
#[derive(Debug)]
pub struct HandoverOptions {
    pub(crate) deadline: Duration,
    pub(crate) executable: Option<OwnedFd>,
    pub(crate) command: Option<Vec<OsString>>,
}

impl Default for HandoverOptions {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_HANDOVER_DEADLINE,
            executable: None,
            command: None,
        }
    }
}

impl HandoverOptions {
    /// A handover into this daemon's own executable, with the command line it
    /// was configured with ([`DaemonConfig::with_handover_command`]).
    ///
    /// [`DaemonConfig::with_handover_command`]: crate::DaemonConfig::with_handover_command
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How long quiesce, snapshot and verify may take before the handover is
    /// abandoned and the daemon carries on.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The executable to hand over to, as an open fd (the design's peer
    /// `/proc/<pid>/exe`). Without one, the daemon re-executes its own image.
    #[must_use]
    pub fn with_executable(mut self, executable: OwnedFd) -> Self {
        self.executable = Some(executable);
        self
    }

    /// The command line for the new image, in place of the configured one.
    /// The resume flags are appended to it.
    #[must_use]
    pub fn with_command(mut self, argv: Vec<OsString>) -> Self {
        self.command = Some(argv);
        self
    }
}

/// Why a handover did not happen. In every case the daemon carried on as it
/// was, panes, clients and all, except where [`HandoverErrorKind::ExecFailed`]
/// says otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverError {
    kind: HandoverErrorKind,
    reason: String,
}

/// What stopped a handover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HandoverErrorKind {
    /// Admission said no before anything changed: something the state format
    /// cannot carry is active, the daemon is not configured for it, or
    /// another handover is running.
    Refused,
    /// Quiesce, snapshot or verify ran past the deadline.
    DeadlineExceeded,
    /// The new binary could not read or restore the state.
    VerifyFailed,
    /// The `execveat` itself failed. The daemon carries on, but its clients
    /// had already been told it was upgrading and have disconnected.
    ExecFailed,
}

impl HandoverError {
    pub(crate) fn new(kind: HandoverErrorKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    pub(crate) fn refused(reason: impl Into<String>) -> Self {
        Self::new(HandoverErrorKind::Refused, reason)
    }

    /// What stopped it.
    #[must_use]
    pub fn kind(&self) -> HandoverErrorKind {
        self.kind
    }

    /// Why, for a person or a log.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for HandoverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            HandoverErrorKind::Refused => "handover refused",
            HandoverErrorKind::DeadlineExceeded => "handover abandoned at its deadline",
            HandoverErrorKind::VerifyFailed => "handover verify failed",
            HandoverErrorKind::ExecFailed => "handover exec failed",
        };
        write!(formatter, "{what}: {}", self.reason)
    }
}

impl std::error::Error for HandoverError {}

/// A daemon entry point's instruction to resume from a handover instead of
/// starting fresh: the `--resume-fd` its predecessor exec'd it with.
///
/// An embedder takes it out of its command line with
/// [`take_from_args`](Self::take_from_args) before parsing the rest, and
/// passes it to [`DaemonConfig::resuming`]. If the embedder itself cannot
/// start (its own flags, its own setup), it calls
/// [`roll_back`](Self::roll_back), so the old binary takes the daemon back.
///
/// [`DaemonConfig::resuming`]: crate::DaemonConfig::resuming
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverResume {
    pub(crate) state_fd: RawFd,
    pub(crate) rollback_fd: Option<RawFd>,
}

impl HandoverResume {
    /// Removes the resume flags from `args`, and returns them if present.
    ///
    /// `args` excludes the program name. A malformed flag is an error rather
    /// than being left for the embedder's parser, which would only say it
    /// does not know it.
    pub fn take_from_args(args: &mut Vec<OsString>) -> Result<Option<Self>, String> {
        let state_fd = take_fd_flag(args, RESUME_FD_FLAG)?;
        let rollback_fd = take_fd_flag(args, ROLLBACK_FD_FLAG)?;
        match (state_fd, rollback_fd) {
            (Some(state_fd), rollback_fd) => Ok(Some(Self {
                state_fd,
                rollback_fd,
            })),
            (None, None) => Ok(None),
            (None, Some(_)) => Err(format!("{ROLLBACK_FD_FLAG} requires {RESUME_FD_FLAG}")),
        }
    }

    /// Gives the daemon back to the binary it came from, with the same
    /// state, because this one cannot resume it.
    ///
    /// Only returns if that is impossible: this image is itself a rollback,
    /// or the exec failed. The caller then has nothing left but to exit, and
    /// the panes go with it (the residual risk in the design's rollback
    /// table).
    pub fn roll_back(self, reason: &str) -> io::Error {
        roll_back(self.state_fd, self.rollback_fd, reason)
    }
}

impl HandoverResume {
    /// The resume is complete: closes what was kept only for a way back
    /// (the memfd, the old executable, the listener's inherited number).
    pub(crate) fn finish(self, inherited_listener_fd: RawFd) {
        for fd in [
            Some(self.state_fd),
            self.rollback_fd,
            Some(inherited_listener_fd),
        ]
        .into_iter()
        .flatten()
        {
            // Adopting closes it on drop; one already gone has nothing to close.
            drop(rmux_os::handover::adopt_inherited_fd(fd));
        }
    }
}

fn take_fd_flag(args: &mut Vec<OsString>, flag: &str) -> Result<Option<RawFd>, String> {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return Ok(None);
    };
    let value = args
        .get(index + 1)
        .ok_or_else(|| format!("{flag} requires a file descriptor"))?
        .to_str()
        .and_then(|value| value.parse::<RawFd>().ok())
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| format!("{flag} requires a non-negative file descriptor"))?;
    args.drain(index..=index + 1);
    if args.iter().any(|arg| arg == flag) {
        return Err(format!("{flag} given twice"));
    }
    Ok(Some(value))
}

/// Execs back into the old image. Shared by an embedder's own rollback and
/// the server's, which happens when restoring fails before readiness.
pub(crate) fn roll_back(state_fd: RawFd, rollback_fd: Option<RawFd>, reason: &str) -> io::Error {
    tracing::error!("live handover: resume failed, rolling back: {reason}");
    let Some(rollback_fd) = rollback_fd else {
        return io::Error::other(format!(
            "resume failed and this image is already a rollback, so there is no binary to \
             return to: {reason}"
        ));
    };
    // Read without adopting: the memfd and every other inherited fd must stay
    // open and inheritable for the image this one execs back into.
    let borrowed = match borrow_inherited(state_fd) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let bytes = match rmux_os::handover::read_all_from_start(borrowed) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let record = match split(&bytes) {
        Ok((_, record)) => record,
        Err(error) => {
            return io::Error::other(format!(
                "cannot roll back: the resume record does not parse ({error}): {reason}"
            ))
        }
    };
    write_failure_marker(&record, reason);
    // The listener was served through a duplicate, but make sure the number
    // the old image will look for is still one it can inherit.
    if let Ok(listener) = rmux_os::handover::borrow_inherited_fd(record.listener_fd) {
        let _ = rmux_os::handover::set_inheritable(listener, true);
    }
    let exe = match borrow_inherited(rollback_fd) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let mut argv: Vec<OsString> = record
        .rollback_argv
        .iter()
        .map(|arg| OsString::from_vec(arg.clone()))
        .collect();
    argv.push(RESUME_FD_FLAG.into());
    argv.push(state_fd.to_string().into());
    let _ = rmux_os::handover::unblock_all_signals();
    // The old image does not need the fd it is exec'd through; leaving it
    // inheritable would only leak it into that image.
    let _ = rmux_os::handover::set_inheritable(exe, false);
    let error = rmux_os::handover::execveat_fd(exe, &argv, &current_environment());
    let _ = rmux_os::handover::set_inheritable(exe, true);
    io::Error::new(
        error.kind(),
        format!("rollback exec failed ({error}) after: {reason}"),
    )
}

/// The marker a failed handover leaves beside the socket, for `doctor`.
fn write_failure_marker(record: &ResumeRecordV1, reason: &str) {
    let socket = Path::new(std::ffi::OsStr::from_bytes(&record.socket_path));
    let Some(directory) = socket.parent() else {
        return;
    };
    let body = format!(
        "reason: {reason}\nresumed by: {}\nrolled back at: {}\n",
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now().to_rfc3339(),
    );
    let _ = std::fs::write(directory.join("handover.failed"), body);
}

fn borrow_inherited(raw: RawFd) -> io::Result<std::os::fd::BorrowedFd<'static>> {
    rmux_os::handover::borrow_inherited_fd(raw)
}

pub(crate) fn current_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// Runs the verify subprocess when `args` (without the program name) asks
/// for it, and returns its exit code; returns `None` otherwise, for the
/// embedder to carry on with its own command line.
///
/// The old daemon runs the new binary with `--handover-verify <fd>` before it
/// commits to the exec. This parses the whole state and restores it into an
/// empty daemon, touching no socket and no pane, and exits 0, or prints why
/// not and exits 1. An embedder calls it first thing in `main`, as it calls
/// [`run_internal_fifo_reader_helper`](crate::run_internal_fifo_reader_helper).
pub fn run_handover_verify_helper<I>(args: I) -> Option<i32>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let index = args.iter().position(|arg| arg == VERIFY_FLAG)?;
    let fd = match args
        .get(index + 1)
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse::<RawFd>().ok())
    {
        Some(fd) => fd,
        None => {
            eprintln!("{VERIFY_FLAG} requires a file descriptor");
            return Some(2);
        }
    };
    match verify_state_on_fd(fd) {
        Ok(summary) => {
            println!("{summary}");
            Some(0)
        }
        Err(reason) => {
            eprintln!("{reason}");
            Some(1)
        }
    }
}

fn verify_state_on_fd(fd: RawFd) -> Result<String, String> {
    let fd = rmux_os::handover::adopt_inherited_fd(fd).map_err(|error| error.to_string())?;
    let bytes = rmux_os::handover::read_all_from_start(fd.as_fd())
        .map_err(|error| format!("cannot read the handover state: {error}"))?;
    super::verify::verify_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn resume_flags_are_taken_out_of_the_command_line() {
        let mut argv = args(&[
            "--socket",
            "/s",
            "--resume-fd",
            "5",
            "--handover-rollback-fd",
            "6",
        ]);
        let resume = HandoverResume::take_from_args(&mut argv).expect("parses");
        assert_eq!(
            resume,
            Some(HandoverResume {
                state_fd: 5,
                rollback_fd: Some(6)
            })
        );
        assert_eq!(argv, args(&["--socket", "/s"]));

        let mut plain = args(&["--socket", "/s"]);
        assert_eq!(HandoverResume::take_from_args(&mut plain), Ok(None));
        assert_eq!(plain, args(&["--socket", "/s"]));
    }

    #[test]
    fn malformed_resume_flags_are_errors() {
        for bad in [
            &["--resume-fd"][..],
            &["--resume-fd", "x"],
            &["--resume-fd", "-1"],
            &["--handover-rollback-fd", "4"],
            &["--resume-fd", "3", "--resume-fd", "4"],
        ] {
            let mut argv = args(bad);
            assert!(
                HandoverResume::take_from_args(&mut argv).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn the_verify_helper_ignores_other_command_lines() {
        assert_eq!(run_handover_verify_helper(args(&["--socket", "/s"])), None);
        assert_eq!(
            run_handover_verify_helper(args(&["--handover-verify"])),
            Some(2)
        );
    }
}
