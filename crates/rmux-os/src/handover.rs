//! The system calls a live daemon handover is made of.
//!
//! A handover re-executes the daemon in place (`docs/design/live-handover.md`):
//! the state goes into a sealed memfd, the descriptors that must survive have
//! their `FD_CLOEXEC` cleared, and the process `execveat`s a binary it holds
//! an fd on. Everything here is a thin, checked wrapper over one of those
//! calls, kept in this crate because the server forbids `unsafe`.
//!
//! cctop ships statically linked musl binaries, so nothing here goes through a
//! glibc-only wrapper: the memfd, the seals and the descriptor flags are
//! rustix's raw syscalls, and `execveat` is `syscall(SYS_execveat)`, which
//! musl has had no wrapper for anyway.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

use rustix::fs::{fcntl_add_seals, memfd_create, MemfdFlags, SealFlags};
use rustix::io::{fcntl_getfd, fcntl_setfd, FdFlags};

/// Writes `bytes` into a new memfd and seals it, so neither this process nor
/// the image it execs into can change what the other reads.
///
/// The memfd is close-on-exec until [`set_inheritable`] says otherwise: a
/// verify subprocess gets it as its stdin, and only the final `execveat`
/// carries it by number.
pub fn sealed_memfd(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    let fd = memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
    let mut written = 0;
    while written < bytes.len() {
        written += rustix::io::write(&fd, &bytes[written..])?;
    }
    fcntl_add_seals(
        &fd,
        SealFlags::WRITE | SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL,
    )?;
    Ok(fd)
}

/// Reads a whole file from its start, whatever its shared offset says.
///
/// A memfd handed across an exec shares its open file description with every
/// dup of it, so a reader that relied on the offset would see whatever an
/// earlier reader left behind. `pread` from zero does not.
pub fn read_all_from_start(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = vec![0_u8; 1 << 20];
    loop {
        let read = rustix::io::pread(fd, &mut chunk, out.len() as u64)?;
        if read == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..read]);
    }
}

/// Takes ownership of a descriptor this process inherited across an exec, by
/// the number the previous image recorded for it.
///
/// The number is checked to be open first, so a wrong record is an error
/// rather than an fd that some later `open` would be handed and this one would
/// close. The fd is made close-on-exec again: a resumed daemon spawns panes,
/// and none of them should inherit another pane's pty master.
pub fn adopt_inherited_fd(raw: RawFd) -> io::Result<OwnedFd> {
    check_open(raw)?;
    // SAFETY: the number names an open descriptor (checked above) that the
    // previous image handed this one across the exec and that nothing in this
    // image has claimed: the caller adopts each recorded number exactly once,
    // before anything else could be given that number.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
    Ok(fd)
}

/// Looks at a descriptor this process inherited across an exec without
/// taking ownership of it, so it stays open, and inheritable, for an image
/// this one may exec into in turn (a rollback).
///
/// The borrow is `'static` because an inherited handover fd is never closed
/// until something adopts it, and nothing adopts a number it has borrowed.
pub fn borrow_inherited_fd(raw: RawFd) -> io::Result<BorrowedFd<'static>> {
    check_open(raw)?;
    // SAFETY: the number names an open descriptor (checked above) that stays
    // open: callers borrow a handover fd only on paths that end in an exec or
    // in process exit, never in adopting and closing the same number.
    Ok(unsafe { BorrowedFd::borrow_raw(raw) })
}

fn check_open(raw: RawFd) -> io::Result<()> {
    if raw < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("inherited fd {raw} is negative"),
        ));
    }
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry; it
    // never dereferences anything and fails with EBADF for a closed number.
    if unsafe { libc::fcntl(raw, libc::F_GETFD) } < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("inherited fd {raw} is not open: {error}"),
        ));
    }
    Ok(())
}

/// Whether a descriptor survives the next `execve`.
pub fn set_inheritable(fd: BorrowedFd<'_>, inheritable: bool) -> io::Result<()> {
    let mut flags = fcntl_getfd(fd)?;
    flags.set(FdFlags::CLOEXEC, !inheritable);
    fcntl_setfd(fd, flags)?;
    Ok(())
}

/// Opens this process's own executable for a later `execveat`.
///
/// `/proc/self/exe` resolves to the running image's inode even after its file
/// was replaced or deleted by an update, which is what makes a rollback into
/// the old binary possible after the new one is on disk.
pub fn open_own_executable() -> io::Result<OwnedFd> {
    let fd = rustix::fs::open(
        "/proc/self/exe",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    Ok(fd)
}

/// Opens a file for a later `execveat`, as [`open_own_executable`] does for
/// this process's own.
pub fn open_executable(path: &std::path::Path) -> io::Result<OwnedFd> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    Ok(fd)
}

