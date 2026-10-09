// The signal plumbing goes through libc, not rustix. It used rustix's
// `runtime` module, which rustix documents as being for libc implementations
// only. rustix 1.1.5 renamed it to a mangled name and made `rustix::runtime`
// crate-private, so this crate stopped building wherever 1.1.5 got resolved:
// `cargo install` without `--locked`, cargo-semver-checks, and any dependent
// whose lock moved forward.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::thread::JoinHandleExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use rmux_proto::TerminalGeometry;

use super::terminal_geometry_from_fd;
use crate::ClientError;

/// A signal set holding SIGWINCH alone.
fn winch_set() -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initialises the set it is given; sigaddset then
    // adds a valid signal number to that initialised set. Neither can fail
    // for these arguments.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGWINCH);
        set.assume_init()
    }
}

/// The pthread functions return their error number rather than setting errno.
fn pthread_result(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

#[derive(Debug)]
pub(in crate::attach) struct SignalMaskGuard {
    previous: libc::sigset_t,
}

impl SignalMaskGuard {
    pub(in crate::attach) fn block_winch() -> super::Result<Self> {
        let signals = winch_set();
        let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: both pointers are valid for the call; on success the kernel
        // has written the previous mask into `previous`.
        pthread_result(unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &signals, previous.as_mut_ptr())
        })?;
        // SAFETY: initialised by the successful call above.
        let previous = unsafe { previous.assume_init() };
        Ok(Self { previous })
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        // SAFETY: This restores the exact mask returned by the earlier successful call.
        let _ = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
    }
}

#[derive(Debug)]
pub(in crate::attach) struct ResizeWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ResizeWatcher {
    pub(in crate::attach) fn spawn(
        terminal_fd: OwnedFd,
        resize_tx: mpsc::Sender<TerminalGeometry>,
    ) -> std::result::Result<Self, ClientError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            let signals = winch_set();

            loop {
                let mut signal: libc::c_int = 0;
                // SAFETY: Only SIGWINCH is waited on, and this thread inherits a blocked mask for it.
                if unsafe { libc::sigwait(&signals, &mut signal) } != 0 {
                    return;
                }

                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }

                if signal == libc::SIGWINCH {
                    let geometry = match terminal_geometry_from_fd(&terminal_fd) {
                        Ok(Some(geometry)) => geometry,
                        Ok(None) => continue,
                        Err(_) => return,
                    };

                    if resize_tx.send(geometry).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Sends SIGWINCH to the watcher thread alone.
    ///
    /// Addressed by its pthread handle, which stays valid until the thread is
    /// joined, so no kernel thread id has to be handed back from the thread
    /// first, and a thread that has already returned is still a valid target.
    fn signal_watcher(&self) -> io::Result<()> {
        let Some(thread) = &self.thread else {
            return Ok(());
        };
        // SAFETY: the handle has not been joined, so its pthread_t is live,
        // and SIGWINCH is the signal the watcher waits on. The cast is the
        // identity on glibc; on musl libc's pthread_t is a pointer while std
        // hands the handle back as a u64 (see `interrupt_thread`).
        let handle = thread.as_pthread_t() as libc::pthread_t;
        pthread_result(unsafe { libc::pthread_kill(handle, libc::SIGWINCH) })
    }

    #[cfg(test)]
    pub(in crate::attach) fn notify_for_test(&self) -> io::Result<()> {
        self.signal_watcher()
    }
}

impl Drop for ResizeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.signal_watcher();

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
