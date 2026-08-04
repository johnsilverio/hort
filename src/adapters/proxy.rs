//! The SNI/CONNECT proxy: the one place an allowlisted sandbox sends outbound
//! traffic through, and the place that refuses everything the allowlist does not
//! name.
//!
//! It speaks HTTP CONNECT, which is how the tools inside the sandbox reach it
//! through their proxy variables, and it never terminates TLS. It reads the
//! hostname off the CONNECT request, reads the server name off the ClientHello
//! the client sends next, and from there copies bytes in both directions
//! untouched. Reading both names is what makes the allowlist mean anything: a
//! request may name a host the list permits and then ask the connection itself
//! for a different one, which is how a permitted host gets used to reach a
//! forbidden one.
//!
//! The proxy listens on the host loopback on a port the kernel picks, and pasta
//! splices that one port into the sandbox, so a sandbox reaches its own proxy and
//! no other. The port is recorded in the sandbox's state directory, which is
//! where a session filling its proxy variables finds it.
//!
//! Like pasta, the proxy outlives the command that started it and is stopped when
//! the sandbox goes down, so what it is has to be recognizable before it is
//! signalled: a pid outlives the process it named.

use std::fs::{self, File};
use std::io::{self, PipeWriter, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use tls_parser::{
    Err as TlsError, SNIType, TlsExtension, TlsMessage, TlsMessageHandshake,
    parse_tls_client_hello_extensions, parse_tls_plaintext,
};

use crate::adapters::streams::open_sandbox_log;
use crate::domain::egress::EgressPolicy;

const PID_FILE: &str = "proxy.pid";
const PORT_FILE: &str = "proxy.port";
const LOOPBACK: &str = "127.0.0.1";
const DEV_NULL: &str = "/dev/null";
/// What the proxy calls itself in the process table, and the whole of what tells
/// a teardown that the pid it recorded still names the process it recorded.
const PROXY_NAME: &str = "hort-proxy";
/// The longest name the kernel keeps for a process, counting the terminator.
const PROCESS_NAME_LIMIT: usize = 16;
const CONNECT: &str = "CONNECT";
const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const REFUSAL: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\r\n";
const HEAD_END: &[u8] = b"\r\n\r\n";
const REQUEST_LIMIT: usize = 8 * 1024;
const HELLO_LIMIT: usize = 64 * 1024;
/// How long a connection has to say which host it is for. What it buys is the
/// client that writes its hello in pieces; what it costs is a thread parked on a
/// connection that will never say anything.
const NAMING_DEADLINE: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 4096;
/// The byte the started proxy announces itself with; only its arrival, never its
/// value, carries meaning, and the closed pipe that yields none is the failure.
const HANDSHAKE: [u8; 1] = [1];

/// A sandbox's proxy, bound to the loopback port it will answer on.
pub struct SniProxy<'a> {
    policy: &'a EgressPolicy,
    listener: TcpListener,
    port: u16,
    log: File,
}

impl<'a> SniProxy<'a> {
    /// Bind a proxy enforcing `policy` on a host loopback port the kernel picks,
    /// writing every decision it makes to `log`.
    pub fn bind(policy: &'a EgressPolicy, log: File) -> Result<Self, String> {
        let listener = TcpListener::bind((LOOPBACK, 0))
            .map_err(|err| format!("binding the sandbox proxy to {LOOPBACK}: {err}"))?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("reading the address the sandbox proxy bound to: {err}"))?
            .port();
        Ok(Self { policy, listener, port, log })
    }

    /// The host loopback port the sandbox reaches this proxy on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serve connections until the process running it is stopped.
    ///
    /// A tunnel lives for as long as the tool inside the sandbox is using it, so
    /// connections are served alongside each other: one at a time would mean one
    /// agent at a time, each waiting out the last one's downloads.
    pub fn serve(self) {
        thread::scope(|connections| {
            loop {
                let Ok((client, _)) = self.listener.accept() else { continue };
                connections.spawn(|| serve_one(client, self.policy, &self.log));
            }
        });
    }
}

