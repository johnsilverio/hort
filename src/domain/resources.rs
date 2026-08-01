//! The per-sandbox resource ceiling: what the configured `resources` block asks
//! for, narrowed to what the host's cgroup delegation can actually enforce. A
//! pure decision over plain data.
//!
//! The two failure modes are deliberately different. A memory size that does not
//! parse, or is not positive, is malformed configuration and fails fast. A
//! controller the host does not delegate is not the user's mistake: the cap is
//! dropped, the rest still applies, and a warning names what went unenforced.

use crate::domain::config::Resources;
use crate::domain::error::HortError;
use crate::domain::model::{CgroupCaps, Warning};
use crate::ports::ResourceLimits;

/// Decide the ceiling a sandbox is built with, plus the advisories the user has
/// to hear about it.
///
/// `memory` is a positive integer with an optional `k`/`m`/`g`/`t` suffix, each
/// accepting an optional `b` or `ib`, case-insensitive and base 1024, with no
/// suffix meaning raw bytes: `4g`, `4G`, `4gb` and `4GiB` all mean 4 GiB, and
/// `4096` means 4096 bytes. A value outside that shape is
/// [`HortError::InvalidConfig`]. A cap whose controller is undelegated is
/// dropped with a [`Warning`] naming it, never an error.
pub fn resource_limits(
    resources: Option<&Resources>,
    cgroup: &CgroupCaps,
) -> Result<(Option<ResourceLimits>, Vec<Warning>), HortError> {
    let Some(resources) = resources else {
        return Ok((None, Vec::new()));
    };

    // Both values are read before any controller is consulted: a typo stays an
    // error on a host that could not have enforced the cap anyway, instead of
    // waiting to surprise the user on the next machine.
    let memory_bytes = resources.memory.as_deref().map(parse_memory).transpose()?;
    let cpus = resources.cpus.map(positive_cpus).transpose()?;

    let mut warnings = Vec::new();
    let memory_bytes = enforceable(memory_bytes, cgroup.memory, "memory", &mut warnings);
    let cpus = enforceable(cpus, cgroup.cpu, "cpu", &mut warnings);

    let ceiling =
        (memory_bytes.is_some() || cpus.is_some()).then_some(ResourceLimits { memory_bytes, cpus });
    Ok((ceiling, warnings))
}

/// Keep a requested cap only if the host delegates the controller that would
/// enforce it, warning about the one it drops.
fn enforceable<T>(
    cap: Option<T>,
    delegated: bool,
    controller: &str,
    warnings: &mut Vec<Warning>,
) -> Option<T> {
    if cap.is_some() && !delegated {
        warnings.push(Warning::new(format!(
            "cgroup controller '{controller}' is not delegated to this user, so the {controller} ceiling is not enforced; add '{controller}' to Delegate= in a systemd drop-in for user@.service"
        )));
        return None;
    }
    cap
}

/// Read a configured memory size as a byte count.
fn parse_memory(value: &str) -> Result<u64, HortError> {
    let lowered = value.to_ascii_lowercase();
    let sized =
        lowered.strip_suffix("ib").or_else(|| lowered.strip_suffix('b')).unwrap_or(&lowered);

    let (amount, multiplier) = match sized.chars().last() {
        Some(unit) if unit.is_ascii_alphabetic() => (
            &sized[..sized.len() - 1],
            unit_multiplier(unit).ok_or_else(|| malformed_memory(value))?,
        ),
        _ => (sized, 1),
    };

    let bytes = amount
        .parse::<u64>()
        .ok()
        .and_then(|amount| amount.checked_mul(multiplier))
        .ok_or_else(|| malformed_memory(value))?;

    if bytes == 0 {
        return Err(malformed_memory(value));
    }
    Ok(bytes)
}

fn unit_multiplier(unit: char) -> Option<u64> {
    const KIB: u64 = 1024;
    match unit {
        'k' => Some(KIB),
        'm' => Some(KIB * KIB),
        'g' => Some(KIB * KIB * KIB),
        't' => Some(KIB * KIB * KIB * KIB),
        _ => None,
    }
}

fn malformed_memory(value: &str) -> HortError {
    HortError::InvalidConfig {
        detail: format!(
            "resources.memory '{value}' is not a positive size like \"4g\" (an integer with an optional k, m, g or t suffix, base 1024)"
        ),
    }
}

