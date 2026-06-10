//! The egress policy decision: whether a sandbox's outbound host is allowed.
//!
//! [`EgressPolicy`] is a pure value. It answers "does this host pass?" and so
//! decides whether the allowlist proxy is needed at all; it never resolves a
//! name, spawns a proxy, or opens a socket. `Open` permits everything (no proxy
//! is spawned); `Allowlist` permits only hosts matching one of its
//! [`HostPattern`]s.

use super::config::Egress;
use super::error::HortError;
use super::model::Domain;

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

    /// Whether `host` — already lowercased with any trailing dot removed —
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
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Suffix(
            Domain::new("example.com").unwrap(),
        )]);

        assert!(!policy.matches("example.com"));
    }

    #[test]
    fn egress_wildcard_does_not_match_host_without_dot_boundary() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Suffix(
            Domain::new("example.com").unwrap(),
        )]);

        assert!(!policy.matches("notexample.com"));
    }

    #[test]
    fn egress_wildcard_does_not_match_host_with_empty_left_label() {
        let policy = EgressPolicy::Allowlist(vec![HostPattern::Suffix(
            Domain::new("example.com").unwrap(),
        )]);

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
            allow: vec![
                "api.anthropic.com".to_string(),
                "*.githubusercontent.com".to_string(),
            ],
        };

        let policy = EgressPolicy::from_config(Some(&egress)).unwrap();

        assert!(policy.matches("api.anthropic.com"));
        assert!(policy.matches("raw.githubusercontent.com"));
    }

    #[test]
    fn egress_config_rejects_invalid_hostname_entry() {
        let egress = Egress::Allowlist {
            allow: vec!["https://api.anthropic.com/v1".to_string()],
        };

        let result = EgressPolicy::from_config(Some(&egress));

        assert!(matches!(result, Err(HortError::InvalidName)));
    }
}