/// Start this sandbox's proxy in a process that outlives this call, record where
/// it listens and what it is, and answer with the port pasta has to splice.
pub fn start(sandbox_dir: &Path, policy: &EgressPolicy) -> Result<u16, String> {
    let streams = proxy_streams(sandbox_dir)?;
    let proxy = SniProxy::bind(policy, open_sandbox_log(sandbox_dir)?)?;
    let port = proxy.port();
    let (mut announcement, announce) =
        io::pipe().map_err(|err| format!("creating a pipe: {err}"))?;

    match unsafe { libc::fork() } {
        -1 => Err(format!("forking the sandbox proxy: {}", io::Error::last_os_error())),
        0 => {
            drop(announcement);
            serve_detached(proxy, streams, announce);
        }
        child => {
            drop(announce);
            // Whatever this side keeps of the listener keeps the port bound, so a
            // proxy stopped later would leave something still answering for it.
            drop(proxy);
            drop(streams);
            let mut signal = [0u8; 1];
            match announcement.read(&mut signal) {
                Ok(0) | Err(_) => {
                    discard(child);
                    Err("the sandbox proxy never started".to_string())
                }
                Ok(_) => match record_proxy(sandbox_dir, child, port) {
                    Ok(()) => Ok(port),
                    Err(failure) => {
                        // Nothing else knows this process exists, so a proxy left
                        // running here is one nothing can ever stop.
                        discard(child);
                        Err(failure)
                    }
                },
            }
        }
    }
}

/// Stop this sandbox's proxy, if the recorded process is still it. Stopping a
/// sandbox that never had one is not a failure.
pub fn stop(sandbox_dir: &Path) -> Result<(), String> {
    let pid_file = sandbox_dir.join(PID_FILE);
    let recorded = fs::read_to_string(&pid_file).ok().and_then(|pid| pid.trim().parse().ok());
    let outcome = match recorded {
        // A pid outlives the process it named, so the recorded one is only acted
        // on while it still names the proxy that was recorded.
        Some(pid) if is_proxy(pid) => stop_proxy(pid),
        _ => Ok(()),
    };
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(sandbox_dir.join(PORT_FILE));
    outcome
}

/// The loopback port this sandbox's proxy answers on, as recorded when it
/// started, or `None` when the sandbox has no proxy.
pub fn recorded_port(sandbox_dir: &Path) -> Option<u16> {
    fs::read_to_string(sandbox_dir.join(PORT_FILE)).ok()?.trim().parse().ok()
}

/// Serve this sandbox's proxy from the forked child, after making it into
/// something the host can recognize and something that holds nothing of the
/// command that forked it. Leaving without unwinding matters as much as in any
/// forked child: what the unwinding would clean up belongs to the process on the
/// other side of the fork.
fn serve_detached(proxy: SniProxy<'_>, streams: ProxyStreams, announce: PipeWriter) -> ! {
    let mut serving_with = [proxy.listener.as_raw_fd(), proxy.log.as_raw_fd()];
    if name_process().is_err() || redirect(streams).is_err() || announced(announce).is_err() {
        unsafe { libc::_exit(1) }
    }
    close_inherited_descriptors(&mut serving_with);
    proxy.serve();
    unsafe { libc::_exit(0) }
}

/// Answer one connection: read what it asks for, hold both the request and the
/// connection itself against the allowlist, and only then reach the host.
fn serve_one(mut client: TcpStream, policy: &EgressPolicy, log: &File) {
    let request = match read_connect_request(&mut client) {
        Ok(request) => request,
        Err(refusal) => {
            record(log, &format!("refused {refusal}"));
            let _ = client.write_all(REFUSAL);
            return;
        }
    };

    if !policy.matches(&request.host) {
        // Judged before anything is dialled: opening the connection first would
        // already have told a forbidden host that this sandbox is here.
        record(log, &format!("refused {} (not in the allowlist)", request.host));
        let _ = client.write_all(REFUSAL);
        return;
    }
    if client.write_all(ESTABLISHED).is_err() {
        return;
    }

    let host = request.host;
    let Some((hello, named)) = read_client_hello(&mut client, request.buffered) else {
        record(log, &format!("refused {host} (the connection named no host)"));
        return;
    };
    if named != host {
        record(log, &format!("refused {host} (the connection asked for {named})"));
        return;
    }

    record(log, &format!("allowed {host}"));
    let Ok(mut upstream) = TcpStream::connect((host.as_str(), request.port)) else { return };
    if upstream.write_all(&hello).is_err() {
        return;
    }
    // The deadline the naming ran under would otherwise cut an idle tunnel.
    let _ = client.set_read_timeout(None);
    splice(&client, &upstream);
}

