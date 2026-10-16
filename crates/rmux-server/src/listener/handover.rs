//! The old image's half of a live handover: quiesce, snapshot, verify, tell
//! the clients, exec (docs/design/live-handover.md, §2).
//!
//! It runs inside the accept loop, so while it runs nothing is accepted and
//! a client that connects waits in the listen backlog, which the exec
//! carries. It returns only when the handover did not happen; then the
//! daemon is as it was, except after a failed `execveat` itself, by which
//! point the clients had been told to go.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rmux_ipc::LocalListener;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::handler::RequestHandler;
use crate::handover::api::{current_environment, HandoverError, HandoverErrorKind};
use crate::handover::record::{
    append_record, ResumeRecordV1, RESUME_FD_FLAG, ROLLBACK_FD_FLAG, VERIFY_FLAG,
};
use crate::handover::{encode, HandoverOptions, HandoverResume};
use crate::pane_reader_runtime::PaneReaderGate;

/// A request for a handover, sent to the accept loop.
#[derive(Debug)]
pub(crate) struct HandoverCommand {
    pub(crate) options: HandoverOptions,
    pub(crate) reply: oneshot::Sender<HandoverError>,
}

/// How often quiesce re-checks a condition it is waiting on. Short, because
/// every pane is frozen while it waits.
const QUIESCE_POLL: Duration = Duration::from_millis(2);

/// What the accept loop lends a handover.
pub(super) struct HandoverRun<'a> {
    pub(super) handler: &'a Arc<RequestHandler>,
    pub(super) listener: &'a LocalListener,
    pub(super) socket_path: &'a Path,
    pub(super) configured_command: Option<&'a [OsString]>,
    pub(super) connection_shutdown: &'a mut watch::Sender<()>,
    pub(super) connection_shutdown_rx: &'a mut watch::Receiver<()>,
    pub(super) connection_tasks: &'a mut JoinSet<io::Result<()>>,
}

/// Runs one handover. Returns only if it did not happen.
pub(super) async fn run(run: HandoverRun<'_>, options: HandoverOptions) -> HandoverError {
    let HandoverOptions {
        deadline,
        executable,
        command,
    } = options;
    let started = Instant::now();
    let deadline = started + deadline;
    let error = match prepare(&run, executable, command).await {
        Ok(prepared) => commit(run, prepared, deadline).await,
        Err(error) => error,
    };
    warn!(
        elapsed = ?started.elapsed(),
        "live handover did not happen; the daemon carries on: {error}"
    );
    error
}

struct Prepared {
    target: OwnedFd,
    rollback: OwnedFd,
    command: Vec<OsString>,
}

/// Admission: everything that can refuse without changing anything.
async fn prepare(
    run: &HandoverRun<'_>,
    executable: Option<OwnedFd>,
    command: Option<Vec<OsString>>,
) -> Result<Prepared, HandoverError> {
    let command = command
        .or_else(|| run.configured_command.map(<[OsString]>::to_vec))
        .filter(|command| !command.is_empty())
        .ok_or_else(|| {
            HandoverError::refused(
                "this daemon was not started with a command line to hand over with",
            )
        })?;
    if let Some(reason) = run.handler.handover_refusal().await {
        return Err(HandoverError::refused(reason));
    }
    let rollback = rmux_os::handover::open_own_executable().map_err(|error| {
        HandoverError::refused(format!("cannot open this daemon's own executable: {error}"))
    })?;
    let target = match executable {
        Some(target) => target,
        None => rmux_os::handover::open_own_executable().map_err(|error| {
            HandoverError::refused(format!(
                "cannot open the executable to hand over to: {error}"
            ))
        })?,
    };
    Ok(Prepared {
        target,
        rollback,
        command,
    })
}

/// Lets parked readers go again on every way out of [`commit`] but the exec.
struct ReleaseReaders(Option<Arc<PaneReaderGate>>);

impl Drop for ReleaseReaders {
    fn drop(&mut self) {
        if let Some(gate) = &self.0 {
            gate.release();
        }
    }
}

