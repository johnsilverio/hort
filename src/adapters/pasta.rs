//! `PastaNetworkProvider`: the host-side egress wiring of a sandbox, built on
//! pasta, the userspace bridge between the sandbox's network namespace and the
//! host. pasta provides connectivity and filters nothing; what closes an
//! allowlist sandbox is the shape of the namespace it runs in.
//!
//! Open posture asks pasta to configure the namespace and stops there, so the
//! sandbox reaches whatever the host reaches. An allowlist keeps pasta from
//! mapping the host's loopback, splices only the declared ports, and then empties
//! the namespace's route tables in both address families, which leaves
//! `127.0.0.1:<declared port>` as the only address the sandbox can reach.
//! Emptying one family alone is the trap to avoid: a surviving IPv6 default route
//! carries traffic straight out of a namespace that looks closed.
//!
//! pasta has to be told which user namespace owns the network namespace, and it
//! insists on a path rather than an inherited descriptor, so a transient holder
//! process publishes one. pasta outlives the holder, which is why the holder is
//! not one of the things tearing a sandbox down has to stop.
//!
//! The pid pasta records lives in the sandbox's state directory rather than in
//! its metadata record, so nothing writes that record outside the build lock.
//! Teardown reads the process's command line before signalling it, because a pid
//! is reusable.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, PipeWriter, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::adapters::namespaces::{enter, in_namespaces, owning_user_namespace};
use crate::adapters::streams::open_sandbox_log;
use crate::adapters::{forwarder, landlock, proxy};
use crate::domain::egress::EgressPolicy;
use crate::domain::error::HortError;
use crate::domain::model::SandboxName;
use crate::ports::{DbForward, NetworkProvider, NetworkSpec};

const PASTA: &str = "pasta";
const IP: &str = "ip";
const SANDBOXES_DIR: &str = "sandboxes";
const PID_FILE: &str = "pasta.pid";
/// pasta's own spelling for an empty port list. Leaving the flag out instead
/// forwards whatever pasta scans for by default.
const NO_PORTS: &str = "none";
/// The byte the holder announces itself with; only its arrival, never its value,
/// carries meaning, and the closed pipe that yields none is the failure signal.
const HANDSHAKE: [u8; 1] = [1];

/// The `NetworkProvider` hort wires real sandboxes with: pasta attached to the
/// sandbox's network namespace, plus the route shaping an allowlist needs.
pub struct PastaNetworkProvider {
    state_root: PathBuf,
}

impl PastaNetworkProvider {
    /// Build a provider keeping each sandbox's pasta pid file under `state_root`,
    /// which is also where teardown looks for it.
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn sandbox_dir(&self, name: &SandboxName) -> PathBuf {
        self.state_root.join(SANDBOXES_DIR).join(name.as_str())
    }

    fn pid_file(&self, name: &SandboxName) -> PathBuf {
        self.sandbox_dir(name).join(PID_FILE)
    }

    fn wire(&self, spec: &NetworkSpec) -> Result<(), String> {
        // Building a name that already has helpers running is a resume, and what
        // it left behind serves a namespace that is gone: pasta is told not to
        // leave when that happens, and the proxy and the forwarding answer on the
        // host, where nothing ever held them up. Starting new ones over them
        // takes their place in the only files that record them, and leaves them
        // running for as long as the machine is up.
        self.stop(&spec.name)?;

        let netns = File::open(&spec.netns).map_err(|err| {
            format!("opening the sandbox network namespace {}: {err}", spec.netns.display())
        })?;
        let owner = owning_user_namespace(netns.as_fd())?;

        let sandbox_dir = self.sandbox_dir(&spec.name);
        fs::create_dir_all(&sandbox_dir)
            .map_err(|err| format!("creating {}: {err}", sandbox_dir.display()))?;
        let pid_file = sandbox_dir.join(PID_FILE);

        // The proxy goes up first because the port it lands on is one of the
        // ports pasta is told to splice: started afterwards, it would be a way
        // out that nothing inside the sandbox can reach.
        let proxy_port = match spec.egress {
            EgressPolicy::Open => None,
            EgressPolicy::Allowlist(_) => Some(proxy::start(&sandbox_dir, &spec.egress)?),
        };
        // A declared database is reachable in both postures: what an allowlist
        // decides is what the sandbox may reach on its way out, not which
        // databases the project declared.
        forwarder::start(&sandbox_dir, &spec.db_forwards)?;

        // The same list the namespace is about to be shaped around, written down
        // where a session reads it, so what a session is held to and what the
        // sandbox can reach cannot drift apart.
        let reachable = reachable_ports(proxy_port, &spec.db_forwards);
        landlock::record_connect_ports(&sandbox_dir, reachable.as_deref())?;

        let holder = OwningUserNamespaceHolder::spawn(owner.as_fd())?;
        let started = start_pasta(
            &pasta_arguments(spec, &holder.namespace_path(), &pid_file, proxy_port),
            &sandbox_dir,
        );
        drop(holder);
        started?;

        if matches!(spec.egress, EgressPolicy::Allowlist(_)) {
            flush_routes(owner.as_fd(), netns.as_fd())?;
        }
        Ok(())
    }