/// What a CONNECT request asks for, and whatever the client wrote after it,
/// which already belongs to the tunnel.
struct ConnectRequest {
    host: String,
    port: u16,
    buffered: Vec<u8>,
}

/// Read the request that opens a tunnel. The error names what is being refused,
/// which is what the log line is made of.
fn read_connect_request(client: &mut TcpStream) -> Result<ConnectRequest, String> {
    let deadline = Instant::now() + NAMING_DEADLINE;
    let mut received = Vec::new();
    let head = loop {
        if let Some(end) = head_end(&received) {
            break end;
        }
        if received.len() >= REQUEST_LIMIT {
            return Err("a request that never ended".to_string());
        }
        read_more(client, &mut received, deadline).ok_or("a request that never arrived")?;
    };

    let request = String::from_utf8_lossy(&received[..head]);
    let mut fields = request.lines().next().unwrap_or_default().split_whitespace();
    let (Some(method), Some(target)) = (fields.next(), fields.next()) else {
        return Err("a request in no shape to read".to_string());
    };
    if method != CONNECT {
        // Plain HTTP is what a proxy variable produces for an `http://` address.
        // Refusing it is honest rather than restrictive: the allowlist is decided
        // on a name the connection itself confirms, and cleartext confirms none.
        return Err(format!("{target} (only CONNECT tunnels are proxied)"));
    }
    let Some((host, port)) = target.rsplit_once(':') else {
        return Err(format!("{target} (the request named no port)"));
    };
    let Ok(port) = port.parse() else {
        return Err(format!("{target} (the request named no port)"));
    };

    Ok(ConnectRequest {
        host: normalized(host),
        port,
        buffered: received[head + HEAD_END.len()..].to_vec(),
    })
}

fn head_end(received: &[u8]) -> Option<usize> {
    received.windows(HEAD_END.len()).position(|window| window == HEAD_END)
}

/// What a connection's opening bytes say about the host they are for.
enum Naming {
    Named(String),
    Nameless,
    Unfinished,
}

/// Read the client's opening bytes until they name a host, and answer with them
/// and the name. A hello names its host well past its first bytes, so reading
/// once and letting through whatever did not parse turns the check into
/// something a client skips by writing twice.
fn read_client_hello(client: &mut TcpStream, buffered: Vec<u8>) -> Option<(Vec<u8>, String)> {
    let deadline = Instant::now() + NAMING_DEADLINE;
    let mut hello = buffered;
    loop {
        match server_name(&hello) {
            Naming::Named(host) => return Some((hello, host)),
            Naming::Nameless => return None,
            Naming::Unfinished => {}
        }
        if hello.len() >= HELLO_LIMIT {
            return None;
        }
        read_more(client, &mut hello, deadline)?;
    }
}

/// Read the server name off the record without touching the handshake itself.
/// Bytes that are no TLS record at all name nothing and never will; bytes that
/// are a record still being written name nothing yet.
fn server_name(opening: &[u8]) -> Naming {
    match parse_tls_plaintext(opening) {
        Ok((_, record)) => match hello_server_name(&record.msg) {
            Some(host) => Naming::Named(host),
            None => Naming::Nameless,
        },
        Err(TlsError::Incomplete(_)) => Naming::Unfinished,
        Err(_) => Naming::Nameless,
    }
}

