//! The egress policy decision: whether a sandbox's outbound host is allowed, and
//! what the posture entitles the sandbox to.
//!
//! [`EgressPolicy`] is a pure value. It answers "does this host pass?" and so
//! decides whether the allowlist proxy is needed at all; it never resolves a
//! name, spawns a proxy, or opens a socket. `Open` permits everything (no proxy
//! is spawned); `Allowlist` permits only hosts matching one of its
//! [`HostPattern`]s and gets no name resolution of its own.

use std::path::PathBuf;

use super::config::Egress;
use super::error::HortError;
use super::model::{Domain, Warning};
use crate::ports::SandboxFile;

/// The outbound-egress decision for a sandbox.
///
/// `Open` is the unfiltered default. `Allowlist` permits a host only when it
/// matches one of the held patterns; everything else is refused.
#[derive(Debug)]
pub enum EgressPolicy {
    Open,
    Allowlist(Vec<HostPattern>),
}

/// One allowlist entry. A bare config entry parses to `Exact` (the host itself);
/// a `*.`-prefixed entry parses to `Suffix` (any host under that domain, never
/// the apex).
#[derive(Debug)]
pub enum HostPattern {
    Exact(Domain),
    Suffix(Domain),
}

impl EgressPolicy {
    /// Resolve the parsed `egress` config value into a policy. Absent or `true`
    /// is `Open`; `false` is deny-all (an empty allowlist); an allowlist becomes
    /// a validated set of [`HostPattern`]s, where each entry's hostname is checked
    /// through [`Domain`] and a `*.` prefix marks a `Suffix` pattern.
    pub fn from_config(egress: Option<&Egress>) -> Result<Self, HortError> {
        match egress {
            None | Some(Egress::Open(true)) => Ok(EgressPolicy::Open),
            Some(Egress::Open(false)) => Ok(EgressPolicy::Allowlist(Vec::new())),
            Some(Egress::Allowlist { allow }) => {
                let patterns = allow
                    .iter()
                    .map(|entry| HostPattern::parse(entry))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(EgressPolicy::Allowlist(patterns))
            }
        }
    }

    /// Decide whether `host` is permitted by this policy. `Open` permits any
    /// host; `Allowlist` permits `host` iff it matches at least one pattern.
    /// Matching is case-insensitive and ignores a trailing dot on `host`.
    ///
    /// ```
    /// use hort::domain::egress::{EgressPolicy, HostPattern};
    /// use hort::domain::model::Domain;
    ///
    /// let policy = EgressPolicy::Allowlist(vec![HostPattern::Suffix(
    ///     Domain::new("githubusercontent.com").unwrap(),
    /// )]);
    ///
    /// assert!(policy.matches("raw.githubusercontent.com"));
    /// assert!(!policy.matches("githubusercontent.com")); // a suffix never admits the apex
    /// ```
    pub fn matches(&self, host: &str) -> bool {
        match self {
            EgressPolicy::Open => true,
            EgressPolicy::Allowlist(patterns) => {
                let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
                patterns.iter().any(|pattern| pattern.permits(&host))
            }
        }
    }
}

/// The Landlock ABI that carries the right to restrict which ports a process may
/// connect to. Anything below it takes the rules and applies none of them.
const CONNECT_RESTRICTION_ABI: u8 = 4;

/// The address, seen from inside the sandbox, that name lookups are carried
/// from to the host's own name server.
///
/// Two things about the value are load-bearing, and both look like details worth
/// tidying away. It is reserved for documentation, so nothing anywhere answers
/// there on its own: a box whose network was never told to answer for it asks
/// into a void rather than at a stranger's resolver, and putting a real public
/// one here would take the sandbox's name resolution out of hort's hands and
/// break every host whose own resolver answers differently. And it is not the
/// address a libc falls back to when it finds no resolver file, which is
/// loopback: choosing that would leave the file hort writes doing nothing while
/// a lookup still succeeded, so the wiring could rot with everything looking
/// fine.
const SANDBOX_RESOLVER: &str = "198.51.100.53";

