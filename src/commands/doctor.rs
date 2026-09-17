//! `doctor`: what this host can do, read and handed back as plain data. It asks
//! the environment probe the one detection every command shares, validates the
//! rootfs the configuration names, and returns a report. Turning that report
//! into text belongs to the pure renderer, and deciding what to leave with
//! belongs to the pure gate; both live outside this file so a report and a
//! status can never come to disagree about the same host.
//!
//! Nothing here writes, asks, or builds anything. `run` cannot fail, and that is
//! the contract rather than an accident of the current body: `doctor` reports,
//! so an error would take the report away on the very host a caller most needs
//! it for.

use std::path::Path;

use crate::domain::config::ResolvedConfig;
use crate::domain::error::HortError;
use crate::domain::model::Capabilities;
use crate::domain::preconditions::rootfs_precondition_error;
use crate::ports::EnvironmentProbe;

/// What `doctor` observed: the host capabilities the probe answered with, and
/// what the configured rootfs would raise, `None` when it can carry a sandbox.
///
/// The rootfs half is an error rather than a verdict of its own because the
/// three answers a caller needs are already errors hort emits elsewhere: none
/// configured, one the host does not have, and one it cannot build from. Their
/// canonical strings already name the consequence and the way out, so a second
/// vocabulary here would be a second copy of the same advice.
pub struct DoctorReport {
    pub capabilities: Capabilities,
    pub configuration: ConfigurationReport,
}

/// What came of reading the configuration, and everything that read produced.
///
/// One value and not a pair, because the rootfs verdict only exists on the arm
/// where there was a configuration to take it from. A file hort cannot parse is
/// reported rather than raised: refusing would leave no report at all on exactly
/// the host somebody is running `doctor` on to find out what is broken.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigurationReport {
    /// hort could not read the configuration, and this is what the parser said.
    /// Nothing was asked about a rootfs, so there is no verdict to report and
    /// none can be built here.
    Unreadable(HortError),
    /// The configuration read. `rootfs` is what the host says about the one it
    /// names, `None` when there is nothing wrong with it.
    Read { rootfs: Option<HortError> },
}

/// Coordinates `doctor` over the one port it reads. It holds the resolved
/// configuration because the rootfs is the only part of the report that is not a
/// host fact, and validating it needs the path the configuration names.
pub struct DoctorCommand<'a> {
    env: &'a dyn EnvironmentProbe,
    config: Result<&'a ResolvedConfig, &'a HortError>,
}

impl<'a> DoctorCommand<'a> {
    pub fn new(env: &'a dyn EnvironmentProbe, config: &'a ResolvedConfig) -> Self {
        Self { env, config: Ok(config) }
    }

    /// Report on a host whose configuration hort could not parse. Two
    /// constructors rather than one taking a `Result`, so a caller has to have
    /// attempted the read and neither state can be built by accident.
    pub fn on_unreadable_configuration(
        env: &'a dyn EnvironmentProbe,
        error: &'a HortError,
    ) -> Self {
        Self { env, config: Err(error) }
    }
}

impl DoctorCommand<'_> {
    /// Read the host and report it. No `Result`: the probe never fails, and the
    /// rootfs verdict is a value.
    pub fn run(&self) -> DoctorReport {
        DoctorReport {
            capabilities: self.env.detect(),
            configuration: match self.config {
                Ok(config) => ConfigurationReport::Read { rootfs: self.rootfs_verdict(config) },
                Err(unreadable) => ConfigurationReport::Unreadable(unreadable.clone()),
            },
        }
    }

    /// What the host says about the rootfs this configuration names, `None` when
    /// it can carry a sandbox.
    ///
    /// The declared session shell goes into the question, because a rootfs that
    /// does not carry it is one a build refuses, and a report that called such a
    /// rootfs fine would send the user off looking somewhere else entirely.
    fn rootfs_verdict(&self, config: &ResolvedConfig) -> Option<HortError> {
        let facts = config.rootfs.as_deref().map(|configured| {
            self.env.inspect_rootfs(Path::new(configured), config.shell.as_deref())
        });
        rootfs_precondition_error(facts.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::domain::config::{Cache, Mounts, ResolvedConfig};
    use crate::domain::model::CgroupCaps;
    use crate::fakes::FakeCapabilities;

    fn ready_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
            git: true,
        }
    }

    fn configuration_naming(rootfs: &str) -> ResolvedConfig {
        ResolvedConfig {
            rootfs: Some(rootfs.to_string()),
            agents: Vec::new(),
            mounts: Mounts::default(),
            network: Vec::new(),
            egress: None,
            notifications: None,
            cache: Cache::default(),
            shell: None,
            resources: None,
            git: None,
        }
    }

    #[test]
    fn doctor_reports_what_the_host_says_about_the_configured_rootfs() {
        let configured = "/opt/hort/rootfs";
        let config = configuration_naming(configured);
        let host_that_has_it = FakeCapabilities::new(ready_host());
        let host_that_does_not = FakeCapabilities::new(ready_host()).with_missing_rootfs();

        let where_it_is = DoctorCommand::new(&host_that_has_it, &config).run();
        let where_it_is_not = DoctorCommand::new(&host_that_does_not, &config).run();

        // Both arms, because either one alone reads the same on a report that
        // asked the host and on one that answers the same thing every time.
        assert_eq!(where_it_is.configuration, ConfigurationReport::Read { rootfs: None });
        assert_eq!(
            where_it_is_not.configuration,
            ConfigurationReport::Read {
                rootfs: Some(HortError::RootfsMissing { path: configured.to_string() })
            },
            "the verdict comes from the host and names the path the configuration gave it"
        );
    }
}
