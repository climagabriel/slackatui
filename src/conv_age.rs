//! How old a conversation's newest message is, as one of four groups, and the
//! line the conversations pane draws under each group under the `Recent` sort.
//!
//! Age here is elapsed time, not a calendar date: no timezone is involved and
//! the boundaries are 24 h, 48 h and 168 h before one `now` captured per
//! render. The intervals are half-open — a conversation exactly 24 h old is
//! `Yesterday`, not `Today` — so every age lands in exactly one group and the
//! boundaries do not overlap.
//!
//! The pane draws a line under the last conversation of each group, so a group
//! with no conversations draws nothing, and only under `Recent`: that is the
//! one sort whose order is the order these groups are in.

pub const DAY: i64 = 24 * 60 * 60;

/// The same day in the unit a message id is in. The comparison happens here
/// rather than on whole seconds: flooring an id to its second would call a
/// conversation 23:59:59.999999 old `Yesterday`, and one a microsecond in the
/// future `Today`.
const DAY_US: i64 = DAY * 1_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgeGroup {
    Today,
    Yesterday,
    ThisWeek,
    Earlier,
}

impl AgeGroup {
    pub const fn label(self) -> &'static str {
        match self {
            AgeGroup::Today => "today",
            AgeGroup::Yesterday => "yesterday",
            AgeGroup::ThisWeek => "this week",
            AgeGroup::Earlier => "earlier",
        }
    }

    /// The group a conversation whose newest message is `last_id` falls in at
    /// `now_secs`. `last_id` is a message id, microseconds since the epoch, the
    /// same figure the `Recent` sort orders by.
    ///
    /// A conversation with no newest message, and one whose newest message is
    /// dated in the future, are both `Earlier`: neither has an age this can
    /// place, and the last group is the one that promises least about the age
    /// of what it holds.
    ///
    /// The arithmetic is done in `i128`: `now_secs` comes from a clock a test
    /// can pin, so a value whose microsecond count overflows an `i64` has to
    /// answer the right group instead of panicking a draw.
    pub fn of(last_id: i64, now_secs: i64) -> Self {
        if last_id <= 0 {
            return AgeGroup::Earlier;
        }
        let age_us = i128::from(now_secs) * 1_000_000 - i128::from(last_id);
        match age_us {
            _ if age_us < 0 => AgeGroup::Earlier,
            _ if age_us < i128::from(DAY_US) => AgeGroup::Today,
            _ if age_us < 2 * i128::from(DAY_US) => AgeGroup::Yesterday,
            _ if age_us < 7 * i128::from(DAY_US) => AgeGroup::ThisWeek,
            _ => AgeGroup::Earlier,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgeGroup, DAY};

    /// A message id for something `secs` old at `now`.
    fn aged(now: i64, secs: i64) -> i64 {
        (now - secs) * 1_000_000
    }

    #[test]
    fn each_boundary_belongs_to_the_older_group() {
        let now = 1_700_000_000;
        for (age, group) in [
            (0, AgeGroup::Today),
            (DAY - 1, AgeGroup::Today),
            (DAY, AgeGroup::Yesterday),
            (DAY + 1, AgeGroup::Yesterday),
            (2 * DAY - 1, AgeGroup::Yesterday),
            (2 * DAY, AgeGroup::ThisWeek),
            (2 * DAY + 1, AgeGroup::ThisWeek),
            (7 * DAY - 1, AgeGroup::ThisWeek),
            (7 * DAY, AgeGroup::Earlier),
            (7 * DAY + 1, AgeGroup::Earlier),
        ] {
            assert_eq!(AgeGroup::of(aged(now, age), now), group, "{age} seconds old");
        }
    }

    /// Neither an unknown nor a future timestamp has an age to place.
    #[test]
    fn a_missing_or_future_timestamp_is_earlier() {
        let now = 1_700_000_000;
        assert_eq!(AgeGroup::of(0, now), AgeGroup::Earlier);
        assert_eq!(AgeGroup::of(-1, now), AgeGroup::Earlier);
        assert_eq!(AgeGroup::of(aged(now, -3600), now), AgeGroup::Earlier);
    }

    /// The sub-second part of a message id counts toward the age: only an id
    /// a whole 24 h old is `Yesterday`, and a microsecond in the future is
    /// already `Earlier`. Flooring the id to its second got both wrong.
    #[test]
    fn the_microseconds_of_an_id_count_toward_the_age() {
        let now = 1_700_000_000;
        // 86 398.5 s old.
        assert_eq!(AgeGroup::of(aged(now, DAY - 1) + 500_000, now), AgeGroup::Today);
        // 86 399.000001 s old: still under 24 h, so still today.
        assert_eq!(AgeGroup::of(aged(now, DAY) + 999_999, now), AgeGroup::Today);
        // Exactly 24 h.
        assert_eq!(AgeGroup::of(aged(now, DAY), now), AgeGroup::Yesterday);
        // One microsecond ahead of the clock.
        assert_eq!(AgeGroup::of(now * 1_000_000 + 1, now), AgeGroup::Earlier);
    }

    /// A clock a test can pin is a clock that can be absurd; no value of it
    /// overflows the microsecond count into a panic mid-draw.
    #[test]
    fn an_extreme_clock_answers_a_group_rather_than_panicking() {
        assert_eq!(AgeGroup::of(1_000_000, i64::MIN), AgeGroup::Earlier);
        assert_eq!(AgeGroup::of(1_000_000, i64::MAX), AgeGroup::Earlier);
        // An id of i64::MAX microseconds is about 292,000 years after the epoch;
        // a clock of i64::MAX seconds is a million times later, so the id is
        // ancient rather than new.
        assert_eq!(AgeGroup::of(i64::MAX, i64::MAX), AgeGroup::Earlier);
    }
}