    /// Stop every host-side process a sandbox leaves behind. Each is stopped
    /// whatever the others did: one that stayed behind because another failed is
    /// one nothing ever comes back for.
    fn stop(&self, name: &SandboxName) -> Result<(), String> {
        let sandbox_dir = self.sandbox_dir(name);
        let stopped_pasta = self.stop_pasta_of(name);
        let stopped_proxy = proxy::stop(&sandbox_dir);
        let stopped_forwarding = forwarder::stop(&sandbox_dir);
        stopped_pasta.and(stopped_proxy).and(stopped_forwarding)
    }

    fn stop_pasta_of(&self, name: &SandboxName) -> Result<(), String> {
        let pid_file = self.pid_file(name);
        let Ok(recorded) = fs::read_to_string(&pid_file) else {
            // Nothing recorded the sandbox's pasta, so there is nothing this can
            // be asked to stop: a sandbox torn down twice, or never wired.
            return Ok(());
        };

        let outcome = match recorded.trim().parse() {
            // A pid outlives the process it named, so the recorded one is only
            // acted on while it still names the pasta that was recorded.
            Ok(pid) if is_pasta(pid) => stop_pasta(pid),
            _ => Ok(()),
        };
        let _ = fs::remove_file(&pid_file);
        outcome
    }
}

impl NetworkProvider for PastaNetworkProvider {
    fn provision(&self, spec: &NetworkSpec) -> Result<(), HortError> {
        self.wire(spec).map_err(network_failure)
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        self.stop(name).map_err(network_failure)
    }
}

/// The arguments pasta is spawned with for this sandbox: the namespaces it
/// attaches to, the posture's mapping and forwarding flags (including the port
/// the sandbox's own proxy answers on, when it has one), and the file it records
/// its pid in.
fn pasta_arguments(
    spec: &NetworkSpec,
    userns: &Path,
    pid_file: &Path,
    proxy_port: Option<u16>,
) -> Vec<String> {
    let mut arguments = vec![
        "--userns".to_string(),
        argument(userns),
        "--netns".to_string(),
        argument(&spec.netns),
        "--config-net".to_string(),
        "--no-netns-quit".to_string(),
    ];

    if matches!(spec.egress, EgressPolicy::Allowlist(_)) {
        arguments.extend([
            "--map-host-loopback".to_string(),
            NO_PORTS.to_string(),
            "--map-guest-addr".to_string(),
            NO_PORTS.to_string(),
            "-T".to_string(),
            forwarded_ports(proxy_port, &spec.db_forwards),
        ]);
    }

    arguments.extend(["-P".to_string(), argument(pid_file)]);
    arguments
}

/// The whole of what an allowlisted sandbox can reach: its own proxy, and the
/// databases the project declared. `None` where there is no proxy to reach,
/// which is the posture that holds the sandbox to no list at all.
///
/// One list serves both the splice and the restriction a session runs under, so
/// the two cannot come to disagree about what this sandbox is wired for.
fn reachable_ports(proxy_port: Option<u16>, forwards: &[DbForward]) -> Option<Vec<u16>> {
    let proxy = proxy_port?;
    Some(std::iter::once(proxy).chain(forwards.iter().map(|forward| forward.port)).collect())
}

fn forwarded_ports(proxy_port: Option<u16>, forwards: &[DbForward]) -> String {
    let Some(ports) = reachable_ports(proxy_port, forwards) else {
        return NO_PORTS.to_string();
    };
    ports.iter().map(u16::to_string).collect::<Vec<_>>().join(",")
}

