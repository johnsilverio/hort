//! The ladder into a running sandbox's namespaces, for the host-side helpers
//! that have to work from inside them.
//!
//! A sandbox's network namespace is owned by a user namespace hort created and
//! then left, so no process lives in it. Joining a network namespace takes
//! privilege in the joiner's own user namespace as well as in the owning one, so
//! an unprivileged process on the host is refused outright and has to enter the
//! owning user namespace first. The kernel hands that namespace over from the
//! network namespace itself, which is what spares hort a process parked in it
//! purely to name it.
//!
//! Every climb happens in a forked child: creating or joining a user namespace
//! demands a single-threaded process, and hort's own process has to stay on the
//! host so the helpers it spawns keep reaching it.

use std::io::{self, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// The user namespace that owns `netns`, which is the one a host-side process
/// has to join before it can join `netns` itself.
pub fn owning_user_namespace(netns: BorrowedFd<'_>) -> Result<OwnedFd, String> {
    let owner = unsafe { libc::ioctl(netns.as_raw_fd(), libc::NS_GET_USERNS) };
    if owner == -1 {
        return Err(format!(
            "reading the owner of the sandbox network namespace: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(owner) })
}

/// Join each namespace in turn. The order is the ladder's: every rung is only
/// reachable with the privilege the rung below it grants.
pub fn enter(namespaces: &[BorrowedFd<'_>]) -> Result<(), String> {
    for namespace in namespaces {
        if unsafe { libc::setns(namespace.as_raw_fd(), 0) } == -1 {
            return Err(format!("joining a sandbox namespace: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Run `work` from inside `namespaces`, in a forked child, and report what it
/// made of it. The detail of a failure travels back as text, because an error
/// value cannot cross a process boundary.
pub fn in_namespaces(
    namespaces: &[BorrowedFd<'_>],
    work: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let (mut report_reader, report_writer) =
        io::pipe().map_err(|err| format!("creating a pipe: {err}"))?;

    match unsafe { libc::fork() } {
        -1 => Err(format!("forking into the sandbox namespaces: {}", io::Error::last_os_error())),
        0 => {
            drop(report_reader);
            report_and_exit(report_writer, enter(namespaces).and_then(|()| work()));
        }
        child => {
            drop(report_writer);
            let mut detail = String::new();
            let _ = report_reader.read_to_string(&mut detail);
            if exited_cleanly(child) {
                return Ok(());
            }
            if detail.is_empty() {
                detail = "the process died without reporting why".to_string();
            }
            Err(detail)
        }
    }
}

/// Hand the outcome to the parent and leave at once, without unwinding and
/// without at-exit handlers: everything they would clean up is still owned by the
/// process on the other side of the fork.
fn report_and_exit(mut report: PipeWriter, outcome: Result<(), String>) -> ! {
    let code = match outcome {
        Ok(()) => 0,
        Err(detail) => {
            let _ = report.write_all(detail.as_bytes());
            1
        }
    };
    drop(report);
    unsafe { libc::_exit(code) }
}

fn exited_cleanly(child: libc::pid_t) -> bool {
    let mut status = 0;
    if unsafe { libc::waitpid(child, &mut status, 0) } == -1 {
        return false;
    }
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}
