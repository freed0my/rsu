//! Health: the bar that drains while the map plays, fills with every hit
//! and ends the play when it is empty (unless No Fail is on).
//!
//! The numbers follow osu!stable's shape, from memory and not checked
//! against its source: a miss costs `DR(HP, 6, 25, 40)` of 200, a 300 gives
//! 6, a 100 gives 2.2, a 50 gives 0.4, slider parts a few points, and the
//! drain per microsecond is chosen per map so that a player who hits
//! everything for 300 never goes below `DR(HP, 100 %, 90 %, 80 %)`.
//! Everything that decides a play is integer: health is in 2^-40 of a full
//! bar, the drain rate in such units per microsecond, so that two slicings
//! of time agree to the unit.

use crate::chart::{difficulty_range, Chart, NestedKind, ObjectKind};
use crate::game::HitResult;
use crate::Time;

/// A full bar.
pub const FULL: i64 = 1 << 40;

/// Slider parts, for the health they give or cost.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Head,
    Tick,
    Repeat,
    Tail,
}

impl From<NestedKind> for Part {
    fn from(kind: NestedKind) -> Part {
        match kind {
            NestedKind::Tick => Part::Tick,
            NestedKind::Repeat => Part::Repeat,
            NestedKind::Tail => Part::Tail,
        }
    }
}

fn units(of_200: f64) -> i64 {
    (of_200 / 200.0 * FULL as f64).round() as i64
}

fn miss(hp: f32) -> i64 {
    -units(difficulty_range(hp, 6.0, 25.0, 40.0) as f64)
}

/// Health change for a final result on a map with this HP value.
pub fn result_delta(hp: f32, result: HitResult) -> i64 {
    match result {
        HitResult::Miss => miss(hp),
        HitResult::Hit50 => units(0.4),
        HitResult::Hit100 => units(2.2),
        HitResult::Hit300 => units(6.0),
    }
}

/// A slider part that was hit.
pub fn part_hit(part: Part) -> i64 {
    match part {
        Part::Head | Part::Tick => units(3.0),
        Part::Repeat | Part::Tail => units(4.0),
    }
}

/// Health for a live Spun Out full turn (two McOsu increments).
pub fn spinner_turn(bonus: bool) -> i64 {
    units(if bonus { 4.0 } else { 3.4 })
}

/// A slider part that was missed: a quarter of a miss.
pub fn part_miss(hp: f32) -> i64 {
    miss(hp) / 4
}

impl Chart {
    /// The part of `a..b` that drains health, in microseconds: inside the
    /// span from the first object to the end of the last, and not in a break.
    pub fn drain_time(&self, a: Time, b: Time) -> Time {
        let (Some(first), Some(last)) = (self.objects.first(), self.objects.iter().map(|o| o.end.max(o.t)).max())
        else {
            return 0;
        };
        let (a, b) = (a.max(first.t), b.min(last));
        if b <= a {
            return 0;
        }
        let mut total = b - a;
        for &(from, to) in &self.breaks {
            total -= (to.min(b) - from.max(a)).max(0);
        }
        total.max(0)
    }
}

