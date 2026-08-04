//! What the processes hort leaves running for a sandbox have in common: how one
//! is started and stopped, and how one carries bytes between two connections.
//!
//! A helper outlives the command that started it, so what it is has to be
//! recognizable later: a pid is reusable, and by the time a sandbox goes down the
//! process a file names may be something else entirely. So a helper says what it
//! is in the process table before anything records it, and nothing is signalled
//! without asking again.
//!
//! A helper is forked rather than executed, and that is what makes the descriptor
//! sweep load-bearing. `O_CLOEXEC` only protects what execs, so a fork keeps every
//! open file of the process that forked it, one of which is the lock hort holds
//! while it builds a sandbox: held by a process that only leaves when the sandbox
//! does, it makes the name read as still being built for as long as the sandbox
//! lives.

use std::fs::{self, File};
use std::io::{self, PipeWriter, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::thread;

use crate::adapters::streams::open_sandbox_log;

const DEV_NULL: &str = "/dev/null";
/// The longest name the kernel keeps for a process, counting the terminator.
const PROCESS_NAME_LIMIT: usize = 16;
/// The byte a started helper announces itself with; only its arrival, never its
/// value, carries meaning, and the closed pipe that yields none is the failure.
const HANDSHAKE: [u8; 1] = [1];

/// One kind of host-side process a sandbox leaves behind: what it calls itself in
/// the process table, and the file recording the one this sandbox has running.
pub struct HostHelper {
    process_name: &'static str,
    pid_file: &'static str,
}

impl HostHelper {
    /// A helper known by this name in the process table, recorded in this file
    /// inside a sandbox's own directory.
    pub const fn new(process_name: &'static str, pid_file: &'static str) -> Self {
        // A name the kernel cuts short is a name no teardown can recognize, and
        // recognition is the whole of what stands between a stop and a stranger.
        assert!(process_name.len() < PROCESS_NAME_LIMIT);
        Self { process_name, pid_file }
    }

    /// Start one in a process that outlives this call, serving with `kept` as the
    /// only descriptors it carries over from here, and record what it is. One
    /// that cannot be recorded is stopped rather than left running: nothing else
    /// knows it exists.
    pub fn start(
        &self,
        sandbox_dir: &Path,
        kept: &[RawFd],
        serve: impl FnOnce(),
    ) -> Result<(), String> {
        let streams = helper_streams(sandbox_dir)?;
        let (mut announcement, announce) =
            io::pipe().map_err(|err| format!("creating a pipe: {err}"))?;
        let mut kept = kept.to_vec();

        match unsafe { libc::fork() } {
            -1 => Err(format!("forking {}: {}", self.process_name, io::Error::last_os_error())),
            0 => {
                drop(announcement);
                serve_detached(self.process_name, &mut kept, streams, announce, serve)
            }
            child => {
                drop(announce);
                // Whatever this side keeps of a listener keeps its port bound, so
                // a helper stopped later would leave something still answering
                // for it.
                drop(serve);
                drop(streams);
                let mut signal = [0u8; 1];
                match announcement.read(&mut signal) {
                    Ok(0) | Err(_) => {
                        discard(child);
                        Err(format!("{} never started", self.process_name))
                    }
                    Ok(_) => match self.record(sandbox_dir, child) {
                        Ok(()) => Ok(()),
                        Err(failure) => {
                            // Nothing else knows this process exists, so one left
                            // running here is one nothing can ever stop.
                            discard(child);
                            Err(failure)
                        }
                    },
                }
            }
        }
    }

    /// Stop the one this sandbox has running, if the recorded process is still
    /// it. Stopping a sandbox that never had one is not a failure.
    pub fn stop(&self, sandbox_dir: &Path) -> Result<(), String> {
        let pid_file = self.pid_file_in(sandbox_dir);
        let recorded = fs::read_to_string(&pid_file).ok().and_then(|pid| pid.trim().parse().ok());
        let outcome = match recorded {
            // A pid outlives the process it named, so the recorded one is only
            // acted on while it still names what was recorded.
            Some(pid) if self.names(pid) => self.signal(pid),
            _ => Ok(()),
        };
        let _ = fs::remove_file(&pid_file);
        outcome
    }

    fn pid_file_in(&self, sandbox_dir: &Path) -> PathBuf {
        sandbox_dir.join(self.pid_file)
    }

    fn record(&self, sandbox_dir: &Path, pid: libc::pid_t) -> Result<(), String> {
        let pid_file = self.pid_file_in(sandbox_dir);
        fs::write(&pid_file, format!("{pid}\n"))
            .map_err(|err| format!("writing {}: {err}", pid_file.display()))
    }

    fn names(&self, pid: libc::pid_t) -> bool {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|name| name.trim() == self.process_name)
    }

    fn signal(&self, pid: libc::pid_t) -> Result<(), String> {
        if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
            let failure = io::Error::last_os_error();
            // The process leaving between being recognized and being signalled is
            // the outcome this asked for, not a failure to produce it.
            if failure.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("stopping {} ({pid}): {failure}", self.process_name));
            }
        }
        Ok(())
    }
}