/// The `ip` invocations that empty a sandbox's route tables, one per address
/// family.
fn route_flush_arguments() -> Vec<Vec<&'static str>> {
    vec![
        vec!["-4", "route", "flush", "table", "main"],
        vec!["-6", "route", "flush", "table", "main"],
    ]
}

fn argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Start pasta and wait for it to finish setting the namespace up. pasta forks
/// into the background once it is wired, so the process hort spawns exiting is
/// the signal that the sandbox's networking is live.
///
/// It reports the whole topology it configured, and that report is the only
/// account hort ever gets of it, so the report goes to the sandbox's log rather
/// than to hort's own streams: the process that survives keeps whatever it was
/// started with, and a reader of a piped invocation would wait on it forever.
fn start_pasta(arguments: &[String], sandbox_dir: &Path) -> Result<(), String> {
    let status = Command::new(PASTA)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(open_sandbox_log(sandbox_dir)?)
        .stderr(open_sandbox_log(sandbox_dir)?)
        .status()
        .map_err(|err| format!("spawning {PASTA}: {err}"))?;
    if !status.success() {
        return Err(format!("{PASTA} {}: exited with {status}", arguments.join(" ")));
    }
    Ok(())
}

/// Take away every route pasta installed, in both address families. There is no
/// pasta flag for configuring an interface without routing it, and a namespace
/// stripped of its IPv4 routes alone still carries an IPv6 default route out.
fn flush_routes(owner: BorrowedFd<'_>, netns: BorrowedFd<'_>) -> Result<(), String> {
    in_namespaces(&[owner, netns], || {
        for arguments in route_flush_arguments() {
            let status = Command::new(IP)
                .args(&arguments)
                .status()
                .map_err(|err| format!("spawning {IP}: {err}"))?;
            if !status.success() {
                return Err(format!("{IP} {}: exited with {status}", arguments.join(" ")));
            }
        }
        Ok(())
    })
}

fn is_pasta(pid: libc::pid_t) -> bool {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    cmdline
        .split(|byte| *byte == 0)
        .next()
        .map(|program| Path::new(OsStr::from_bytes(program)))
        .and_then(Path::file_name)
        .is_some_and(|program| program == OsStr::new(PASTA))
}

fn stop_pasta(pid: libc::pid_t) -> Result<(), String> {
    if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
        let failure = io::Error::last_os_error();
        // The process leaving between being recognized and being signalled is
        // the outcome this asked for, not a failure to produce it.
        if failure.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("stopping {PASTA} ({pid}): {failure}"));
        }
    }
    Ok(())
}

/// A process parked in the user namespace that owns a sandbox's network
/// namespace, so that namespace has a path pasta can be pointed at: pasta insists
/// on a path and refuses an inherited descriptor. It is scoped to provisioning
/// because pasta outlives it, so it never becomes something teardown has to stop.
struct OwningUserNamespaceHolder {
    pid: libc::pid_t,
}

impl OwningUserNamespaceHolder {
    fn spawn(owner: BorrowedFd<'_>) -> Result<Self, String> {
        let (mut ready_reader, ready_writer) =
            io::pipe().map_err(|err| format!("creating a pipe: {err}"))?;

        match unsafe { libc::fork() } {
            -1 => Err(format!("forking the namespace holder: {}", io::Error::last_os_error())),
            0 => {
                drop(ready_reader);
                park_in(owner, ready_writer);
            }
            pid => {
                drop(ready_writer);
                let holder = Self { pid };
                // Reading the announcement is what keeps pasta from being pointed
                // at a path that does not name the owning namespace yet.
                let mut signal = [0u8; 1];
                match ready_reader.read(&mut signal) {
                    Ok(0) | Err(_) => {
                        Err("the holder never joined the owning user namespace".to_string())
                    }
                    Ok(_) => Ok(holder),
                }
            }
        }
    }

    fn namespace_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/user", self.pid))
    }
}

impl Drop for OwningUserNamespaceHolder {
    fn drop(&mut self) {
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        let mut status = 0;
        unsafe { libc::waitpid(self.pid, &mut status, 0) };
    }
}

