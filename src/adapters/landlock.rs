//! What a session may reach, held by the kernel: the Landlock ruleset every
//! session runs under, and the record of the ports an allowlisted sandbox's
//! sessions may connect to.
//!
//! A ruleset restricts the thread that applies it and everything that thread
//! goes on to exec, and nothing can loosen it afterwards, which is what makes it
//! worth having in a sandbox whose user is root of its own user namespace. It
//! also means where it is applied is the whole of whether it is right: the paths
//! a rule names are resolved when the rule is made, so a ruleset built before
//! the session enters the sandbox would name hort's own root and deny the
//! session every file it has, in a way that looks like anything but Landlock.
//! The last step before the exec is therefore the only place it can go.
//!
//! The ports come from the run that wired the sandbox, not from the
//! configuration read again later: they are the same list the sandbox's
//! networking was built with, and a configuration edited in between would
//! restrict a session to ports the sandbox cannot reach, or admit ports it can.
//! A sandbox that records none is one that was never closed, and is left alone.

use std::fs;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetError,
};

const CONNECT_PORTS_FILE: &str = "connect.ports";
/// The rights the filesystem half is written against, the newest this crate
/// knows. A kernel that carries fewer keeps the ones it has.
const FILESYSTEM_ABI: ABI = ABI::V9;
/// The whole of what a session can see, which after the pivot is the sandbox's
/// own merged root and nothing of the host.
const SANDBOX_ROOT: &str = "/";

/// Write down the ports the sessions of this sandbox may connect to, or take the
/// record away when the sandbox restricts none. The wiring run owns this file
/// either way: a stale list left behind by an earlier posture would restrict
/// sessions to ports nothing answers on.
pub fn record_connect_ports(sandbox_dir: &Path, ports: Option<&[u16]>) -> Result<(), String> {
    let record = connect_ports_file(sandbox_dir);
    let Some(ports) = ports else {
        let _ = fs::remove_file(&record);
        return Ok(());
    };

    let recorded: Vec<String> = ports.iter().map(u16::to_string).collect();
    fs::write(&record, recorded.join("\n") + "\n")
        .map_err(|err| format!("writing {}: {err}", record.display()))
}

/// The ports this sandbox recorded its sessions as being allowed to connect to,
/// or `None` when it recorded none.
pub fn recorded_connect_ports(sandbox_dir: &Path) -> Result<Option<Vec<u16>>, String> {
    let record = connect_ports_file(sandbox_dir);
    let Ok(recorded) = fs::read_to_string(&record) else {
        return Ok(None);
    };

    recorded
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim().parse().map_err(|err| {
                // Reading a damaged record as "no ports" would turn the one file
                // that closes a sandbox into the file that opens it.
                format!("reading {} as the ports a session may reach: {err}", record.display())
            })
        })
        .collect::<Result<Vec<u16>, String>>()
        .map(Some)
}

/// Restrict this thread, and everything it execs, to the sandbox's own root and
/// to `connect_ports` if the sandbox named any. A sandbox that named none is
/// unfiltered by contract, and reading that silence as an empty list would leave
/// its sessions unable to reach anything at all.
///
/// What the kernel cannot enforce it drops, so this stays quiet about a host too
/// old to hold the network half; the build says that once, for the sandbox.
pub fn restrict_session(connect_ports: Option<&[u16]>) -> Result<(), String> {
    let root = PathFd::new(SANDBOX_ROOT)
        .map_err(|err| format!("opening {SANDBOX_ROOT} to restrict the session to it: {err}"))?;
    enforce(root, connect_ports)
        .map_err(|err| format!("restricting what the session may reach: {err}"))
}

fn enforce(root: PathFd, connect_ports: Option<&[u16]>) -> Result<(), RulesetError> {
    // Stated rather than inherited: everything about this ruleset degrading
    // instead of failing on an older kernel rests on it.
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(FILESYSTEM_ABI))?;
    if connect_ports.is_some() {
        // The right is named rather than taken from the set of the ABI the host
        // reports, which below the one that introduced it is an empty set, and an
        // empty set is the one thing the crate refuses outright instead of
        // degrading. Named, an old kernel simply drops it.
        // Only connecting is restricted: binding a port is no way out of a
        // namespace that has no route, and denying it would take with it every
        // loopback server the work inside the sandbox runs.
        ruleset = ruleset.handle_access(AccessNet::ConnectTcp)?;
    }

    let mut created =
        ruleset.create()?.add_rule(PathBeneath::new(root, AccessFs::from_all(FILESYSTEM_ABI)))?;
    for port in connect_ports.unwrap_or_default() {
        created = created.add_rule(NetPort::new(*port, AccessNet::ConnectTcp))?;
    }
    created.restrict_self()?;
    Ok(())
}

fn connect_ports_file(sandbox_dir: &Path) -> PathBuf {
    sandbox_dir.join(CONNECT_PORTS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sandbox_that_records_no_ports_leaves_no_record_behind() {
        let sandbox_dir = tempfile::tempdir().unwrap();
        record_connect_ports(sandbox_dir.path(), Some(&[44001])).unwrap();

        record_connect_ports(sandbox_dir.path(), None).unwrap();

        // Wiring a sandbox is what says which ports its sessions may reach, and
        // it says it every time: a list left over from what this name used to be
        // would lock the sessions of an open sandbox onto dead ports.
        assert_eq!(recorded_connect_ports(sandbox_dir.path()).unwrap(), None);
    }

    #[test]
    fn recorded_ports_are_read_back_as_they_were_written() {
        let sandbox_dir = tempfile::tempdir().unwrap();

        record_connect_ports(sandbox_dir.path(), Some(&[44001, 5432])).unwrap();

        assert_eq!(recorded_connect_ports(sandbox_dir.path()).unwrap(), Some(vec![44001, 5432]));
    }

    #[test]
    fn a_record_that_cannot_be_read_as_ports_is_a_failure_rather_than_an_empty_list() {
        let sandbox_dir = tempfile::tempdir().unwrap();
        fs::write(sandbox_dir.path().join("connect.ports"), "half a port\n").unwrap();

        let result = recorded_connect_ports(sandbox_dir.path());

        // The one file that closes a sandbox is the one file whose damage must
        // not read as permission: a session that cannot be restricted is a
        // session that does not start.
        assert!(result.is_err());
    }
}