/// Copy both directions until either side is done with the other. A connection
/// that carried one direction would deliver a request and never its reply.
pub fn splice(client: &TcpStream, upstream: &TcpStream) {
    thread::scope(|directions| {
        directions.spawn(|| {
            let _ = io::copy(&mut &*client, &mut &*upstream);
            let _ = upstream.shutdown(Shutdown::Write);
        });
        let _ = io::copy(&mut &*upstream, &mut &*client);
        // Ending both sides is what releases the direction still being copied:
        // one side reaching its end leaves nothing for the other to carry.
        let _ = client.shutdown(Shutdown::Both);
        let _ = upstream.shutdown(Shutdown::Both);
    });
}

/// Serve from the forked child, after making it into something the host can
/// recognize and something that holds nothing of the command that forked it.
/// Leaving without unwinding matters as much as in any forked child: what the
/// unwinding would clean up belongs to the process on the other side of the fork.
fn serve_detached(
    process_name: &str,
    kept: &mut [RawFd],
    streams: HelperStreams,
    announce: PipeWriter,
    serve: impl FnOnce(),
) -> ! {
    if name_process(process_name).is_err()
        || redirect(streams).is_err()
        || announced(announce).is_err()
    {
        unsafe { libc::_exit(1) }
    }
    close_inherited_descriptors(kept);
    serve();
    unsafe { libc::_exit(0) }
}

/// The streams a helper is given: the sandbox's log for both outputs, and nothing
/// to read.
struct HelperStreams {
    input: File,
    output: File,
    errors: File,
}

/// Open them from hort's own process, before the fork. A helper keeps whatever it
/// is started with for as long as the sandbox lives, so one started with hort's
/// streams holds a redirected or piped invocation open forever, and holds the
/// writing end of whatever feeds hort open just as long.
fn helper_streams(sandbox_dir: &Path) -> Result<HelperStreams, String> {
    Ok(HelperStreams {
        input: File::open(DEV_NULL).map_err(|err| format!("opening {DEV_NULL}: {err}"))?,
        output: open_sandbox_log(sandbox_dir)?,
        errors: open_sandbox_log(sandbox_dir)?,
    })
}

/// Put the streams in place of the ones the fork was inherited with. Taking them
/// by value is what closes the originals here, before anything else is closed: a
/// descriptor closed later could already have been handed to a connection.
fn redirect(streams: HelperStreams) -> Result<(), ()> {
    let placed = [
        (streams.input.as_raw_fd(), 0),
        (streams.output.as_raw_fd(), 1),
        (streams.errors.as_raw_fd(), 2),
    ];
    for (stream, standard) in placed {
        if unsafe { libc::dup2(stream, standard) } == -1 {
            return Err(());
        }
    }
    Ok(())
}

/// Say in the process table what this process is, so a teardown can tell that the
/// pid it holds still names the helper and not whatever inherited the number.
fn name_process(name: &str) -> Result<(), ()> {
    let mut named = [0u8; PROCESS_NAME_LIMIT];
    named[..name.len()].copy_from_slice(name.as_bytes());
    if unsafe { libc::prctl(libc::PR_SET_NAME, named.as_ptr()) } == -1 {
        return Err(());
    }
    Ok(())
}

fn announced(mut announce: PipeWriter) -> io::Result<()> {
    announce.write_all(&HANDSHAKE)
}

/// Close everything this process inherited except the descriptors it was left to
/// serve with.
fn close_inherited_descriptors(kept: &mut [RawFd]) {
    kept.sort_unstable();
    let mut first = 3;
    for descriptor in kept.iter() {
        if *descriptor < first {
            continue;
        }
        close_descriptors(first, *descriptor - 1);
        first = *descriptor + 1;
    }
    close_descriptors(first, RawFd::MAX);
}

fn close_descriptors(first: RawFd, last: RawFd) {
    unsafe { libc::close_range(first as libc::c_uint, last as libc::c_uint, 0) };
}

/// A port a test can declare a database or a service on, taken from below the
/// range the kernel hands out on its own. Binding `:0` and letting the listener
/// go looks like it reserves a port and does not: the number goes straight back
/// into the pool, and the next `:0` bind anywhere in the suite can be given it,
/// which turns every test that declares a port into a race against its siblings.
#[cfg(test)]
pub(crate) fn a_declared_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};

    /// Below `net.ipv4.ip_local_port_range`, so nothing the kernel assigns lands
    /// here, and stepping forward so no two tests in one run declare the same.
    static NEXT: AtomicU16 = AtomicU16::new(21000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Take back a helper that was started but could not be handed over.
fn discard(pid: libc::pid_t) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}