/// Where a sandbox looks for the address of its name server. The path belongs to
/// the libc inside the box, not to hort.
const RESOLVER_FILE: &str = "/etc/resolv.conf";

/// The address a sandbox under `egress` reaches a name server at, and `None` for
/// one that is to have no name resolution of its own.
///
/// An allowlist gets nothing, and that is a layer of the allowlist rather than an
/// omission: the proxy is handed a hostname and resolves it on the host, so a way
/// out of the box that carries a name and comes back with an address is a way out
/// the allowlist never sees.
pub fn sandbox_resolver(egress: &EgressPolicy) -> Option<String> {
    matches!(egress, EgressPolicy::Open).then(|| SANDBOX_RESOLVER.to_string())
}

/// The file that points a sandbox's name lookups at `address`.
///
/// It goes in whatever the prepared rootfs already carries: the write lands in
/// the sandbox's own disposable layer, so a resolver baked in by the author of an
/// image is shadowed by the live one and nothing of the base is touched.
pub fn resolver_drop_in(address: &str) -> SandboxFile {
    SandboxFile { path: PathBuf::from(RESOLVER_FILE), content: format!("nameserver {address}\n") }
}

/// The advisory a build owes the user when this host cannot enforce the
/// connect-port restriction an allowlisted sandbox is supposed to run under.
///
/// The kernel silently drops what it cannot do, so a sandbox built here is one
/// layer thinner than the one that was asked for and nothing would say so.
/// `landlock_abi` is what the host reports; an open sandbox restricts no port in
/// the first place and has nothing to lose.
pub fn egress_degradation_warning(
    egress: &EgressPolicy,
    landlock_abi: Option<u8>,
) -> Option<Warning> {
    let enforceable = matches!(landlock_abi, Some(abi) if abi >= CONNECT_RESTRICTION_ABI);
    match egress {
        EgressPolicy::Open => None,
        EgressPolicy::Allowlist(_) if enforceable => None,
        EgressPolicy::Allowlist(_) => Some(Warning::new(
            "this kernel cannot restrict which ports a process connects to, so the egress allowlist of this sandbox runs without its kernel layer (Linux 6.7 or newer enforces it)",
        )),
    }
}

impl HostPattern {
    /// Parse one allowlist entry: a `*.` prefix yields a `Suffix` over the
    /// remaining hostname, anything else an `Exact`. The hostname is validated
    /// through [`Domain`], so a malformed entry propagates its error.
    fn parse(entry: &str) -> Result<Self, HortError> {
        match entry.strip_prefix("*.") {
            Some(suffix) => Ok(HostPattern::Suffix(Domain::new(suffix)?)),
            None => Ok(HostPattern::Exact(Domain::new(entry)?)),
        }
    }