/// Join the owning namespace, announce it, and stay there until killed. Leaving
/// without unwinding matters here as much as in any forked child: what the
/// unwinding would clean up belongs to the process on the other side of the fork.
fn park_in(owner: BorrowedFd<'_>, mut ready: PipeWriter) -> ! {
    if enter(&[owner]).is_err() || ready.write_all(&HANDSHAKE).is_err() {
        unsafe { libc::_exit(1) }
    }
    loop {
        unsafe { libc::pause() };
    }
}

fn network_failure(detail: impl Into<String>) -> HortError {
    HortError::NetworkProviderFailed { detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::net::TcpStream;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use crate::adapters::helper::a_declared_port;
    use crate::adapters::{forwarder, proxy};
    use crate::domain::egress::{EgressPolicy, HostPattern};
    use crate::domain::model::Domain;
    use crate::ports::DbForward;

    const USERNS: &str = "/proc/4242/ns/user";
    const PID_FILE: &str = "/state/sandboxes/demo/pasta.pid";
    /// A port the kernel could have handed the sandbox's proxy.
    const PROXY_PORT: u16 = 44001;
    /// A destination reserved for documentation, so nothing ever answers there:
    /// enough for a database that needs a forwarder, and no traffic to anyone.
    const UNANSWERED_ADDRESS: &str = "192.0.2.1";

    fn network_spec(egress: EgressPolicy, db_forwards: Vec<DbForward>) -> NetworkSpec {
        NetworkSpec {
            name: SandboxName::new("demo").unwrap(),
            netns: PathBuf::from("/proc/1234/ns/net"),
            egress,
            db_forwards,
        }
    }

    fn allowlist() -> EgressPolicy {
        EgressPolicy::Allowlist(vec![HostPattern::Exact(Domain::new("api.anthropic.com").unwrap())])
    }

    fn database_on(port: u16) -> DbForward {
        DbForward { host: "127.0.0.1".to_string(), port }
    }

    /// A database somewhere the host's own loopback does not answer for, which is
    /// the case a sandbox needs a forwarder for.
    fn remote_database_on(port: u16) -> DbForward {
        DbForward { host: UNANSWERED_ADDRESS.to_string(), port }
    }

    /// Whether the port stops answering, waiting for it: a signalled process takes
    /// a moment to leave.
    fn stopped_answering_within_deadline(port: u16) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn open_egress_asks_pasta_to_configure_the_namespace() {
        let spec = network_spec(EgressPolicy::Open, Vec::new());

        let arguments = pasta_arguments(&spec, Path::new(USERNS), Path::new(PID_FILE), None);

        // What `--no-netns-quit` gives up is pasta watching the directory of the
        // namespace file so it can leave when the file goes away. It cannot watch
        // one under /proc anyway, and without the flag it refuses to start.
        // No mapping or forwarding flag belongs here: open mode is unfiltered by
        // contract, and pasta's defaults already splice the sandbox's loopback to
        // the host's, which is where a declared database answers in this posture.
        assert_eq!(
            arguments,
            [
                "--userns",
                "/proc/4242/ns/user",
                "--netns",
                "/proc/1234/ns/net",
                "--config-net",
                "--no-netns-quit",
                "-P",
                "/state/sandboxes/demo/pasta.pid",
            ]
        );
    }

    #[test]
    fn allowlist_egress_unmaps_the_host_loopback() {
        let spec = network_spec(allowlist(), vec![database_on(5432)]);

        let arguments =
            pasta_arguments(&spec, Path::new(USERNS), Path::new(PID_FILE), Some(PROXY_PORT));

        // The default mapping exposes every one of the host's loopback services
        // to the sandbox, and declaring forwarded ports does not close it. Only
        // unmapping does, which is what leaves the declared ports as the whole of
        // what the sandbox can reach.
        assert_eq!(
            arguments,
            [
                "--userns",
                "/proc/4242/ns/user",
                "--netns",
                "/proc/1234/ns/net",
                "--config-net",
                "--no-netns-quit",
                "--map-host-loopback",
                "none",
                "--map-guest-addr",
                "none",
                "-T",
                "44001,5432",
                "-P",
                "/state/sandboxes/demo/pasta.pid",
            ]
        );
    }

    #[test]
    fn allowlist_egress_forwards_the_declared_database_ports() {
        let spec = network_spec(allowlist(), vec![database_on(5432), database_on(6379)]);

        let arguments =
            pasta_arguments(&spec, Path::new(USERNS), Path::new(PID_FILE), Some(PROXY_PORT));

        assert_eq!(
            arguments,
            [
                "--userns",
                "/proc/4242/ns/user",
                "--netns",
                "/proc/1234/ns/net",
                "--config-net",
                "--no-netns-quit",
                "--map-host-loopback",
                "none",
                "--map-guest-addr",
                "none",
                "-T",
                "44001,5432,6379",
                "-P",
                "/state/sandboxes/demo/pasta.pid",
            ]
        );
    }

    #[test]
    fn allowlist_egress_forwards_nothing_when_no_database_is_declared() {
        let spec = network_spec(allowlist(), Vec::new());

        let arguments =
            pasta_arguments(&spec, Path::new(USERNS), Path::new(PID_FILE), Some(PROXY_PORT));

        // With no database declared, the sandbox's own proxy is the whole of the
        // forward list, and that one port is the whole of what the sandbox can
        // reach.
        assert_eq!(
            arguments,
            [
                "--userns",
                "/proc/4242/ns/user",
                "--netns",
                "/proc/1234/ns/net",
                "--config-net",
                "--no-netns-quit",
                "--map-host-loopback",
                "none",
                "--map-guest-addr",
                "none",
                "-T",
                "44001",
                "-P",
                "/state/sandboxes/demo/pasta.pid",
            ]
        );
    }

    #[test]
    fn route_flush_covers_both_address_families() {
        let flushes = route_flush_arguments();

        // A namespace stripped of its IPv4 routes still carries an IPv6 default
        // route, and that one route is enough to carry HTTPS out of a sandbox
        // that looks closed.
        assert_eq!(
            flushes,
            vec![
                vec!["-4", "route", "flush", "table", "main"],
                vec!["-6", "route", "flush", "table", "main"],
            ]
        );
    }

    #[test]
    fn provision_fails_when_the_sandbox_namespace_is_gone() {
        let state_root = tempfile::tempdir().unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        let mut spec = network_spec(EgressPolicy::Open, Vec::new());
        spec.netns = PathBuf::from("/proc/nonexistent/ns/net");

        let result = provider.provision(&spec);

        // A provider that reported success here would leave `up` announcing a
        // sandbox whose network was never wired. Which error it is stays open:
        // the failure is the behavior, the wording is not a product promise.
        assert!(result.is_err());
    }

    #[test]
    fn teardown_is_idempotent_for_an_unknown_sandbox() {
        let state_root = tempfile::tempdir().unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        let result = provider.teardown(&SandboxName::new("ghost").unwrap());

        assert!(result.is_ok());
    }

    #[test]
    fn teardown_survives_a_pid_file_it_cannot_read_as_a_pid() {
        let state_root = tempfile::tempdir().unwrap();
        let sandbox_dir = state_root.path().join("sandboxes").join("demo");
        fs::create_dir_all(&sandbox_dir).unwrap();
        fs::write(sandbox_dir.join("pasta.pid"), "half a pid\n").unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        let result = provider.teardown(&SandboxName::new("demo").unwrap());

        // Tearing a sandbox down reconciles against whatever the disk holds, and a
        // truncated file is one of the states an interrupted run leaves behind.
        // Refusing to proceed here would make a corrupt byte unremovable.
        assert!(result.is_ok());
    }

    #[test]
    fn teardown_leaves_a_pid_that_is_no_longer_pasta_alone() {
        let state_root = tempfile::tempdir().unwrap();
        let sandbox_dir = state_root.path().join("sandboxes").join("demo");
        fs::create_dir_all(&sandbox_dir).unwrap();
        // The test process itself: a live pid that is certainly not pasta. A pid
        // is reusable, so a teardown that signals whatever the file names would
        // take this suite down with it.
        let recorded = std::process::id();
        fs::write(sandbox_dir.join("pasta.pid"), format!("{recorded}\n")).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        let result = provider.teardown(&SandboxName::new("demo").unwrap());

        assert!(result.is_ok());
        assert!(Path::new(&format!("/proc/{recorded}")).exists());
    }

    #[test]
    fn tearing_a_sandbox_down_stops_its_proxy() {
        let state_root = tempfile::tempdir().unwrap();
        let sandbox_dir = state_root.path().join("sandboxes").join("demo");
        fs::create_dir_all(&sandbox_dir).unwrap();
        let port = proxy::start(&sandbox_dir, &allowlist()).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider.teardown(&SandboxName::new("demo").unwrap()).unwrap();

        // The proxy is the second host-side process a sandbox leaves behind, and
        // like pasta it has no reason to leave on its own. A teardown that stops
        // only one of the two leaks the other for every sandbox that ever ran.
        assert!(stopped_answering_within_deadline(port));
    }

    #[test]
    fn tearing_a_sandbox_down_stops_its_database_forwarding() {
        let state_root = tempfile::tempdir().unwrap();
        let sandbox_dir = state_root.path().join("sandboxes").join("demo");
        fs::create_dir_all(&sandbox_dir).unwrap();
        let port = a_declared_port();
        forwarder::start(&sandbox_dir, &[remote_database_on(port)]).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider.teardown(&SandboxName::new("demo").unwrap()).unwrap();

        // The forwarding is the third host-side process a sandbox leaves behind.
        // Left running, it holds the port of a database the next sandbox declares
        // just as much as it holds the one it was started for.
        assert!(stopped_answering_within_deadline(port));
    }
}

#[cfg(all(test, feature = "privileged-tests"))]
mod privileged_tests {
    use super::*;

    use std::fs;
    use std::net::TcpStream;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use serial_test::serial;

    use crate::adapters::helper::a_declared_port;
    use crate::adapters::proxy;
    use crate::adapters::runtime::LibcontainerRuntime;
    use crate::adapters::streams::sandbox_log_path;
    use crate::domain::egress::{EgressPolicy, HostPattern};
    use crate::domain::model::Domain;
    use crate::ports::{ContainerRuntime, OciSpec};

    const PASTA_DEADLINE: Duration = Duration::from_secs(5);

    /// The prepared rootfs these tests boot, or `None` after reporting what is
    /// missing, so a host without one says why it skipped instead of failing.
    fn prepared_rootfs() -> Option<PathBuf> {
        let Ok(configured) = std::env::var("HORT_TEST_ROOTFS") else {
            eprintln!("skipped: set HORT_TEST_ROOTFS to a prepared rootfs directory to run this");
            return None;
        };
        let rootfs = PathBuf::from(configured);
        if !rootfs.is_dir() {
            eprintln!(
                "skipped: rootfs directory '{}' does not exist, prepare it first",
                rootfs.display()
            );
            return None;
        }
        Some(rootfs)
    }

    fn sandbox_spec(rootfs: PathBuf, state_root: &Path) -> OciSpec {
        let workdir = state_root.join("sandboxes/demo/worktree-demo");
        fs::create_dir_all(&workdir).unwrap();
        OciSpec {
            name: SandboxName::new("demo").unwrap(),
            rootfs,
            overlay: state_root.join("sandboxes/demo/overlay"),
            workdir,
            env: vec![("HORT_SANDBOX".to_string(), "demo".to_string())],
            resources: None,
        }
    }

    fn open_network(name: &SandboxName, anchor: u32) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Open,
            db_forwards: Vec::new(),
        }
    }

    fn allowlist_network(name: &SandboxName, anchor: u32) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Allowlist(Vec::new()),
            db_forwards: Vec::new(),
        }
    }

    /// A sandbox network in the allowlist posture that permits one host. The
    /// empty allowlist above is deliberately not reused: a list that permits
    /// nothing leaves whether a proxy exists at all open to the implementation.
    fn allowlist_network_permitting_one_host(name: &SandboxName, anchor: u32) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Allowlist(vec![HostPattern::Exact(
                Domain::new("api.anthropic.com").unwrap(),
            )]),
            db_forwards: Vec::new(),
        }
    }

    /// A sandbox network in the allowlist posture with one declared database on
    /// the host's own loopback, which the splice reaches without a forwarder of
    /// hort's own, so the port is spliced and free for the test to listen on.
    fn allowlist_network_with_a_database(
        name: &SandboxName,
        anchor: u32,
        port: u16,
    ) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Allowlist(vec![HostPattern::Exact(
                Domain::new("api.anthropic.com").unwrap(),
            )]),
            db_forwards: vec![DbForward { host: "127.0.0.1".to_string(), port }],
        }
    }

    /// A sandbox network in the open posture with one declared database, at an
    /// address the host's own loopback does not answer for.
    fn open_network_with_a_database(name: &SandboxName, anchor: u32, port: u16) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Open,
            db_forwards: vec![DbForward { host: "192.0.2.1".to_string(), port }],
        }
    }

    fn sandbox_dir(state_root: &Path) -> PathBuf {
        state_root.join("sandboxes/demo")
    }

    /// The sandbox's IPv4 routing table as the host reads it. An emptied table is
    /// the header line and nothing else.
    fn sandbox_ipv4_routes(anchor: u32) -> String {
        fs::read_to_string(format!("/proc/{anchor}/net/route")).expect("the sandbox route table")
    }

    /// The ports the sandbox recorded its sessions as being allowed to connect
    /// to, one per line, as a session reads them.
    fn recorded_connect_ports(sandbox_dir: &Path) -> Vec<u16> {
        let recorded = fs::read_to_string(sandbox_dir.join("connect.ports"))
            .expect("the ports a session may connect to");
        recorded.lines().map(|port| port.trim().parse().expect("a port")).collect()
    }

    /// The pid pasta recorded for this sandbox.
    fn recorded_pasta_pid(state_root: &Path) -> u32 {
        let recorded =
            fs::read_to_string(state_root.join("sandboxes/demo/pasta.pid")).expect("a pid file");
        recorded.trim().parse().expect("a pid")
    }

    /// Whether the port stops answering, waiting for it: a signalled process
    /// takes a moment to leave.
    fn stopped_answering_within_deadline(port: u16) -> bool {
        let deadline = Instant::now() + PASTA_DEADLINE;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    /// Whether the process is gone, waiting for it: a signalled process takes a
    /// moment to leave the process table.
    fn stopped_within_deadline(pid: u32) -> bool {
        let deadline = Instant::now() + PASTA_DEADLINE;
        while Instant::now() < deadline {
            if !Path::new(&format!("/proc/{pid}/ns/net")).exists() {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn provision_attaches_pasta_to_the_sandbox_namespace() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        let provisioned = provider.provision(&open_network(&spec.name, token.pid.0));

        assert!(provisioned.is_ok());
        let pasta = recorded_pasta_pid(state_root.path());
        let cmdline = fs::read(format!("/proc/{pasta}/cmdline")).unwrap();
        assert!(String::from_utf8_lossy(&cmdline).contains("pasta"));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn provisioning_keeps_what_pasta_reports_in_the_sandbox_log() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider.provision(&open_network(&spec.name, token.pid.0)).unwrap();

        // pasta reports every mapping and forwarding rule it installed, and that
        // report is the only account hort ever gets of the one host-side helper
        // whose failure is otherwise silent. It goes to the stream pasta was
        // started with, and the process that survives keeps that stream open.
        let sandbox_dir = state_root.path().join("sandboxes/demo");
        let logged = fs::read_to_string(sandbox_log_path(&sandbox_dir)).expect("the sandbox log");
        assert!(!logged.is_empty());
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn allowlist_provisioning_empties_the_sandbox_routing_table() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider.provision(&allowlist_network(&spec.name, token.pid.0)).unwrap();

        // Asking pasta for the right flags is not the same as the namespace ending
        // up closed: pasta configures the routes and something has to take them
        // away afterwards. A provider that skipped that step satisfies every other
        // test here while leaving the sandbox a default route to the internet.
        assert_eq!(sandbox_ipv4_routes(token.pid.0).lines().count(), 1);
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn teardown_stops_the_pasta_of_a_sandbox() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        provider.provision(&open_network(&spec.name, token.pid.0)).unwrap();
        let pasta = recorded_pasta_pid(state_root.path());

        provider.teardown(&spec.name).unwrap();

        // pasta never quits on its own once it is told not to watch the namespace
        // file, so a teardown that does not signal it leaves it running for good.
        assert!(stopped_within_deadline(pasta));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn allowlist_provisioning_starts_the_sandbox_proxy() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider
            .provision(&allowlist_network_permitting_one_host(&spec.name, token.pid.0))
            .unwrap();

        // Closing the namespace is only half of the posture. A sandbox whose
        // routes are gone and whose proxy was never started is not restricted,
        // it is disconnected, and the allowlist it was given permits nothing.
        let port = proxy::recorded_port(&sandbox_dir(state_root.path())).expect("a proxy port");
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn allowlist_provisioning_tells_pasta_to_splice_the_proxy_port() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider
            .provision(&allowlist_network_permitting_one_host(&spec.name, token.pid.0))
            .unwrap();

        // The sandbox reaches the proxy at all only because pasta was told to
        // splice that one port, which is also why the proxy has to be listening
        // before pasta is started: an unspliced proxy is a proxy nobody inside
        // can talk to.
        let port = proxy::recorded_port(&sandbox_dir(state_root.path())).expect("a proxy port");
        let pasta = recorded_pasta_pid(state_root.path());
        let cmdline = fs::read(format!("/proc/{pasta}/cmdline")).unwrap();
        assert!(String::from_utf8_lossy(&cmdline).contains(&port.to_string()));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn allowlist_provisioning_records_the_ports_a_session_may_connect_to() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        let declared = a_declared_port();

        provider
            .provision(&allowlist_network_with_a_database(&spec.name, token.pid.0, declared))
            .unwrap();

        // What a session is locked into has to be what this sandbox is actually
        // wired for, so it is written down here, by the run that wired it, and
        // not derived again from a configuration file that may have been edited
        // since. These two ports are the whole of what the sandbox can reach.
        let sandbox_dir = sandbox_dir(state_root.path());
        let proxy = proxy::recorded_port(&sandbox_dir).expect("a proxy port");
        let recorded = recorded_connect_ports(&sandbox_dir);
        assert!(recorded.contains(&proxy));
        assert!(recorded.contains(&declared));
        assert_eq!(recorded.len(), 2);
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn open_provisioning_starts_no_proxy() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());

        provider.provision(&open_network(&spec.name, token.pid.0)).unwrap();

        // Open egress is unfiltered by contract, so a proxy standing in an open
        // sandbox would be a chokepoint nobody asked for and a second thing to
        // stop for no gain.
        assert_eq!(proxy::recorded_port(&sandbox_dir(state_root.path())), None);
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn provisioning_a_resumed_sandbox_stops_the_pasta_it_left_behind() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let first = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        provider.provision(&open_network(&spec.name, first.pid.0)).unwrap();
        let left_behind = recorded_pasta_pid(state_root.path());
        runtime.teardown(&spec.name).unwrap();

        let resumed = runtime.start_anchor(&spec).unwrap();
        provider.provision(&open_network(&spec.name, resumed.pid.0)).unwrap();

        // pasta is told not to leave when the namespace it serves goes away,
        // which is what keeps a sandbox wired while nothing else holds it, and is
        // also why the one whose anchor died is still running when the same name
        // is built again. Provisioning that only starts things leaves that one
        // running for good and overwrites the only record of it.
        assert!(stopped_within_deadline(left_behind));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn allowlist_provisioning_of_a_resumed_sandbox_stops_the_proxy_it_left_behind() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let first = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        provider
            .provision(&allowlist_network_permitting_one_host(&spec.name, first.pid.0))
            .unwrap();
        let left_behind =
            proxy::recorded_port(&sandbox_dir(state_root.path())).expect("a proxy port");
        runtime.teardown(&spec.name).unwrap();

        let resumed = runtime.start_anchor(&spec).unwrap();
        provider
            .provision(&allowlist_network_permitting_one_host(&spec.name, resumed.pid.0))
            .unwrap();

        // The proxy answers on the host loopback, which no sandbox namespace
        // holds up, so it survives its sandbox exactly as pasta does. One left
        // behind holds a port for good and keeps enforcing a list the sandbox it
        // was started for no longer has.
        assert!(stopped_answering_within_deadline(left_behind));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn open_provisioning_forwards_the_declared_databases() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        let port = a_declared_port();

        provider.provision(&open_network_with_a_database(&spec.name, token.pid.0, port)).unwrap();

        // What an allowlist decides is what a sandbox may reach on its way out,
        // not which databases the project declared, so a declared database is
        // reachable in both postures and at the same address in both. Started
        // alongside the proxy, the forwarding would exist only in the posture
        // that has one, and an open sandbox would reach nothing it declared.
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }
}