fn positive_cpus(cpus: f64) -> Result<f32, HortError> {
    if cpus <= 0.0 {
        return Err(HortError::InvalidConfig {
            detail: format!("resources.cpus {cpus} is not a positive number of cores"),
        });
    }
    Ok(cpus as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegated_host() -> CgroupCaps {
        CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false }
    }

    #[test]
    fn resource_limits_parses_the_gibibyte_suffix() {
        let resources = Resources { memory: Some("4g".to_string()), cpus: None };

        let (limits, _warnings) = resource_limits(Some(&resources), &delegated_host()).unwrap();

        assert_eq!(
            limits.expect("a resources block yields a ceiling").memory_bytes,
            Some(4_294_967_296)
        );
    }

    #[test]
    fn resource_limits_parses_a_size_written_the_docker_way() {
        let resources = Resources { memory: Some("4GiB".to_string()), cpus: None };

        let (limits, _warnings) = resource_limits(Some(&resources), &delegated_host()).unwrap();

        assert_eq!(
            limits.expect("a resources block yields a ceiling").memory_bytes,
            Some(4_294_967_296)
        );
    }

    #[test]
    fn resource_limits_reads_a_bare_number_as_bytes() {
        let resources = Resources { memory: Some("4096".to_string()), cpus: None };

        let (limits, _warnings) = resource_limits(Some(&resources), &delegated_host()).unwrap();

        assert_eq!(limits.expect("a resources block yields a ceiling").memory_bytes, Some(4096));
    }

    #[test]
    fn resource_limits_rejects_a_malformed_memory_value() {
        let resources = Resources { memory: Some("4gigs".to_string()), cpus: None };

        let result = resource_limits(Some(&resources), &delegated_host());

        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }

    #[test]
    fn resource_limits_rejects_a_non_positive_memory_value() {
        let resources = Resources { memory: Some("0".to_string()), cpus: None };

        let result = resource_limits(Some(&resources), &delegated_host());

        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }

    #[test]
    fn resource_limits_drops_the_memory_cap_when_the_controller_is_not_delegated() {
        let resources = Resources { memory: Some("4g".to_string()), cpus: Some(2.0) };
        let cgroup = CgroupCaps { memory: false, ..delegated_host() };

        let (limits, warnings) = resource_limits(Some(&resources), &cgroup).unwrap();

        assert_eq!(limits.expect("the delegated caps still apply").memory_bytes, None);
        assert!(warnings.iter().any(|warning| warning.to_string().contains("memory")));
    }

    #[test]
    fn resource_limits_drops_the_cpu_cap_when_the_controller_is_not_delegated() {
        let resources = Resources { memory: Some("4g".to_string()), cpus: Some(2.0) };
        let cgroup = CgroupCaps { cpu: false, ..delegated_host() };

        let (limits, warnings) = resource_limits(Some(&resources), &cgroup).unwrap();

        assert_eq!(limits.expect("the delegated caps still apply").cpus, None);
        assert!(warnings.iter().any(|warning| warning.to_string().contains("cpu")));
    }

    #[test]
    fn resource_limits_is_absent_without_a_resources_block() {
        let (limits, _warnings) = resource_limits(None, &delegated_host()).unwrap();

        assert!(limits.is_none());
    }

    #[test]
    fn resource_limits_keeps_both_caps_on_a_fully_delegated_host() {
        let resources = Resources { memory: Some("4g".to_string()), cpus: Some(2.0) };

        let (limits, _warnings) = resource_limits(Some(&resources), &delegated_host()).unwrap();

        let limits = limits.expect("a resources block yields a ceiling");
        assert_eq!(limits.memory_bytes, Some(4_294_967_296));
        assert_eq!(limits.cpus, Some(2.0));
    }

    #[test]
    fn resource_limits_reports_no_warning_on_a_fully_delegated_host() {
        let resources = Resources { memory: Some("4g".to_string()), cpus: Some(2.0) };

        let (_limits, warnings) = resource_limits(Some(&resources), &delegated_host()).unwrap();

        assert!(warnings.is_empty());
    }

    #[test]
    fn resource_limits_is_absent_when_no_controller_is_delegated() {
        let resources = Resources { memory: Some("4g".to_string()), cpus: Some(2.0) };
        let cgroup = CgroupCaps { memory: false, cpu: false, ..delegated_host() };

        let (limits, _warnings) = resource_limits(Some(&resources), &cgroup).unwrap();

        assert!(limits.is_none());
    }

    #[test]
    fn resource_limits_rejects_a_non_positive_cpus_value() {
        let resources = Resources { memory: None, cpus: Some(0.0) };

        let result = resource_limits(Some(&resources), &delegated_host());

        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }

    #[test]
    fn resource_limits_prefers_the_parse_error_over_the_missing_controller() {
        let resources = Resources { memory: Some("4gigs".to_string()), cpus: None };
        let cgroup = CgroupCaps { memory: false, ..delegated_host() };

        let result = resource_limits(Some(&resources), &cgroup);

        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }
}