fn deadline_error(what: &str) -> HandoverError {
    HandoverError::new(HandoverErrorKind::DeadlineExceeded, what)
}

async fn commit(run: HandoverRun<'_>, prepared: Prepared, deadline: Instant) -> HandoverError {
    let HandoverRun {
        handler,
        listener,
        socket_path,
        configured_command: _,
        connection_shutdown,
        connection_shutdown_rx,
        connection_tasks,
    } = run;
    let Prepared {
        target,
        rollback,
        command,
    } = prepared;

    // In-flight commands first, while the readers still run: a command may be
    // waiting on its pane's output.
    if !wait_until(deadline, || handler.normal_drain_requests_quiesced()).await {
        return deadline_error("in-flight commands did not finish before the deadline");
    }

    // Then the readers, without the state lock: a reader's callbacks may want
    // it, and a reader that cannot finish its iteration cannot park.
    let gate = handler.pane_reader_gate().await;
    let _release = ReleaseReaders(gate.clone());
    if let Some(gate) = &gate {
        if !gate.park_all(deadline).await {
            return deadline_error("pane readers did not park before the deadline");
        }
    }

    // The state lock is held from the snapshot to the exec, so a mutation
    // admitted after the quiesce cannot change what the snapshot missed: it
    // waits for the lock and is gone with the exec, or runs after an abort.
    let state = loop {
        let Ok(state) = tokio::time::timeout_at(deadline, handler.lock_state_for_handover()).await
        else {
            return deadline_error("the daemon state stayed locked past the deadline");
        };
        if handler.normal_drain_requests_quiesced() {
            break state;
        }
        drop(state);
        if Instant::now() >= deadline {
            return deadline_error("commands kept arriving until the deadline");
        }
        tokio::time::sleep(QUIESCE_POLL).await;
    };
    // Admission looked before the quiesce; a command that ran since may have
    // started something the state cannot carry.
    if let Some(reason) = handler
        .handover_runtime_refusal()
        .or_else(|| state.handover_refusal())
    {
        return HandoverError::refused(reason);
    }

    let snapshot = match state.to_handover() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return HandoverError::refused(format!("the daemon state cannot be captured: {error}"))
        }
    };
    let pane_fds: Vec<i32> = snapshot.header.pane_fds.iter().map(|(_, fd)| *fd).collect();
    let mut bytes = encode(&snapshot);
    drop(snapshot);
    append_record(
        &mut bytes,
        &ResumeRecordV1 {
            listener_fd: listener.as_fd().as_raw_fd(),
            socket_path: socket_path.as_os_str().to_owned().into_vec(),
            rollback_argv: own_command_line(),
        },
    );
    let memfd = match rmux_os::handover::sealed_memfd("rmux-handover", &bytes) {
        Ok(memfd) => memfd,
        Err(error) => {
            return HandoverError::refused(format!("cannot write the state to a memfd: {error}"))
        }
    };
    let state_bytes = bytes.len();
    drop(bytes);
    if Instant::now() >= deadline {
        return deadline_error("the snapshot ran past the deadline");
    }

    if let Err(error) = verify(&target, &command, &memfd, deadline).await {
        return error;
    }

    info!(
        state_bytes,
        panes = pane_fds.len(),
        "live handover verified; telling clients and executing"
    );
    // Clients go now, with the reason they will print. Past this point a
    // failure no longer leaves the daemon exactly as it was: only the
    // `execveat` itself can still fail, and then the clients have gone.
    handler.set_server_upgrading(true);
    connection_shutdown.send_replace(());
    super::drain_connection_tasks_for_shutdown(connection_tasks).await;

    let error = exec(
        &target,
        &rollback,
        command,
        &memfd,
        listener.as_fd(),
        &pane_fds,
    );

    // Still here: the kernel refused the new image before replacing this one.
    handler.set_server_upgrading(false);
    let (sender, receiver) = watch::channel(());
    *connection_shutdown = sender;
    *connection_shutdown_rx = receiver;
    drop(state);
    HandoverError::new(
        HandoverErrorKind::ExecFailed,
        format!("execveat refused the new image: {error}"),
    )
}