fn hello_server_name(messages: &[TlsMessage<'_>]) -> Option<String> {
    for message in messages {
        let TlsMessage::Handshake(TlsMessageHandshake::ClientHello(hello)) = message else {
            continue;
        };
        let Some(extensions) = hello.ext else { continue };
        let Ok((_, extensions)) = parse_tls_client_hello_extensions(extensions) else { continue };
        for extension in extensions {
            let TlsExtension::SNI(names) = extension else { continue };
            for (kind, name) in names {
                if kind == SNIType::HostName {
                    return Some(normalized(&String::from_utf8_lossy(name)));
                }
            }
        }
    }
    None
}

/// Copy both directions until either side is done with the other. A tunnel that
/// carried one direction would deliver a request and never its reply.
fn splice(client: &TcpStream, upstream: &TcpStream) {
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

/// Read whatever the client has to say next, within what is left of the time it
/// was given to name a host. `None` is a client that said nothing more.
fn read_more(client: &mut TcpStream, into: &mut Vec<u8>, deadline: Instant) -> Option<()> {
    let left = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())?;
    client.set_read_timeout(Some(left)).ok()?;
    let mut chunk = [0u8; READ_CHUNK];
    match client.read(&mut chunk) {
        Ok(0) | Err(_) => None,
        Ok(read) => {
            into.extend_from_slice(&chunk[..read]);
            Some(())
        }
    }
}

/// One host as everything here compares it: what an allowlist, a request line
/// and a hello have to agree on regardless of how each of them spelled it.
fn normalized(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// Write one decision to the sandbox's log. Formatted whole and written once,
/// because the connections writing here run alongside each other and an append
/// is only atomic per write.
fn record(log: &File, decision: &str) {
    let mut log = log;
    let _ = log.write_all(format!("{decision}\n").as_bytes());
}

/// The streams the proxy is given: the sandbox's log for both outputs, and
/// nothing to read.
struct ProxyStreams {
    input: File,
    output: File,
    errors: File,
}

/// Open them from hort's own process, before the fork. A proxy keeps whatever it
/// is started with for as long as the sandbox lives, so one started with hort's
/// streams holds a redirected or piped invocation open forever, and holds the
/// writing end of whatever feeds hort open just as long.
fn proxy_streams(sandbox_dir: &Path) -> Result<ProxyStreams, String> {
    Ok(ProxyStreams {
        input: File::open(DEV_NULL).map_err(|err| format!("opening {DEV_NULL}: {err}"))?,
        output: open_sandbox_log(sandbox_dir)?,
        errors: open_sandbox_log(sandbox_dir)?,
    })
}

/// Put the streams in place of the ones the fork was inherited with. Taking them
/// by value is what closes the originals here, before anything else is closed:
/// a descriptor closed later could already have been handed to a connection.
fn redirect(streams: ProxyStreams) -> Result<(), ()> {
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

/// Say in the process table what this process is, so a teardown can tell that
/// the pid it holds is still the proxy and not whatever inherited the number.
fn name_process() -> Result<(), ()> {
    let mut name = [0u8; PROCESS_NAME_LIMIT];
    name[..PROXY_NAME.len()].copy_from_slice(PROXY_NAME.as_bytes());
    if unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) } == -1 {
        return Err(());
    }
    Ok(())
}

fn announced(mut announce: PipeWriter) -> io::Result<()> {
    announce.write_all(&HANDSHAKE)
}

/// Close everything this process inherited except the descriptors it was left
/// to serve with. A forked child keeps every open file of the process that
/// forked it, and one of those is the lock hort holds while it builds a sandbox:
/// held by a process that never exits, it would make the name look like it is
/// still being built for as long as the sandbox lives.
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

fn record_proxy(sandbox_dir: &Path, pid: libc::pid_t, port: u16) -> Result<(), String> {
    write_line(&sandbox_dir.join(PID_FILE), &pid.to_string())?;
    write_line(&sandbox_dir.join(PORT_FILE), &port.to_string())
}

fn write_line(path: &Path, value: &str) -> Result<(), String> {
    fs::write(path, format!("{value}\n"))
        .map_err(|err| format!("writing {}: {err}", path.display()))
}

fn is_proxy(pid: libc::pid_t) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|name| name.trim() == PROXY_NAME)
}

fn stop_proxy(pid: libc::pid_t) -> Result<(), String> {
    if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
        let failure = io::Error::last_os_error();
        // The process leaving between being recognized and being signalled is
        // the outcome this asked for, not a failure to produce it.
        if failure.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("stopping the sandbox proxy ({pid}): {failure}"));
        }
    }
    Ok(())
}

