//! Pure age and idle arithmetic over `SystemTime`, plus the one timestamp parse
//! bridge. `ls` reports how long a sandbox has been alive and how long it has
//! been idle; both are decisions over plain timestamps a caller supplies, with
//! no access to the record, a clock port, or the filesystem.

use std::time::{Duration, SystemTime};

use super::error::HortError;

/// Whether a sandbox is doing anything. `Active` whenever a non-anchor process
/// is running; otherwise `Idle` carrying the elapsed time since its last sign of
/// activity.
#[derive(Debug, PartialEq)]
pub enum IdleState {
    Active,
    Idle(Duration),
}

/// Parse a persisted timestamp into a `SystemTime`. hort writes timestamps as
/// RFC 3339 in UTC with a `Z` suffix, so a localized offset is rejected even
/// though it is otherwise valid RFC 3339. A failure carries a human-readable
/// detail and is matched by variant, not by its rendered text.
pub fn parse_timestamp(value: &str) -> Result<SystemTime, HortError> {
    humantime::parse_rfc3339(value).map_err(|err| HortError::InvalidTimestamp {
        detail: format!("'{value}': {err}"),
    })
}

/// How long the sandbox has existed: `now` minus its creation time, saturating
/// to zero if the creation time is in the future (clock skew).
pub fn age(created_at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(created_at).unwrap_or(Duration::ZERO)
}

/// The idle state of a sandbox. A running session (`session_count >= 1`) makes
/// it `Active` regardless of timestamps; otherwise idle is `now` minus the most
/// recent of creation, last attach, and last notify event, saturating to zero.
/// `last_event_at` is `None` when the sandbox has no notify channel.
pub fn idle(
    session_count: usize,
    created_at: SystemTime,
    last_attach_at: SystemTime,
    last_event_at: Option<SystemTime>,
    now: SystemTime,
) -> IdleState {
    if session_count >= 1 {
        return IdleState::Active;
    }

    let mut most_recent = created_at.max(last_attach_at);
    if let Some(last_event_at) = last_event_at {
        most_recent = most_recent.max(last_event_at);
    }

    IdleState::Idle(now.duration_since(most_recent).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn idle_is_active_when_session_running() {
        let created = UNIX_EPOCH + Duration::from_secs(1000);
        let now = UNIX_EPOCH + Duration::from_secs(100_000);

        let state = idle(1, created, created, None, now);

        assert_eq!(state, IdleState::Active);
    }

    #[test]
    fn idle_counts_from_last_attach() {
        let created = UNIX_EPOCH + Duration::from_secs(1000);
        let last_attach = UNIX_EPOCH + Duration::from_secs(5000);
        let now = UNIX_EPOCH + Duration::from_secs(8000);

        let state = idle(0, created, last_attach, None, now);

        assert_eq!(state, IdleState::Idle(Duration::from_secs(3000)));
    }

    #[test]
    fn idle_counts_from_last_notify_event() {
        let created = UNIX_EPOCH + Duration::from_secs(1000);
        let last_attach = UNIX_EPOCH + Duration::from_secs(2000);
        let last_event = UNIX_EPOCH + Duration::from_secs(6000);
        let now = UNIX_EPOCH + Duration::from_secs(9000);

        let state = idle(0, created, last_attach, Some(last_event), now);

        assert_eq!(state, IdleState::Idle(Duration::from_secs(3000)));
    }

    #[test]
    fn age_is_now_minus_created() {
        let created = UNIX_EPOCH + Duration::from_secs(1000);
        let now = UNIX_EPOCH + Duration::from_secs(4500);

        assert_eq!(age(created, now), Duration::from_secs(3500));
    }

    #[test]
    fn age_saturates_to_zero_when_created_in_future() {
        let created = UNIX_EPOCH + Duration::from_secs(9000);
        let now = UNIX_EPOCH + Duration::from_secs(1000);

        assert_eq!(age(created, now), Duration::from_secs(0));
    }

    #[test]
    fn idle_saturates_to_zero_when_all_timestamps_in_future() {
        let future = UNIX_EPOCH + Duration::from_secs(9000);
        let now = UNIX_EPOCH + Duration::from_secs(1000);

        let state = idle(0, future, future, Some(future), now);

        assert_eq!(state, IdleState::Idle(Duration::from_secs(0)));
    }

    #[test]
    fn parse_timestamp_accepts_rfc3339_utc() {
        let parsed = parse_timestamp("1970-01-02T00:00:00Z").unwrap();

        assert_eq!(parsed, UNIX_EPOCH + Duration::from_secs(86400));
    }

    #[test]
    fn parse_timestamp_rejects_malformed_input() {
        let result = parse_timestamp("not a timestamp");

        assert!(matches!(result, Err(HortError::InvalidTimestamp { .. })));
    }

    #[test]
    fn parse_timestamp_rejects_localized_utc_offset() {
        let result = parse_timestamp("1970-01-02T00:00:00-03:00");

        assert!(matches!(result, Err(HortError::InvalidTimestamp { .. })));
    }
}