async fn wait_until(deadline: Instant, mut ready: impl FnMut() -> bool) -> bool {
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(QUIESCE_POLL).await;
    }
}

/// This image's command line, without a resume it may itself have come from:
/// the rollback adds its own.
fn own_command_line() -> Vec<Vec<u8>> {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    let program = if args.is_empty() {
        OsString::from("rmux")
    } else {
        args.remove(0)
    };
    // A malformed flag here would have stopped this image from starting.
    let _ = HandoverResume::take_from_args(&mut args);
    std::iter::once(program)
        .chain(args)
        .map(OsString::into_vec)
        .collect()
}

/// Runs the new binary on the state, as a separate process that serves
/// nothing, and waits for its verdict.
async fn verify(
    target: &OwnedFd,
    command: &[OsString],
    memfd: &OwnedFd,
    deadline: Instant,
) -> Result<(), HandoverError> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let verify_failed =
        |reason: String| HandoverError::new(HandoverErrorKind::VerifyFailed, reason);
    // The executable is named through this process's fd table, so it is the
    // file the fd was opened on even if its path now holds another binary.
    let program = format!("/proc/{}/fd/{}", std::process::id(), target.as_raw_fd());
    let stdin = memfd
        .try_clone()
        .map_err(|error| verify_failed(format!("cannot pass the state to verify: {error}")))?;
    let mut child = Command::new(program)
        .arg0(&command[0])
        .args(&command[1..])
        .arg(VERIFY_FLAG)
        .arg("0")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| verify_failed(format!("cannot start the new binary: {error}")))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let verdict = tokio::task::spawn_blocking(move || {
        let until = std::time::Instant::now() + remaining;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        let _ = io::Read::read_to_string(&mut pipe, &mut stderr);
                    }
                    return Ok((status, stderr));
                }
                Ok(None) if std::time::Instant::now() >= until => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(None);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(error) => return Err(Some(error)),
            }
        }
    })
    .await
    .map_err(|error| verify_failed(format!("the verify wait failed: {error}")))?;
    match verdict {
        Ok((status, _)) if status.success() => Ok(()),
        Ok((status, stderr)) => Err(verify_failed(format!(
            "the new binary cannot resume this state ({status}): {}",
            stderr.trim()
        ))),
        Err(None) => Err(deadline_error(
            "the new binary's verify ran past the deadline",
        )),
        Err(Some(error)) => Err(verify_failed(format!("cannot wait for verify: {error}"))),
    }
}

/// Makes the carried fds survive the exec and execs. Returns only on failure,
/// with every fd close-on-exec again.
fn exec(
    target: &OwnedFd,
    rollback: &OwnedFd,
    mut command: Vec<OsString>,
    memfd: &OwnedFd,
    listener: BorrowedFd<'_>,
    pane_fds: &[i32],
) -> io::Error {
    let mut carried: Vec<BorrowedFd<'_>> = vec![memfd.as_fd(), rollback.as_fd(), listener];
    for fd in pane_fds {
        match rmux_os::handover::borrow_inherited_fd(*fd) {
            Ok(fd) => carried.push(fd),
            Err(error) => return error,
        }
    }
    let mut inherited = Vec::with_capacity(carried.len());
    let mut failure = None;
    for fd in &carried {
        match rmux_os::handover::set_inheritable(*fd, true) {
            Ok(()) => inherited.push(*fd),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let error = match failure {
        Some(error) => error,
        None => {
            command.push(RESUME_FD_FLAG.into());
            command.push(memfd.as_raw_fd().to_string().into());
            command.push(ROLLBACK_FD_FLAG.into());
            command.push(rollback.as_raw_fd().to_string().into());
            let _ = rmux_os::handover::unblock_all_signals();
            rmux_os::handover::execveat_fd(target.as_fd(), &command, &current_environment())
        }
    };
    for fd in inherited {
        let _ = rmux_os::handover::set_inheritable(fd, false);
    }
    error
}
