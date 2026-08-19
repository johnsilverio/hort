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
    /// only descriptors it carries over from here, and record what it is. By the
    /// time this returns the started process has already let go of everything
    /// else it inherited. One that cannot be recorded is stopped rather than left
    /// running: nothing else knows it exists.
    pub fn start(
        &self,
        sandbox_dir: &Path,
        kept: &[RawFd],
        serve: impl FnOnce(),
    ) -> Result<(), String> {
        let streams = helper_streams(sandbox_dir)?;
        let (mut announcement, announce) =
            io::pipe().map_err(|err| format!("creating a pipe: {err}"))?;
        let mut carried = carried_through_the_sweep(kept, &streams, &announce);

        match unsafe { libc::fork() } {
            -1 => Err(format!("forking {}: {}", self.process_name, io::Error::last_os_error())),
            0 => {
                drop(announcement);
                serve_detached(self.process_name, &mut carried, streams, announce, serve)
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
            Some(pid) if self.names(pid) => {
                let signalled = self.signal(pid);
                collect(pid);
                signalled
            }
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
    carried: &mut [RawFd],
    streams: HelperStreams,
    announce: PipeWriter,
    serve: impl FnOnce(),
) -> ! {
    // Swept before anything else, because until this line the child holds every
    // descriptor the forking process had open, including files another of its
    // threads has in hand. Nothing written here closes that interval: the kernel
    // copies the table inside the fork, so the child is already holding all of it
    // while the rest of the fork runs and while it waits for a CPU, which is far
    // longer than its own path to this call. The announcement comes last for the
    // opposite reason, since it is what releases the caller.
    //
    // No test pins this order, and that is a finding rather than a gap. One was
    // written and deleted: all it can observe is whether a file the caller let
    // go of is still held, and a child forked by any other thread holds that
    // same file for the same instant, which is a different race and one no
    // ordering here reaches. Beside its siblings it went red against this order
    // and the wrong one alike; alone it went green against both. What keeps the
    // order right is that it cannot be written wrong: an announcement cannot
    // come before a sweep that is the first line.
    close_inherited_descriptors(carried);
    if name_process(process_name).is_err()
        || redirect(streams).is_err()
        || announced(announce).is_err()
    {
        unsafe { libc::_exit(1) }
    }
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

impl HelperStreams {
    fn descriptors(&self) -> [RawFd; 3] {
        [self.input.as_raw_fd(), self.output.as_raw_fd(), self.errors.as_raw_fd()]
    }
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

/// What the sweep leaves in place: the descriptors the helper was left to serve
/// with, the streams the redirect still has to put in place of the inherited
/// ones, and the one the helper announces itself over. Assembled before the fork
/// so that sweeping can be the child's first act, and the announcement belongs in
/// it because a sweep that closed it would reach the caller as a helper that
/// never started.
fn carried_through_the_sweep(
    kept: &[RawFd],
    streams: &HelperStreams,
    announce: &PipeWriter,
) -> Vec<RawFd> {
    let mut carried = kept.to_vec();
    carried.extend(streams.descriptors());
    carried.push(announce.as_raw_fd());
    carried
}

/// Close everything this process inherited except what it was left to carry over.
fn close_inherited_descriptors(carried: &mut [RawFd]) {
    carried.sort_unstable();
    let mut first = 3;
    for descriptor in carried.iter() {
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

/// A port a test can declare a database or a service on: one nothing else on the
/// machine is answering on, and one no other test process will be given for as
/// long as this one runs. Binding `:0` and letting the listener go looks like it
/// reserves a port and does not: the number goes straight back into the pool, and
/// the next `:0` bind anywhere in the suite can be given it, which turns every
/// test that declares a port into a race against its siblings.
///
/// What is held is the number and never the socket, because the two kinds of
/// caller need opposite things from it: some listen on it themselves, and the
/// rest need it free for hort's own forwarder to bind.
#[cfg(test)]
pub(crate) fn a_declared_port() -> u16 {
    use std::net::TcpStream;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    use std::sync::Mutex;

    /// Below `net.ipv4.ip_local_port_range`, so nothing the kernel assigns on its
    /// own lands here.
    const FIRST: u16 = 21000;
    const LAST: u16 = 21999;
    /// What this process is holding, kept until it exits however it exits. A
    /// number given back earlier is one a sibling process may hand out while the
    /// test that asked for it is still using it.
    static HELD: Mutex<Vec<UnixListener>> = Mutex::new(Vec::new());

    /// Hold the number against every other process on the machine. A counter is
    /// the same guess in every process, and the kernel is the only thing that can
    /// settle it for processes that cannot see each other; this asks it by a name
    /// that exists nowhere on disk and that it takes back when the holder dies.
    fn hold(port: u16) -> Option<UnixListener> {
        let name = SocketAddr::from_abstract_name(format!("hort-declared-port-{port}")).ok()?;
        UnixListener::bind_addr(&name).ok()
    }

    for port in FIRST..=LAST {
        let Some(held) = hold(port) else { continue };
        // Asked by binding instead, this holds the port for the moment it takes
        // to answer, and every helper forked in that moment inherits the listener
        // and keeps the port it has just called free (measured: 1 red in 5).
        let answering = TcpStream::connect(("127.0.0.1", port)).is_ok();
        if answering {
            continue;
        }
        HELD.lock().expect("the ports this process holds").push(held);
        return port;
    }
    panic!("no port between {FIRST} and {LAST} is free to declare");
}

/// Take the exit status of a helper this process is the one that started, so the
/// stopped process leaves the table instead of staying in it as a corpse that
/// still answers to its name. Usually there is nothing to take: a sandbox is
/// normally stopped by a later run of hort, which is not the parent of anything
/// the earlier one forked.
fn collect(pid: libc::pid_t) {
    let mut status = 0;
    // Asked without waiting first, because a blocking wait on a process that is
    // not this one's child is the one arrangement that would never return.
    if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == 0 {
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
}

/// Take back a helper that was started but could not be handed over.
fn discard(pid: libc::pid_t) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::TcpListener;
    use std::thread::sleep;
    use std::time::Duration;

    /// A helper of no particular kind, so what the whole family is started under
    /// is asked of the family and not of one of its four members.
    const A_HELPER: HostHelper = HostHelper::new("hort-witness", "witness.pid");
    /// How long a started helper stays in the process table. Bounded rather than
    /// endless because a test that dies on an assertion never reaches the stop,
    /// and what it leaves behind should leave on its own.
    const LINGERS: Duration = Duration::from_secs(10);

    #[test]
    fn a_helper_given_descriptors_to_serve_with_is_reported_as_started() {
        let sandbox = tempfile::tempdir().unwrap();
        let served = TcpListener::bind("127.0.0.1:0").unwrap();

        let started = A_HELPER.start(sandbox.path(), &[served.as_raw_fd()], || sleep(LINGERS));

        // A caller learns a helper started by hearing from it over a descriptor
        // of its own, and that descriptor is in nobody's keeping. Swept away
        // along with everything else inherited, it reaches the caller as a closed
        // pipe, and a helper that is already serving is killed as one that never
        // started.
        assert!(started.is_ok());

        A_HELPER.stop(sandbox.path()).unwrap();
    }
}
