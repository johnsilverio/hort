//! `SystemClock`: the real `Clock`, reading the host wall clock so age and idle
//! are measured against the present moment.

use std::time::SystemTime;

use crate::ports::Clock;

/// A `Clock` backed by the system wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