/// Health units drained per microsecond of play, for this chart.
pub fn drain_rate(chart: &Chart) -> i64 {
    // Everything a perfect play collects, in order of time.
    let hp = chart.difficulty.hp;
    let mut gains: Vec<(Time, i64)> = Vec::with_capacity(chart.objects.len() * 2);
    for o in &chart.objects {
        match o.kind {
            ObjectKind::Circle | ObjectKind::Spinner => gains.push((o.end.max(o.t), result_delta(hp, HitResult::Hit300))),
            ObjectKind::Slider => {
                gains.push((o.t, part_hit(Part::Head)));
                for n in chart.slider_nested(chart.slider(o)) {
                    gains.push((n.t, part_hit(n.kind.into())));
                }
                gains.push((o.end, result_delta(hp, HitResult::Hit300)));
            }
        }
    }
    if gains.is_empty() {
        return 0;
    }
    gains.sort_by_key(|g| g.0);
    // Compute each interval once. In particular, do not rescan all objects
    // for their maximum end for every gain and every binary-search step.
    let first = chart.objects[0].t;
    let last_end = chart.objects.iter().map(|o| o.end.max(o.t)).max().unwrap();
    let mut previous = Time::MIN;
    let mut intervals = Vec::with_capacity(gains.len());
    for &(t, _) in &gains {
        let (a, b) = (previous.max(first), t.min(last_end));
        let mut total = if b > a { b - a } else { 0 };
        if b > a {
            for &(from, to) in &chart.breaks {
                total -= (to.min(b) - from.max(a)).max(0);
            }
        }
        intervals.push(total.max(0));
        previous = t;
    }
    let target = difficulty_range(hp, 1.0, 0.9, 0.8) as f64;
    // The lowest the bar gets for a given drain per microsecond, in bars.
    let lowest = |rate: f64| {
        let (mut bar, mut low) = (1.0f64, 1.0f64);
        for (&(_, gain), &duration) in gains.iter().zip(&intervals) {
            bar -= rate * duration as f64;
            low = low.min(bar);
            bar = (bar + gain as f64 / FULL as f64).min(1.0);
        }
        low
    };
    // The most drain that still keeps a perfect play above the target.
    let (mut lo, mut hi) = (0.0f64, 1e-3);
    for _ in 0..60 {
        let mid = (lo + hi) * 0.5;
        if lowest(mid) >= target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo * FULL as f64).round() as i64
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{ChartBuilder, Difficulty};
    use crate::MS;

    fn reference_drain_rate(chart: &Chart) -> i64 {
        // Everything a perfect play collects, in order of time.
        let hp = chart.difficulty.hp;
        let mut gains: Vec<(Time, i64)> = Vec::with_capacity(chart.objects.len() * 2);
        for o in &chart.objects {
            match o.kind {
                ObjectKind::Circle | ObjectKind::Spinner => gains.push((o.end.max(o.t), result_delta(hp, HitResult::Hit300))),
                ObjectKind::Slider => {
                    gains.push((o.t, part_hit(Part::Head)));
                    for n in chart.slider_nested(chart.slider(o)) {
                        gains.push((n.t, part_hit(n.kind.into())));
                    }
                    gains.push((o.end, result_delta(hp, HitResult::Hit300)));
                }
            }
        }
        if gains.is_empty() {
            return 0;
        }
        gains.sort_by_key(|g| g.0);
        let target = difficulty_range(hp, 1.0, 0.9, 0.8) as f64;
        // The lowest the bar gets for a given drain per microsecond, in bars.
        let lowest = |rate: f64| {
            let (mut bar, mut low, mut last) = (1.0f64, 1.0f64, Time::MIN);
            for &(t, gain) in &gains {
                bar -= rate * chart.drain_time(last, t) as f64;
                low = low.min(bar);
                bar = (bar + gain as f64 / FULL as f64).min(1.0);
                last = t;
            }
            low
        };
        // The most drain that still keeps a perfect play above the target.
        let (mut lo, mut hi) = (0.0f64, 1e-3);
        for _ in 0..60 {
            let mid = (lo + hi) * 0.5;
            if lowest(mid) >= target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo * FULL as f64).round() as i64
    }

    #[test]
    fn drain_rate_matches_reference_with_overlapping_objects_and_breaks() {
        for hp in [0.0, 5.0, 10.0] {
            for breaks in [&[][..], &[(1200, 2200), (1700, 2700), (1700, 2700)][..], &[(0, 5000)][..]] {
                let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 5.0, hp });
                b.breaks(breaks);
                b.spinner(1000 * MS, 4000 * MS);
                b.slider(1500 * MS, &[(0, 0), (1000, 0)], 2, 500 * MS,
                    &[(1750 * MS, NestedKind::Tick), (2000 * MS, NestedKind::Repeat), (2500 * MS, NestedKind::Tail)]);
                b.circle(2000 * MS, 0, 0);
                b.circle(3000 * MS, 0, 0);
                let chart = b.build();
                assert_eq!(drain_rate(&chart), reference_drain_rate(&chart), "hp={hp}, breaks={breaks:?}");
            }
        }
    }
}
