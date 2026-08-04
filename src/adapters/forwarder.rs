//! The host-side forwarders of a sandbox's declared databases: the reason a
//! database is reachable from a namespace that has no route to it.
//!
//! Whatever a sandbox reaches on its own loopback is spliced to the host's
//! loopback on the same port, so a database that answers anywhere else needs
//! something on the host listening where that splice lands and dialling where
//! the database actually is. That listener is this. A database that already
//! answers on the host's loopback needs none: the splice reaches it directly,
//! and a listener of hort's own would be trying to take the address the real
//! service is already on.
//!
//! The address a sandbox reaches a declared database at is the same in both
//! egress postures, because what an allowlist decides is what the sandbox may
//! reach on its way out, not which databases the project declared.
//!
//! Like pasta and the proxy, the forwarders outlive the command that started
//! them and are stopped when the sandbox goes down, so what they are has to be
//! recognizable before they are signalled: a pid outlives the process it named.

use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::thread;

use crate::adapters::helper::{HostHelper, splice};
use crate::ports::DbForward;

/// The address a sandbox reaches every declared database at, which is also the
/// one address pasta's splice lands on, and therefore where a forwarder listens.
const LOOPBACK: &str = "127.0.0.1";
const FORWARDING: HostHelper = HostHelper::new("hort-forwarder", "forwarder.pid");

/// The host-side listeners of one sandbox's declared database forwards.
pub struct DbForwarder {
    bound: Vec<BoundForward>,
}

impl DbForwarder {
    /// Bind a listener for each of these forwards, on the host loopback port the
    /// sandbox reaches that database at.
    pub fn bind(forwards: &[DbForward]) -> Result<Self, String> {
        Ok(Self { bound: forwards.iter().map(BoundForward::bind).collect::<Result<_, _>>()? })
    }

    /// Carry connections between the bound ports and the declared databases
    /// until the process running it is stopped.
    pub fn serve(self) {
        thread::scope(|forwarding| {
            for bound in &self.bound {
                forwarding.spawn(move || bound.carry());
            }
        });
    }

    fn descriptors(&self) -> Vec<RawFd> {
        self.bound.iter().map(|bound| bound.listener.as_raw_fd()).collect()
    }
}

/// One declared database and the host loopback port the sandbox reaches it at.
struct BoundForward {
    listener: TcpListener,
    host: String,
    port: u16,
}

impl BoundForward {
    fn bind(forward: &DbForward) -> Result<Self, String> {
        let listener = TcpListener::bind((LOOPBACK, forward.port)).map_err(|err| {
            format!("binding {LOOPBACK}:{} for {}: {err}", forward.port, forward.host)
        })?;
        Ok(Self { listener, host: forward.host.clone(), port: forward.port })
    }

    /// Carry every connection this port receives to the database it was declared
    /// for. Sessions are carried alongside each other because a pool opens
    /// several at once, and one at a time would leave the second waiting out the
    /// first, which for an open session is forever.
    fn carry(&self) {
        thread::scope(|sessions| {
            loop {
                let Ok((client, _)) = self.listener.accept() else { continue };
                sessions.spawn(move || self.dial(client));
            }
        });
    }

    fn dial(&self, client: TcpStream) {
        let Ok(database) = TcpStream::connect((self.host.as_str(), self.port)) else { return };
        splice(&client, &database);
    }
}

/// Start this sandbox's database forwarding in a process that outlives this
/// call, and record what that process is.
pub fn start(sandbox_dir: &Path, forwards: &[DbForward]) -> Result<(), String> {
    let needed: Vec<DbForward> = one_database_per_port(forwards)?
        .into_iter()
        .filter(|forward| !reached_by_the_splice(forward))
        .map(|forward| DbForward { host: forward.host.clone(), port: forward.port })
        .collect();

    if needed.is_empty() {
        return Ok(());
    }

    let forwarder = DbForwarder::bind(&needed)?;
    let kept = forwarder.descriptors();
    FORWARDING.start(sandbox_dir, &kept, || forwarder.serve())
}

