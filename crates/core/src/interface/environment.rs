//! `Environment` — the two inputs that make a run different from the last one.
//!
//! Wall-clock time and fresh identifiers are read from the operating system in
//! nearly two hundred places. Replacing all of them would be busywork: most
//! are `Instant::now()` measuring how long something took, and a latency
//! number that varies between runs is the number doing its job.
//!
//! The ones that matter are the ones whose answers are *kept*: a timestamp
//! written into the session log, an id that names a log entry forever, a date
//! that goes into the prompt the model reads. Those decide whether the same
//! conversation, replayed, is the same conversation. They come from here.
//!
//! # Not `Instant`
//!
//! There is deliberately no monotonic clock on this contract. `Instant` cannot
//! be formatted, stored or transmitted — everything it is used for is
//! measurement, and measurement is exactly the category this is not for.
//! Putting it here would invite hundreds of mechanical substitutions that buy
//! nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use time::OffsetDateTime;

use crate::id::Id;

/// Where kept time and kept identifiers come from.
pub trait Environment: Send + Sync {
    /// Wall-clock now.
    fn now(&self) -> OffsetDateTime;

    /// An identifier nothing else will be given.
    fn new_id(&self) -> Id;
}

/// The machine's own answers.
pub struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn new_id(&self) -> Id {
        Id::new()
    }
}

/// Time that moves only when it is asked, and identifiers that count.
///
/// The second implementation, and the one the whole contract is for: two runs
/// of the same conversation under one of these produce the same log, byte for
/// byte. That is the property a recorded session is replayed against, and it
/// is unachievable while a timestamp and a UUID are read from the machine.
///
/// Each call to `now` advances the clock by `step`, so successive entries are
/// ordered the way real ones are rather than sharing one instant.
pub struct FixedEnvironment {
    start: OffsetDateTime,
    step: time::Duration,
    ticks: AtomicU64,
    ids: AtomicU64,
}

impl FixedEnvironment {
    pub fn new(start: OffsetDateTime, step: time::Duration) -> Self {
        Self {
            start,
            step,
            ticks: AtomicU64::new(0),
            ids: AtomicU64::new(0),
        }
    }

    /// The epoch, one second per reading.
    pub fn epoch() -> Self {
        Self::new(OffsetDateTime::UNIX_EPOCH, time::Duration::seconds(1))
    }
}

impl Environment for FixedEnvironment {
    fn now(&self) -> OffsetDateTime {
        let n = self.ticks.fetch_add(1, Ordering::SeqCst);
        self.start + self.step * (n as i32)
    }

    fn new_id(&self) -> Id {
        let n = self.ids.fetch_add(1, Ordering::SeqCst);
        let mut bytes = [0u8; 16];
        bytes[8..].copy_from_slice(&n.to_be_bytes());
        Id::from_bytes(bytes)
    }
}

/// The machine's clock moved to another starting point: it reads `start` when made, and from
/// there runs at wall-clock speed. Identifiers stay fresh.
///
/// For a session whose kept time has to be some other day than today — replaying a recording, so
/// the date in its prompt is the day it was recorded — while the order and spacing of what it
/// writes stay real.
pub struct ShiftedEnvironment {
    start: OffsetDateTime,
    origin: Instant,
}

impl ShiftedEnvironment {
    pub fn starting_at(start: OffsetDateTime) -> Self {
        Self {
            start,
            origin: Instant::now(),
        }
    }

    /// From milliseconds since the Unix epoch; `None` when out of range.
    pub fn starting_at_ms(ms: u64) -> Option<Self> {
        let ns = i128::from(ms).checked_mul(1_000_000)?;
        OffsetDateTime::from_unix_timestamp_nanos(ns)
            .ok()
            .map(Self::starting_at)
    }
}

impl Environment for ShiftedEnvironment {
    fn now(&self) -> OffsetDateTime {
        self.start + self.origin.elapsed()
    }

    fn new_id(&self) -> Id {
        Id::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_clock_advances_one_step_per_reading() {
        let env = FixedEnvironment::epoch();
        let a = env.now();
        let b = env.now();
        assert_eq!(a, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            b - a,
            time::Duration::seconds(1),
            "entries must still order"
        );
    }

    #[test]
    fn a_shifted_clock_starts_where_it_is_told_and_keeps_running() {
        let start = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let env = ShiftedEnvironment::starting_at(start);
        let a = env.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = env.now();
        assert!(a >= start && a - start < time::Duration::seconds(1));
        assert!(b > a, "it runs");
        assert_ne!(env.new_id(), env.new_id());
        let from_ms = ShiftedEnvironment::starting_at_ms(1_761_955_200_000)
            .unwrap()
            .now();
        assert_eq!(from_ms.date().to_string(), "2025-11-01");
    }

    #[test]
    fn two_runs_of_the_same_length_agree_on_every_answer() {
        let one = FixedEnvironment::epoch();
        let two = FixedEnvironment::epoch();
        for _ in 0..8 {
            assert_eq!(one.now(), two.now());
            assert_eq!(one.new_id(), two.new_id());
        }
    }

    #[test]
    fn fixed_identifiers_are_still_distinct() {
        let env = FixedEnvironment::epoch();
        let ids: Vec<Id> = (0..64).map(|_| env.new_id()).collect();
        let mut sorted: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            64,
            "a counter that repeats is not an id source"
        );
    }

    #[test]
    fn the_system_environment_does_not_repeat_itself() {
        let env = SystemEnvironment;
        assert_ne!(env.new_id(), env.new_id());
    }
}