/// Unblocks every signal on the calling thread.
///
/// `execve` keeps the calling thread's signal mask, and a daemon that resumed
/// with SIGCHLD or SIGTERM blocked would never hear its children die or be
/// asked to stop.
pub fn unblock_all_signals() -> io::Result<()> {
    // SAFETY: `sigemptyset` initialises the set it is given, and
    // `pthread_sigmask` reads that set and writes nothing back (null old set).
    unsafe {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        if libc::sigemptyset(set.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let set = set.assume_init();
        let status = libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
    }
    Ok(())
}

/// Replaces this process's image with the executable `fd` refers to.
///
/// Only returns on failure, and then the process is exactly as it was: the
/// kernel commits to the new image only once it has loaded it, so a binary it
/// cannot run (ENOEXEC, ENOMEM, a missing interpreter) leaves the caller in
/// place to carry on.
pub fn execveat_fd(
    fd: BorrowedFd<'_>,
    argv: &[OsString],
    envp: &[(OsString, OsString)],
) -> io::Error {
    let argv = match argv
        .iter()
        .map(|arg| c_string(arg))
        .collect::<io::Result<Vec<_>>>()
    {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let envp = match envp
        .iter()
        .map(|(name, value)| {
            let mut entry = name.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            c_string(OsStr::from_bytes(&entry))
        })
        .collect::<io::Result<Vec<_>>>()
    {
        Ok(envp) => envp,
        Err(error) => return error,
    };
    let mut argv_pointers: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_pointers.push(std::ptr::null());
    let mut envp_pointers: Vec<*const libc::c_char> =
        envp.iter().map(|entry| entry.as_ptr()).collect();
    envp_pointers.push(std::ptr::null());
    let empty = c"";
    // SAFETY: every pointer is to a NUL-terminated string owned by `argv`,
    // `envp` or `empty`, all alive across the call, and both arrays end with
    // a null pointer as execveat(2) requires. On success the call does not
    // return; on failure it changes nothing in this process.
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            fd.as_fd().as_raw_fd(),
            empty.as_ptr(),
            argv_pointers.as_ptr(),
            envp_pointers.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    io::Error::last_os_error()
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{value:?} contains a NUL byte and cannot be passed to exec"),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn a_sealed_memfd_reads_back_and_refuses_writes() {
        let fd = sealed_memfd("rmux-test", b"hello handover").expect("memfd");
        assert_eq!(
            read_all_from_start(fd.as_fd()).expect("read"),
            b"hello handover"
        );
        assert!(
            rustix::io::write(&fd, b"x").is_err(),
            "sealed against writes"
        );
    }

    #[test]
    fn inheritance_toggles_close_on_exec() {
        let fd = sealed_memfd("rmux-test", b"").expect("memfd");
        set_inheritable(fd.as_fd(), true).expect("clear");
        assert!(!fcntl_getfd(&fd).expect("flags").contains(FdFlags::CLOEXEC));
        set_inheritable(fd.as_fd(), false).expect("set");
        assert!(fcntl_getfd(&fd).expect("flags").contains(FdFlags::CLOEXEC));
    }

    #[test]
    fn adopting_a_closed_number_is_an_error_not_an_fd() {
        let fd = sealed_memfd("rmux-test", b"").expect("memfd");
        let raw = fd.as_raw_fd();
        drop(fd);
        assert!(adopt_inherited_fd(raw).is_err());
        assert!(adopt_inherited_fd(-1).is_err());
    }

    #[test]
    fn an_adopted_fd_is_close_on_exec_again() {
        let fd = sealed_memfd("rmux-test", b"x").expect("memfd");
        set_inheritable(fd.as_fd(), true).expect("clear");
        let raw = std::os::fd::IntoRawFd::into_raw_fd(fd);
        let adopted = adopt_inherited_fd(raw).expect("adopt");
        assert!(fcntl_getfd(&adopted)
            .expect("flags")
            .contains(FdFlags::CLOEXEC));
    }

    #[test]
    fn exec_of_something_that_is_not_a_binary_returns_and_changes_nothing() {
        let fd = sealed_memfd("rmux-test", b"not an executable").expect("memfd");
        let error = execveat_fd(fd.as_fd(), &["x".into()], &[]);
        // A memfd without the exec bit fails with EACCES; one with it but
        // no valid format, ENOEXEC. Either way, the call came back.
        assert!(error.raw_os_error().is_some(), "{error}");
    }
}
