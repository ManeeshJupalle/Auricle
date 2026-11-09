//! Whether a retention sweep may believe the system clock.
//!
//! A sweep deletes by date and for good, so a wrong clock costs more here than
//! anywhere else: one ten years ahead makes every meeting look expired. So the
//! clock alone may only move the cutoff a short way. Each sweep's reading is
//! kept, and a sweep that finds the clock further on than `MAX_SWEEP_GAP`
//! deletes nothing until that reading has held for `CLOCK_HOLD`.
//!
//! That pauses rather than refuses for good because a long spell with the
//! daemon off looks exactly like a jump, and nothing local can tell them apart:
//! a false alarm costs a week's delay, a missed one costs the meetings. The
//! case this cannot catch is a clock that stays wrong through the whole hold.
//!
//! A clock behind is no threat to a sweep, which only deletes less. The danger
//! is in what gets recorded meanwhile, and `Store::sessions_older_than` deals
//! with that: a stamp is not believed over the order things were recorded in.

use serde::{Deserialize, Serialize};

pub(crate) const SECS_PER_DAY: i64 = 86_400;
/// The furthest the clock can move between sweeps and be taken at its word. A
/// jump inside this can only expire meetings this much early. Daily sweeps and
/// a weekend with the machine off fit; a false alarm past it only delays things.
const MAX_SWEEP_GAP: i64 = 3 * SECS_PER_DAY;
/// How long a reading past `MAX_SWEEP_GAP` must hold: time for someone to see
/// the wrong date. Measured on the clock itself, so a daemon that restarts
/// repeatedly doesn't wear it down, and one run monthly isn't held forever.
const CLOCK_HOLD: i64 = 7 * SECS_PER_DAY;

/// What is kept between sweeps, in the `retention_clock` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SweepClock {
    /// The clock at the last sweep that believed it.
    last_sweep: i64,
    /// When the clock was first found past `MAX_SWEEP_GAP`.
    held_since: Option<i64>,
}

impl SweepClock {
    /// A clock last known to read `at`.
    pub(crate) fn last_read(at: i64) -> SweepClock {
        SweepClock {
            last_sweep: at,
            held_since: None,
        }
    }

    /// Take a reading: the record to keep, and why this sweep must delete
    /// nothing, if it must.
    pub(crate) fn observe(self, now: i64) -> (SweepClock, Option<String>) {
        let gap = now.saturating_sub(self.last_sweep);
        // Backwards counts as believed: it deletes less, and the reading is
        // kept so that the jump back gets held like any other.
        if gap <= MAX_SWEEP_GAP {
            return (SweepClock::last_read(now), None);
        }
        let held_since = self.held_since.filter(|held| *held <= now).unwrap_or(now);
        let held_for = now - held_since;
        if held_for >= CLOCK_HOLD {
            return (SweepClock::last_read(now), None);
        }
        let notice = format!(
            "retention paused, nothing deleted: the system clock is {} day(s) past the last \
             sweep. If the date is wrong, correct it. If it is right, sweeps resume in {} day(s)",
            gap / SECS_PER_DAY,
            (CLOCK_HOLD - held_for + SECS_PER_DAY - 1) / SECS_PER_DAY,
        );
        let clock = SweepClock {
            last_sweep: self.last_sweep,
            held_since: Some(held_since),
        };
        (clock, Some(notice))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = SECS_PER_DAY;
    const T0: i64 = 1_800_000_000;

    /// Run readings through a fresh record, and say which were believed.
    fn believed(readings: &[i64]) -> Vec<bool> {
        let mut clock = SweepClock::last_read(T0);
        let mut verdicts = Vec::new();
        for now in readings {
            let (next, notice) = clock.observe(*now);
            clock = next;
            verdicts.push(notice.is_none());
        }
        verdicts
    }

    #[test]
    fn steady_sweeps_are_believed() {
        assert_eq!(
            believed(&[T0, T0 + DAY, T0 + 2 * DAY, T0 + 5 * DAY]),
            [true; 4]
        );
    }

    #[test]
    fn a_step_past_the_gap_is_held_until_it_has_lasted() {
        let jump = T0 + 3 * DAY + 1;
        assert_eq!(
            believed(&[jump, jump + DAY, jump + 7 * DAY - 1, jump + 7 * DAY]),
            [false, false, false, true]
        );
    }

    #[test]
    fn the_hold_ends_only_once() {
        let jump = T0 + 60 * DAY;
        assert_eq!(
            believed(&[jump, jump + 7 * DAY, jump + 8 * DAY]),
            [false, true, true]
        );
    }

    #[test]
    fn the_clock_coming_back_ends_the_hold() {
        assert_eq!(
            believed(&[T0 + 3_650 * DAY, T0 + DAY, T0 + 2 * DAY]),
            [false, true, true]
        );
    }

    #[test]
    fn a_clock_only_partly_corrected_starts_its_hold_again() {
        let wrong = T0 + 3_650 * DAY;
        let still_wrong = T0 + 1_000 * DAY;
        assert_eq!(
            believed(&[
                wrong,
                still_wrong,
                still_wrong + 6 * DAY,
                still_wrong + 7 * DAY
            ]),
            [false, false, false, true]
        );
    }

    #[test]
    fn a_clock_behind_is_believed_and_the_way_back_is_held() {
        // A dead battery, then the time service putting it right.
        let behind = T0 - 7_300 * DAY;
        assert_eq!(believed(&[behind, T0, T0 + 7 * DAY]), [true, false, true]);
    }

    #[test]
    fn a_held_sweep_says_how_far_and_how_long() {
        let (_, notice) = SweepClock::last_read(T0).observe(T0 + 60 * DAY);
        let notice = notice.unwrap();
        assert!(notice.contains("60 day(s)"), "{notice}");
        assert!(notice.contains("resume in 7 day(s)"), "{notice}");
    }

    #[test]
    fn the_record_survives_the_settings_table() {
        let (clock, _) = SweepClock::last_read(T0).observe(T0 + 60 * DAY);
        let stored = serde_json::to_string(&clock).unwrap();
        assert_eq!(serde_json::from_str::<SweepClock>(&stored).unwrap(), clock);
    }
}
