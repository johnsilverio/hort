//! The user's terminal, lent to one session and taken back.
//!
//! A session with a pty of its own is two streams and a size: what the user
//! types goes to the sandbox's pty master, what the sandbox writes comes back to
//! the terminal, and a resized window is passed on so what runs inside redraws
//! at the new size. hort's own terminal is put into raw mode for the duration,
//! because the session's pty is the one doing the echoing and the line editing,
//! and a ^C typed there has to reach the sandbox rather than kill hort.
//!
//! Whatever happens, the terminal is handed back the way it was found. A hort
//! that dies with the terminal still raw leaves a shell that no longer echoes
//! what is typed into it.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::domain::error::HortError;
use crate::ports::{Session, SessionTerminal};

/// How much is carried in one direction at a time.
const RELAY_BYTES: usize = 8 * 1024;

/// Relays a session over the terminal hort itself was invoked on.
pub struct HostTerminal;

impl SessionTerminal for HostTerminal {
    fn relay(&self, session: Session) -> Result<i32, HortError> {
        relay_session(session).map_err(|detail| HortError::SessionTerminalFailed { detail })
    }
}

fn relay_session(session: Session) -> Result<i32, String> {
    if let Some(master) = &session.pty {
        carry_between(io::stdin().as_raw_fd(), master.as_fd())?;
    }
    wait_for(session.pid)
}

/// Carry each side to the other until the session's terminal ends, which is what
/// the kernel reports once nothing inside the sandbox holds it any longer.
fn carry_between(terminal: RawFd, master: BorrowedFd<'_>) -> Result<(), String> {
    let master = master.as_raw_fd();
    let output = io::stdout().as_raw_fd();
    let _raw = RawMode::enter(terminal)?;
    watch_window_changes();
    resize(terminal, master);

    let mut watched = [
        libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: terminal, events: libc::POLLIN, revents: 0 },
    ];
    loop {
        if WINDOW_CHANGED.swap(false, Ordering::Relaxed) {
            resize(terminal, master);
        }
        if unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, -1) } == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("waiting on the session terminal: {err}"));
        }
        if watched[0].revents != 0 && !carry(master, output)? {
            return Ok(());
        }
        // Only the session's side ends the relay. A terminal that stops giving
        // input is one nobody is typing into any more, and the session it was
        // opened for keeps running and keeps being shown.
        if watched[1].revents != 0 && !carry(terminal, master)? {
            watched[1].fd = -1;
        }
    }
}

/// Move whatever is waiting on `from` over to `to`. `false` means the source has
/// ended, which for a pty master is how the kernel reports the last thing on the
/// other side of it going away.
fn carry(from: RawFd, to: RawFd) -> Result<bool, String> {
    let mut buffer = [0u8; RELAY_BYTES];
    let read = unsafe { libc::read(from, buffer.as_mut_ptr().cast(), buffer.len()) };
    if read == 0 {
        return Ok(false);
    }
    if read == -1 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EINTR) => Ok(true),
            Some(libc::EIO) => Ok(false),
            _ => Err(format!("reading from the session terminal: {err}")),
        };
    }
    write_all(to, &buffer[..read as usize]).map(|()| true)
}

fn write_all(to: RawFd, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(to, bytes.as_ptr().cast(), bytes.len()) };
        if written == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("writing to the session terminal: {err}"));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

/// The status the kernel reports for the session, which is what hort itself
/// leaves with. Waiting is the whole of the relay for a session that asked for no
/// terminal: without it hort returns to the prompt while the shell it opened is
/// still running.
fn wait_for(session: u32) -> Result<i32, String> {
    let mut status = 0;
    loop {
        if unsafe { libc::waitpid(session as libc::pid_t, &mut status, 0) } != -1 {
            return Ok(status);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!("waiting for the session to end: {err}"));
    }
}

/// hort's own terminal for the length of a session, put back the way it was found
/// when this is dropped, including on the way out of a failure.
struct RawMode {
    terminal: RawFd,
    found: libc::termios,
}

impl RawMode {
    fn enter(terminal: RawFd) -> Result<Self, String> {
        let mut found: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(terminal, &mut found) } == -1 {
            return Err(format!("reading the terminal settings: {}", io::Error::last_os_error()));
        }
        let mut raw = found;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(terminal, libc::TCSANOW, &raw) } == -1 {
            return Err(format!("holding the terminal: {}", io::Error::last_os_error()));
        }
        Ok(Self { terminal, found })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.terminal, libc::TCSANOW, &self.found) };
    }
}

static WINDOW_CHANGED: AtomicBool = AtomicBool::new(false);

extern "C" fn note_window_change(_signal: libc::c_int) {
    WINDOW_CHANGED.store(true, Ordering::Relaxed);
}

/// Ask to be told when the terminal is resized. The handler is installed without
/// asking for interrupted waits to be restarted, so the wait for input comes back
/// and the new size is passed on before the next keystroke rather than after it.
fn watch_window_changes() {
    let mut handler: libc::sigaction = unsafe { std::mem::zeroed() };
    handler.sa_sigaction = note_window_change as *const () as usize;
    unsafe {
        libc::sigemptyset(&mut handler.sa_mask);
        libc::sigaction(libc::SIGWINCH, &handler, ptr::null_mut());
    }
}

/// Give the session's terminal the size of the one it is shown on. Nothing is
/// reported when it fails: a window that redraws at the wrong size is not a
/// reason to end a session the user is working in.
fn resize(terminal: RawFd, master: RawFd) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::ioctl(terminal, libc::TIOCGWINSZ, &mut size) == 0 {
            libc::ioctl(master, libc::TIOCSWINSZ, &size);
        }
    }
}