    /// Whether `host`, already lowercased with any trailing dot removed,
    /// satisfies this pattern. `Exact` admits only the host itself; `Suffix`
    /// admits any host with at least one extra left label, anchored at the dot,
    /// never the apex.
    fn permits(&self, host: &str) -> bool {
        match self {
            HostPattern::Exact(domain) => host == domain.as_str().to_ascii_lowercase(),
            HostPattern::Suffix(domain) => {
                let dotted = format!(".{}", domain.as_str().to_ascii_lowercase());
                host.strip_suffix(&dotted).is_some_and(|label| !label.is_empty())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_open_permits_anything() {
        let policy = EgressPolicy::Open;

        assert!(policy.matches("anything.example.test"));
    }

    #[test]
    fn egress_allowlist_permits_exact_host() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        assert!(policy.matches("api.anthropic.com"));
    }

    #[test]
    fn egress_allowlist_refuses_unlisted_host() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        assert!(!policy.matches("evil.com"));
    }

    #[test]
    fn egress_wildcard_matches_subdomain() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Suffix(
            Domain::new("githubusercontent.com").unwrap(),
        )]);

        assert!(policy.matches("raw.githubusercontent.com"));
    }

    #[test]
    fn egress_wildcard_does_not_match_apex() {
        let policy =
            EgressPolicy::Allowlist(vec![HostPattern::Suffix(Domain::new("example.com").unwrap())]);

        assert!(!policy.matches("example.com"));
    }

    #[test]
    fn egress_wildcard_does_not_match_host_without_dot_boundary() {
        let policy =
            EgressPolicy::Allowlist(vec![HostPattern::Suffix(Domain::new("example.com").unwrap())]);

        assert!(!policy.matches("notexample.com"));
    }

    #[test]
    fn egress_wildcard_does_not_match_host_with_empty_left_label() {
        let policy =
            EgressPolicy::Allowlist(vec![HostPattern::Suffix(Domain::new("example.com").unwrap())]);

        assert!(!policy.matches(".example.com"));
    }

    #[test]
    fn egress_match_is_case_insensitive() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        assert!(policy.matches("API.Anthropic.COM"));
    }

    #[test]
    fn egress_match_ignores_trailing_dot() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        assert!(policy.matches("api.anthropic.com."));
    }

    #[test]
    fn egress_config_true_resolves_to_open() {
        let policy = EgressPolicy::from_config(Some(&Egress::Open(true))).unwrap();

        assert!(matches!(policy, EgressPolicy::Open));
    }

    #[test]
    fn egress_config_absent_resolves_to_open() {
        let policy = EgressPolicy::from_config(None).unwrap();

        assert!(matches!(policy, EgressPolicy::Open));
    }

    #[test]
    fn egress_config_false_denies_all_egress() {
        let policy = EgressPolicy::from_config(Some(&Egress::Open(false))).unwrap();

        assert!(!policy.matches("api.anthropic.com"));
    }

    #[test]
    fn egress_config_allow_entries_resolve_to_allowlist() {
        let egress = Egress::Allowlist {
            allow: vec!["api.anthropic.com".to_string(), "*.githubusercontent.com".to_string()],
        };

        let policy = EgressPolicy::from_config(Some(&egress)).unwrap();

        assert!(policy.matches("api.anthropic.com"));
        assert!(policy.matches("raw.githubusercontent.com"));
    }

    #[test]
    fn an_allowlist_warns_when_the_kernel_is_too_old_to_restrict_connections() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        // Landlock has existed since long before it could restrict a port, and
        // asking a kernel of that age for the network rules costs nothing and
        // does nothing: it drops them and reports success.
        let warning = egress_degradation_warning(&policy, Some(3));

        assert!(warning.is_some());
    }

    #[test]
    fn an_allowlist_warns_when_the_kernel_has_no_landlock_at_all() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        let warning = egress_degradation_warning(&policy, None);

        assert!(warning.is_some());
    }

    #[test]
    fn an_allowlist_is_silent_on_a_kernel_that_restricts_connections() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Exact(
            Domain::new("api.anthropic.com").unwrap(),
        )]);

        let warning = egress_degradation_warning(&policy, Some(4));

        assert!(warning.is_none());
    }

    #[test]
    fn open_egress_is_silent_on_a_kernel_that_cannot_restrict_connections() {
        // Open egress is unfiltered by contract, so there is no restriction for
        // this kernel to be missing. Warning here would put a security advisory
        // on every sandbox of every user whose kernel is old, about a layer none
        // of those sandboxes ever asked for.
        let warning = egress_degradation_warning(&EgressPolicy::Open, None);

        assert!(warning.is_none());
    }

    #[test]
    fn egress_config_rejects_invalid_hostname_entry() {
        let egress = Egress::Allowlist { allow: vec!["https://api.anthropic.com/v1".to_string()] };

        let result = EgressPolicy::from_config(Some(&egress));

        assert!(matches!(result, Err(HortError::InvalidName)));
    }
}