/// Stop this sandbox's forwarding, if the recorded process is still it. Stopping
/// a sandbox that never had any is not a failure.
pub fn stop(sandbox_dir: &Path) -> Result<(), String> {
    FORWARDING.stop(sandbox_dir)
}

/// The declared databases, refusing a port declared for more than one of them.
/// Inside the sandbox every declared database is one loopback port, so two
/// declarations sharing a port are one address that can only be one of them, and
/// nothing in the config says which.
fn one_database_per_port(forwards: &[DbForward]) -> Result<Vec<&DbForward>, String> {
    let mut declared: Vec<&DbForward> = Vec::new();
    for forward in forwards {
        match declared.iter().find(|other| other.port == forward.port) {
            Some(other) if other.host != forward.host => {
                return Err(format!(
                    "port {} is declared for both {} and {}, and a sandbox reaches one of them",
                    forward.port, other.host, forward.host
                ));
            }
            Some(_) => {}
            None => declared.push(forward),
        }
    }
    Ok(declared)
}

/// Whether the sandbox already reaches this database without a listener of
/// hort's own. What pasta splices is the host's loopback on the same port, and it
/// lands on one address: a database answering there is reached by the splice
/// itself, and a listener here would be taking the address the real service is on.
fn reached_by_the_splice(forward: &DbForward) -> bool {
    resolved(forward).contains(&IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Where a declared database answers, as the host resolves it now. Two spellings
/// of one address name one destination, which is why the declaration is resolved
/// rather than read.
fn resolved(forward: &DbForward) -> Vec<IpAddr> {
    (forward.host.as_str(), forward.port)
        .to_socket_addrs()
        .map(|addresses| addresses.map(|address| address.ip()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::{self, File};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, sleep};
    use std::time::{Duration, Instant};

    use crate::adapters::helper::a_declared_port;
    use crate::adapters::streams::sandbox_log_path;

    /// The address a sandbox reaches every declared database at, and the address
    /// the host-side listener therefore has to be on.
    const LOOPBACK: &str = "127.0.0.1";
    /// Where the databases these tests stand up answer. Any address the machine
    /// answers on that is not the one above serves, because what matters to a
    /// listener already bound is only that its destination is somewhere else.
    const DATABASE_ADDRESS: &str = "127.0.0.2";
    /// A destination reserved for documentation, so nothing ever answers there:
    /// enough for a forward that has to be started, and no traffic to anyone.
    const UNANSWERED_ADDRESS: &str = "192.0.2.1";
    const QUERY: &[u8] = b"select 1";
    const ANSWER: &[u8] = b"1 row";
    const FORWARDER_PID_FILE: &str = "forwarder.pid";
    /// How long an observation waits for what a working forwarder would already
    /// have delivered, and therefore how long absence is watched for before it
    /// counts as absence.
    const SETTLE: Duration = Duration::from_millis(500);
    /// How long a started process is given to finish wiring itself before what it
    /// looks like is held against it.
    const WIRING_DEADLINE: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(20);

    fn database_at(host: &str, port: u16) -> DbForward {
        DbForward { host: host.to_string(), port }
    }

    /// A database somewhere the host's own loopback does not answer for, which is
    /// the case that needs a forwarder at all.
    fn database_beyond_the_loopback(port: u16) -> DbForward {
        database_at(UNANSWERED_ADDRESS, port)
    }

    /// A database listening for what a forwarder will bring it.
    fn database_on(address: &str) -> TcpListener {
        TcpListener::bind((address, 0)).unwrap()
    }

    fn port_of(database: &TcpListener) -> u16 {
        database.local_addr().unwrap().port()
    }

    /// The connection a database was given, or nothing if none arrived. Nothing
    /// arriving and nothing being carried are different failures: this one says
    /// the declared destination was never reached at all.
    fn accepted_by(database: &TcpListener) -> Option<TcpStream> {
        database.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if let Ok((connection, _)) = database.accept() {
                connection.set_nonblocking(false).unwrap();
                connection.set_read_timeout(Some(SETTLE)).unwrap();
                return Some(connection);
            }
            sleep(POLL);
        }
        None
    }

    /// What arrived over a connection, bounded, so a connection that carries
    /// nothing fails a test instead of hanging it.
    fn received_over(connection: &mut TcpStream) -> Vec<u8> {
        let mut received = Vec::new();
        let _ = connection.read_to_end(&mut received);
        received
    }

    fn recorded_pid(sandbox_dir: &Path) -> u32 {
        let recorded = fs::read_to_string(sandbox_dir.join(FORWARDER_PID_FILE))
            .expect("a recorded forwarder process");
        recorded.trim().parse().expect("a pid")
    }

    /// Whether both of a process's output streams end at the sandbox log,
    /// waiting for it: a forked process takes a moment to put its streams in
    /// place.
    fn writes_to_within_deadline(pid: u32, log: &Path) -> bool {
        let deadline = Instant::now() + WIRING_DEADLINE;
        while Instant::now() < deadline {
            let output = fs::read_link(format!("/proc/{pid}/fd/1")).ok();
            let errors = fs::read_link(format!("/proc/{pid}/fd/2")).ok();
            if output.as_deref() == Some(log) && errors.as_deref() == Some(log) {
                return true;
            }
            sleep(POLL);
        }
        false
    }

    /// Whether a process holds no descriptor of this file, waiting for it: a
    /// forked process takes a moment to let go of what it inherited, and a
    /// descriptor once let go is never taken back.
    fn released_within_deadline(pid: u32, file: &Path) -> bool {
        let deadline = Instant::now() + WIRING_DEADLINE;
        while Instant::now() < deadline {
            if !holds(pid, file) {
                return true;
            }
            sleep(POLL);
        }
        false
    }

    fn holds(pid: u32, file: &Path) -> bool {
        let Ok(descriptors) = fs::read_dir(format!("/proc/{pid}/fd")) else { return false };
        descriptors
            .filter_map(|descriptor| fs::read_link(descriptor.ok()?.path()).ok())
            .any(|target| target == file)
    }

    /// Whether the port stops answering, waiting for it: a signalled process
    /// takes a moment to leave.
    fn stopped_answering_within_deadline(port: u16) -> bool {
        let deadline = Instant::now() + WIRING_DEADLINE;
        while Instant::now() < deadline {
            if TcpStream::connect((LOOPBACK, port)).is_err() {
                return true;
            }
            sleep(POLL);
        }
        false
    }

    #[test]
    fn a_declared_database_receives_what_the_sandbox_sends_it() {
        let database = database_on(DATABASE_ADDRESS);
        let port = port_of(&database);
        let forwarder = DbForwarder::bind(&[database_at(DATABASE_ADDRESS, port)]).unwrap();
        thread::spawn(move || forwarder.serve());

        let mut client = TcpStream::connect((LOOPBACK, port)).unwrap();
        client.write_all(QUERY).unwrap();

        // Inside the sandbox a declared database is one loopback port and nothing
        // else, because the namespace has no route to wherever it really is. The
        // whole of what makes that port a database is this: what arrives on it
        // reaches the declared destination, unchanged.
        let mut arrived = accepted_by(&database).expect("the declared database to be dialled");
        assert_eq!(received_over(&mut arrived), QUERY);
    }

    #[test]
    fn what_a_declared_database_answers_reaches_the_client() {
        let database = database_on(DATABASE_ADDRESS);
        let port = port_of(&database);
        thread::spawn(move || {
            let (mut answering, _) = database.accept().unwrap();
            answering.write_all(ANSWER).unwrap();
        });
        let forwarder = DbForwarder::bind(&[database_at(DATABASE_ADDRESS, port)]).unwrap();
        thread::spawn(move || forwarder.serve());

        let mut client = TcpStream::connect((LOOPBACK, port)).unwrap();
        client.set_read_timeout(Some(SETTLE)).unwrap();
        client.write_all(QUERY).unwrap();

        // A forward that carried only what the sandbox writes would deliver every
        // query and no result, which is a database that accepts work and answers
        // nothing.
        assert_eq!(received_over(&mut client), ANSWER);
    }

    #[test]
    fn a_second_declared_database_is_reachable_at_its_own_port() {
        let first = database_on(DATABASE_ADDRESS);
        let second = database_on(DATABASE_ADDRESS);
        let second_port = port_of(&second);
        let forwarder = DbForwarder::bind(&[
            database_at(DATABASE_ADDRESS, port_of(&first)),
            database_at(DATABASE_ADDRESS, second_port),
        ])
        .unwrap();
        thread::spawn(move || forwarder.serve());

        let mut client = TcpStream::connect((LOOPBACK, second_port)).unwrap();
        client.write_all(QUERY).unwrap();

        // A project declares every database it uses and one process carries all
        // of them, so anything that serves the first alone hands the sandbox a
        // second address that accepts connections and takes them nowhere.
        let mut arrived = accepted_by(&second).expect("the second database to be dialled");
        assert_eq!(received_over(&mut arrived), QUERY);
    }

    #[test]
    fn a_second_connection_to_a_database_is_served_while_the_first_is_open() {
        let database = database_on(DATABASE_ADDRESS);
        let port = port_of(&database);
        let forwarder = DbForwarder::bind(&[database_at(DATABASE_ADDRESS, port)]).unwrap();
        thread::spawn(move || forwarder.serve());
        let mut first = TcpStream::connect((LOOPBACK, port)).unwrap();
        first.write_all(QUERY).unwrap();
        let _first_session = accepted_by(&database).expect("the first database session");

        let mut second = TcpStream::connect((LOOPBACK, port)).unwrap();
        second.write_all(QUERY).unwrap();

        // A database session lives as long as whatever opened it, and a pool
        // opens several at once. Carried one at a time, the second client waits
        // for the first session to end, which for an open connection is never.
        assert!(accepted_by(&database).is_some());
    }

    #[test]
    fn a_database_on_the_host_loopback_gets_no_forwarder() {
        let sandbox = tempfile::tempdir().unwrap();
        let database = database_on(LOOPBACK);

        start(sandbox.path(), &[database_at(LOOPBACK, port_of(&database))]).unwrap();

        // The sandbox's loopback is spliced to the host's on the same port, which
        // is exactly where this database already answers. A listener of hort's
        // own has nothing to add and nowhere to be: the address it would need is
        // the one the real service is on, so binding it either fails or takes a
        // port away from the thing the forward exists to reach.
        assert!(!sandbox.path().join(FORWARDER_PID_FILE).exists());
    }

    #[test]
    fn a_sandbox_that_declares_no_database_gets_no_forwarder() {
        let sandbox = tempfile::tempdir().unwrap();

        start(sandbox.path(), &[]).unwrap();

        // Most projects declare none. A process serving nothing is one more thing
        // left running, to be recognized and stopped, for every sandbox that ever
        // existed.
        assert!(!sandbox.path().join(FORWARDER_PID_FILE).exists());
    }

    #[test]
    fn a_declared_port_already_taken_on_the_host_fails_the_start() {
        let sandbox = tempfile::tempdir().unwrap();
        let taken = database_on(LOOPBACK);

        let started = start(sandbox.path(), &[database_beyond_the_loopback(port_of(&taken))]);

        // Reported as started, this hands out a sandbox whose declared database
        // address answers for something else entirely, and the agent inside finds
        // out by talking to it. Which error it is stays open: the failure is the
        // behavior, the wording is not a product promise.
        assert!(started.is_err());
    }

    #[test]
    fn two_databases_declared_on_one_port_name_both_in_the_failure() {
        let sandbox = tempfile::tempdir().unwrap();
        let clashing = [database_at("192.0.2.1", 5432), database_at("192.0.2.2", 5432)];

        let failure = start(sandbox.path(), &clashing).unwrap_err();

        // Inside the sandbox both are the same address, so one of the two
        // declarations is a promise hort cannot keep, and nothing about the
        // config says which. Naming both is what makes it fixable without
        // guessing which database the sandbox was really talking to.
        assert!(failure.contains("192.0.2.1"));
        assert!(failure.contains("192.0.2.2"));
    }

    #[test]
    fn the_declared_port_answers_once_the_forwarding_is_started() {
        let sandbox = tempfile::tempdir().unwrap();
        let port = a_declared_port();

        start(sandbox.path(), &[database_beyond_the_loopback(port)]).unwrap();

        // A sandbox outlives the command that built it, so what makes its
        // databases reachable has to as well. Served from inside this call, the
        // address would answer for exactly as long as the call took to return,
        // and a sandbox whose forward was never started looks from the inside
        // like one whose database is down.
        assert!(TcpStream::connect((LOOPBACK, port)).is_ok());

        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn the_forwarding_writes_its_streams_to_the_sandbox_log() {
        let sandbox = tempfile::tempdir().unwrap();
        let port = a_declared_port();

        start(sandbox.path(), &[database_beyond_the_loopback(port)]).unwrap();

        // A process hort leaves running keeps the streams it was started with, so
        // one left holding the streams hort was invoked with holds them for the
        // life of the sandbox, and a piped invocation never reaches EOF. Here
        // that pipe belongs to the test runner, which is why leaving this unwired
        // hangs the suite rather than merely failing it.
        let log = sandbox_log_path(sandbox.path());
        assert!(writes_to_within_deadline(recorded_pid(sandbox.path()), &log));

        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn the_forwarding_holds_no_open_file_of_the_process_that_started_it() {
        let sandbox = tempfile::tempdir().unwrap();
        let carried = sandbox.path().join("held-by-the-caller");
        let held = File::create(&carried).unwrap();
        let port = a_declared_port();

        start(sandbox.path(), &[database_beyond_the_loopback(port)]).unwrap();

        // A fork keeps every open file of the process that forked it, and one of
        // those is the lock hort holds while it builds a sandbox. Held by a
        // process that only leaves when the sandbox does, it makes the name read
        // as still being built for as long as the sandbox lives.
        assert!(released_within_deadline(recorded_pid(sandbox.path()), &carried));

        drop(held);
        stop(sandbox.path()).unwrap();
    }

    #[test]
    fn stopping_the_forwarding_leaves_nothing_answering_for_the_sandbox() {
        let sandbox = tempfile::tempdir().unwrap();
        let port = a_declared_port();
        start(sandbox.path(), &[database_beyond_the_loopback(port)]).unwrap();

        stop(sandbox.path()).unwrap();

        // Nothing else ever stops it. A forwarder has no reason to leave when the
        // sandbox it served goes away, so one that is not signalled stays bound
        // to that port, and the next sandbox declaring the same database cannot
        // have it.
        assert!(stopped_answering_within_deadline(port));
    }

    #[test]
    fn stopping_leaves_a_recorded_process_that_is_not_the_forwarding_alone() {
        let sandbox = tempfile::tempdir().unwrap();
        // This test's own process: alive, recorded, and certainly not a forwarder.
        let recorded = std::process::id();
        fs::write(sandbox.path().join(FORWARDER_PID_FILE), format!("{recorded}\n")).unwrap();

        stop(sandbox.path()).unwrap();

        // A pid outlives the process it named, so a teardown that signals
        // whatever the file holds eventually signals something else entirely.
        assert!(Path::new(&format!("/proc/{recorded}")).exists());
    }
}