/// Take back a proxy that was started but could not be handed over.
fn discard(pid: libc::pid_t) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::thread::{self, sleep};
    use std::time::{Duration, Instant};

    use crate::adapters::streams::{open_sandbox_log, sandbox_log_path};
    use crate::domain::egress::HostPattern;
    use crate::domain::model::Domain;

    /// A real ClientHello asking for `localhost`, captured from `openssl s_client
    /// -servername localhost` against a local listener that never answered. Real
    /// bytes rather than assembled ones, because what the proxy has to read is
    /// whatever a client actually puts on the wire.
    const CLIENT_HELLO_FOR_LOCALHOST: &[u8] = include_bytes!("testdata/client-hello-localhost.bin");
    /// The same capture with `-servername evil.example.com`: a hello asking for
    /// a host other than the one a CONNECT request will name.
    const CLIENT_HELLO_FOR_ANOTHER_HOST: &[u8] =
        include_bytes!("testdata/client-hello-another-host.bin");
    const UPSTREAM_ANSWER: &[u8] = b"the upstream answered";
    /// How long an observation waits for bytes a working proxy would already have
    /// delivered, and therefore also how long the absence of bytes is watched for
    /// before it counts as absence.
    const SETTLE: Duration = Duration::from_millis(500);
    const STOP_DEADLINE: Duration = Duration::from_secs(5);

    fn allowlist_of(host: &str) -> EgressPolicy {
        EgressPolicy::Allowlist(vec![HostPattern::Exact(Domain::new(host).unwrap())])
    }

    /// The same allowlist, leaked: the proxy serves from a thread that outlives
    /// the frame that built it, the way the real one serves from a process that
    /// outlives the call that started it.
    fn allowlisting(host: &str) -> &'static EgressPolicy {
        Box::leak(Box::new(allowlist_of(host)))
    }

    /// A proxy serving this policy in the background, and the loopback port it
    /// answers on.
    fn serving(policy: &'static EgressPolicy, sandbox_dir: &Path) -> u16 {
        let proxy = SniProxy::bind(policy, open_sandbox_log(sandbox_dir).unwrap()).unwrap();
        let port = proxy.port();
        thread::spawn(move || proxy.serve());
        port
    }

    /// A listener standing in for the host a sandbox asked to reach.
    fn upstream() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").unwrap()
    }

    /// A client of the proxy, with the reader it takes answers from. Every read
    /// is bounded, so a proxy that never answers fails a test instead of hanging
    /// it.
    fn client_of(proxy: u16) -> (TcpStream, BufReader<TcpStream>) {
        let stream = TcpStream::connect(("127.0.0.1", proxy)).unwrap();
        stream.set_read_timeout(Some(SETTLE)).unwrap();
        let answers = BufReader::new(stream.try_clone().unwrap());
        (stream, answers)
    }

    /// What reached the upstream. A connection that never arrives and one that
    /// carries no byte are the same answer here: what the client sent did not get
    /// through.
    fn bytes_that_reached(upstream: &TcpListener) -> Vec<u8> {
        upstream.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if let Ok((mut connection, _)) = upstream.accept() {
                connection.set_nonblocking(false).unwrap();
                connection.set_read_timeout(Some(SETTLE)).unwrap();
                let mut received = Vec::new();
                let _ = connection.read_to_end(&mut received);
                return received;
            }
            sleep(Duration::from_millis(20));
        }
        Vec::new()
    }

    /// Whether the upstream was left alone entirely: nothing ever connected to it.
    fn nothing_connected_to(upstream: &TcpListener) -> bool {
        upstream.set_nonblocking(true).unwrap();
        upstream.accept().is_err()
    }

    /// Waits for the proxy to answer, so a descriptor read afterwards is read
    /// from a process that has finished wiring itself rather than from one still
    /// being born.
    fn answering_within_deadline(port: u16) {
        let deadline = Instant::now() + STOP_DEADLINE;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            sleep(Duration::from_millis(20));
        }
    }

    /// Whether the port stops answering, waiting for it: a signalled process takes
    /// a moment to leave.
    fn stopped_answering_within_deadline(port: u16) -> bool {
        let deadline = Instant::now() + STOP_DEADLINE;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn an_allowlisted_host_receives_the_client_hello_untouched() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        answers.read_line(&mut String::new()).unwrap();
        client.write_all(CLIENT_HELLO_FOR_LOCALHOST).unwrap();

        // A proxy that terminated TLS to read the name would hand the upstream a
        // handshake it originated itself. What has to arrive is the client's own
        // first bytes, to the byte, because everything after them is the client's
        // TLS running end to end through a connection the proxy only copies.
        assert_eq!(bytes_that_reached(&upstream), CLIENT_HELLO_FOR_LOCALHOST);
    }

    #[test]
    fn a_client_hello_split_across_writes_is_still_checked_against_the_target() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        answers.read_line(&mut String::new()).unwrap();
        let (opening, rest) = CLIENT_HELLO_FOR_ANOTHER_HOST.split_at(24);
        client.write_all(opening).unwrap();
        sleep(Duration::from_millis(50));
        client.write_all(rest).unwrap();

        // A hello names its host well past its first bytes, so a client is free to
        // write those bytes on their own and the name a moment later. A proxy that
        // reads once and lets through whatever it could not parse turns the check
        // into something any client skips by writing twice.
        let _ = answers.read_to_end(&mut Vec::new());
        assert!(bytes_that_reached(&upstream).is_empty());
    }

    #[test]
    fn a_connection_that_never_names_a_host_is_refused() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        answers.read_line(&mut String::new()).unwrap();
        client.write_all(b"SSH-2.0-OpenSSH_9.6\r\n").unwrap();

        // Nothing in this connection says which host it is for, so nothing in it
        // can be held against the host the request named. Letting it through makes
        // the check optional: whatever is not a hello passes unread, and a
        // permitted host ends up carrying whatever protocol was asked of it.
        let _ = answers.read_to_end(&mut Vec::new());
        assert!(bytes_that_reached(&upstream).is_empty());
    }

    #[test]
    fn a_second_connection_is_served_while_the_first_is_open() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let request =
            format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n");
        let (mut first, mut first_answers) = client_of(proxy);
        first.write_all(request.as_bytes()).unwrap();
        first_answers.read_line(&mut String::new()).unwrap();
        first.write_all(CLIENT_HELLO_FOR_LOCALHOST).unwrap();

        let (mut second, mut second_answers) = client_of(proxy);
        second.write_all(request.as_bytes()).unwrap();

        // A tunnel stays open for as long as the tool inside the sandbox is using
        // it, so a proxy that serves one connection at a time serves one tool at a
        // time, and the second agent waits for the first one's download to end.
        let mut answer = String::new();
        second_answers.read_line(&mut answer).expect("the second client to be answered");
        assert!(!answer.is_empty());
    }

    #[test]
    fn what_an_allowlisted_host_answers_reaches_the_client() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut connection, _) = upstream.accept().unwrap();
            connection.write_all(UPSTREAM_ANSWER).unwrap();
        });
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        answers.read_line(&mut String::new()).unwrap();
        client.write_all(CLIENT_HELLO_FOR_LOCALHOST).unwrap();

        // A tunnel that only carries one direction delivers a request and never
        // its reply. What the proxy sent before the tunnel opened is still in
        // front of the answer, so the answer is what the client ends up with.
        let mut received = Vec::new();
        let _ = answers.read_to_end(&mut received);
        assert!(received.ends_with(UPSTREAM_ANSWER));
    }

    #[test]
    fn a_host_outside_the_allowlist_is_never_connected_to() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("api.anthropic.com"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let _ = answers.read_to_end(&mut Vec::new());

        // The request is judged before anything is dialled. A proxy that opened
        // the connection first and judged afterwards would already have told a
        // forbidden host that the sandbox is here, which is the whole of what an
        // allowlist exists to prevent.
        assert!(nothing_connected_to(&upstream));
    }

    #[test]
    fn a_client_hello_that_renames_the_target_never_reaches_upstream() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        answers.read_line(&mut String::new()).unwrap();
        client.write_all(CLIENT_HELLO_FOR_ANOTHER_HOST).unwrap();

        // The request named a host the allowlist permits and the connection then
        // asked for a different one. A proxy that trusted only what the request
        // said would forward this, and a host on the list would have carried
        // traffic for a host that is not.
        let _ = answers.read_to_end(&mut Vec::new());
        assert!(bytes_that_reached(&upstream).is_empty());
    }

    #[test]
    fn a_refused_host_is_named_in_the_sandbox_log() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("api.anthropic.com"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let _ = answers.read_to_end(&mut Vec::new());

        // Inside the sandbox a refusal looks like a tool that could not reach
        // something, and this log is where that becomes a diagnosis instead of a
        // mystery. It has to say what happened and to whom; the wording around
        // those two is free.
        let logged = fs::read_to_string(sandbox_log_path(sandbox.path())).unwrap();
        assert!(logged.contains("refused"));
        assert!(logged.contains("localhost"));
    }

    #[test]
    fn an_allowed_host_is_named_in_the_sandbox_log() {
        let sandbox = tempfile::tempdir().unwrap();
        let upstream = upstream();
        let target = upstream.local_addr().unwrap().port();
        let proxy = serving(allowlisting("localhost"), sandbox.path());
        let (mut client, mut answers) = client_of(proxy);

        client
            .write_all(
                format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: localhost:{target}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        answers.read_line(&mut String::new()).unwrap();
        client.write_all(CLIENT_HELLO_FOR_LOCALHOST).unwrap();
        let _ = bytes_that_reached(&upstream);

        // The other half of the diagnosis: a line here means the tool did reach
        // the proxy and the allowlist let it through, and no line at all means the
        // tool never used the proxy in the first place.
        let logged = fs::read_to_string(sandbox_log_path(sandbox.path())).unwrap();
        assert!(logged.contains("allowed"));
        assert!(logged.contains("localhost"));
    }

    #[test]
    fn the_sandbox_proxy_answers_on_the_port_it_records() {
        let sandbox = tempfile::tempdir().unwrap();

        start(sandbox.path(), &allowlist_of("localhost")).unwrap();

        // This file is where a session reads the address it points its proxy
        // variables at, so a port that is missing, or not the one being answered
        // on, sends every tool in the sandbox at nothing.
        let recorded = recorded_port(sandbox.path()).expect("a recorded proxy port");
        assert!(TcpStream::connect(("127.0.0.1", recorded)).is_ok());

        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn the_proxy_outlives_the_call_that_started_it() {
        let sandbox = tempfile::tempdir().unwrap();

        start(sandbox.path(), &allowlist_of("localhost")).unwrap();

        // A sandbox outlives the command that built it, so the only way out of it
        // has to as well. A proxy served from inside this call would be gone
        // before the first agent ran.
        let recorded = fs::read_to_string(sandbox.path().join("proxy.pid")).unwrap();
        assert_ne!(recorded.trim().parse::<u32>().unwrap(), std::process::id());

        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn the_proxy_writes_its_streams_to_the_sandbox_log() {
        let sandbox = tempfile::tempdir().unwrap();

        let port = start(sandbox.path(), &allowlist_of("localhost")).unwrap();
        answering_within_deadline(port);

        // A process hort leaves running keeps the streams it was started with, so
        // a proxy left holding the ones hort was invoked with holds them for the
        // life of the sandbox and a piped invocation never reaches EOF. Here that
        // pipe belongs to the test runner, which is why leaving this unwired hangs
        // the suite instead of merely failing it.
        let recorded = fs::read_to_string(sandbox.path().join("proxy.pid")).unwrap();
        let log = sandbox_log_path(sandbox.path());
        assert_eq!(fs::read_link(format!("/proc/{}/fd/1", recorded.trim())).unwrap(), log);
        assert_eq!(fs::read_link(format!("/proc/{}/fd/2", recorded.trim())).unwrap(), log);

        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn the_proxy_holds_no_open_file_of_the_process_that_started_it() {
        let sandbox = tempfile::tempdir().unwrap();
        let carried = sandbox.path().join("held-by-the-caller");
        let held = File::create(&carried).unwrap();

        let port = start(sandbox.path(), &allowlist_of("localhost")).unwrap();
        answering_within_deadline(port);

        // A fork keeps every open file of the process that forked it, and one of
        // those is the lock held while the sandbox is being built. Kept by a
        // process that only leaves when the sandbox does, it makes the name read
        // as still being built for as long as the sandbox lives.
        let recorded = fs::read_to_string(sandbox.path().join("proxy.pid")).unwrap();
        let descriptors = fs::read_dir(format!("/proc/{}/fd", recorded.trim())).unwrap();
        let carried_along = descriptors
            .filter_map(|descriptor| fs::read_link(descriptor.ok()?.path()).ok())
            .any(|target| target == carried);
        assert!(!carried_along);

        drop(held);
        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn stopping_the_proxy_leaves_nothing_answering_for_the_sandbox() {
        let sandbox = tempfile::tempdir().unwrap();
        let port = start(sandbox.path(), &allowlist_of("localhost")).unwrap();

        stop(sandbox.path()).unwrap();

        // Nothing else ever stops it. The proxy has no reason to leave when the
        // sandbox it served goes away, so a teardown that does not signal it
        // leaves one running for every sandbox that ever existed.
        assert!(stopped_answering_within_deadline(port));
    }

    #[test]
    fn stopping_leaves_a_recorded_process_that_is_not_the_proxy_alone() {
        let sandbox = tempfile::tempdir().unwrap();
        // This test's own process: alive, recorded, and certainly not a proxy.
        let recorded = std::process::id();
        fs::write(sandbox.path().join("proxy.pid"), format!("{recorded}\n")).unwrap();

        stop(sandbox.path()).unwrap();

        // A pid outlives the process it named, so a teardown that signals whatever
        // the file holds eventually signals something else entirely.
        assert!(Path::new(&format!("/proc/{recorded}")).exists());
    }
}
