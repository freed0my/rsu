//! A perfect player, as an input stream. Used by tests, benchmarks and as a
//! stand-in for a human when exercising the game without one.

use crate::chart::{Chart, ObjectKind, CENTRE};
use crate::game::{Button, Input, InputKind};
use crate::{Fx, Time, FX_ONE, MS};

/// How fast the perfect player spins: comfortably above any requirement,
/// just below the cap.
const SPIN_RPM: f64 = 450.0;

/// Inputs that hit every object exactly on time, alternating keys, riding
/// slider balls and spinning spinners.
///
/// With `move_rate_hz > 0` the cursor travels in straight lines between
/// objects and follows sliders sampled at that rate, like a real pointing
/// device would report; with 0 it jumps to exactly where it has to be.
pub fn autoplay(chart: &Chart, move_rate_hz: u32) -> Vec<Input> {
    let mut inputs = Vec::new();
    let step = if move_rate_hz > 0 { 1_000_000 / move_rate_hz as Time } else { 0 };
    let tap = 30 * MS;
    let Some(first) = chart.objects.first() else { return inputs };
    let (mut px, mut py, mut pt) = (first.x, first.y, first.t - 500 * MS);
    let buttons = [Button::K1, Button::K2];
    let mut busy_until = [Time::MIN; 2];
    let mut key = 0;
    let mv = |inputs: &mut Vec<Input>, t: Time, (x, y): (Fx, Fx)| {
        inputs.push(Input { t, kind: InputKind::Move { x, y } });
    };

    for (n, o) in chart.objects.iter().enumerate() {
        let spin_radius = 100 * FX_ONE;
        let start = match o.kind {
            ObjectKind::Spinner => (CENTRE.0 + spin_radius, CENTRE.1),
            _ => (o.x, o.y),
        };
        // Arrive a millisecond early so the position is settled at the
        // press, but never leave before the previous object is dealt with.
        let arrive = (o.t - MS).max(pt);
        if step > 0 {
            let mut t = pt + step;
            while t < arrive {
                let f = (t - pt) as f64 / (arrive - pt) as f64;
                let at = (px + ((start.0 - px) as f64 * f) as i32, py + ((start.1 - py) as f64 * f) as i32);
                mv(&mut inputs, t, at);
                t += step;
            }
        }
        mv(&mut inputs, arrive, start);

        // Alternate, unless the other key is still holding something.
        if busy_until[key] > o.t && busy_until[1 - key] <= o.t {
            key = 1 - key;
        }
        let button = buttons[key];
        inputs.push(Input { t: o.t, kind: InputKind::Press(button) });

        let (release, rest) = match o.kind {
            ObjectKind::Circle => {
                // Let go before this key is needed again two objects later.
                let release = match chart.objects.get(n + 2) {
                    Some(next) => (o.t + tap).min(o.t + (next.t - o.t) / 2),
                    None => o.t + tap,
                };
                (release.max(o.t), start)
            }
            ObjectKind::Slider => {
                let s = chart.slider(o);
                if step > 0 {
                    let mut t = o.t + step;
                    while t < o.end {
                        mv(&mut inputs, t, chart.ball_at(o, t));
                        t += step;
                    }
                }
                // Be exactly on the ball wherever it is judged.
                for nested in chart.slider_nested(s) {
                    mv(&mut inputs, nested.t, (nested.x, nested.y));
                }
                let end = chart.ball_at(o, o.end);
                mv(&mut inputs, o.end, end);
                (o.end + 1, end)
            }
            ObjectKind::Spinner => {
                let dt = if step > 0 { step } else { 10 * MS };
                let mut t = o.t;
                let mut at = start;
                while t <= o.end {
                    let a = (t - o.t) as f64 / 60e6 * SPIN_RPM * std::f64::consts::TAU;
                    at = (
                        CENTRE.0 + (spin_radius as f64 * a.cos()) as i32,
                        CENTRE.1 + (spin_radius as f64 * a.sin()) as i32,
                    );
                    mv(&mut inputs, t, at);
                    t += dt;
                }
                (o.end + 1, at)
            }
        };
        inputs.push(Input { t: release, kind: InputKind::Release(button) });
        busy_until[key] = release;
        key = 1 - key;
        // The cursor stays put until the press (or the end of a hold);
        // leaving earlier would reorder same-time objects into
        // "move, move, press, press".
        (px, py) = rest;
        // A held object lets the cursor go the moment it ends, even though
        // the key comes up a microsecond later: the next object may start
        // at that very instant.
        pt = if o.kind == ObjectKind::Circle { o.t } else { o.end };
    }
    inputs.sort_by_key(|i| i.t); // stable: same-time events keep their order
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{ChartBuilder, Difficulty, NestedKind};
    use crate::game::Game;
    use crate::to_fx;
    use std::sync::Arc;

    #[test]
    fn autoplay_gets_a_perfect_score_even_on_awkward_maps() {
        let mut b = ChartBuilder::new(Difficulty { cs: 5.0, od: 9.0, ar: 9.0, hp: 5.0 });
        // Doubles 1 ms apart, a stack and a 20 ms stream.
        let times = [1000, 1001, 1500, 1500, 2000, 2020, 2040, 2060, 2080, 3000];
        for (i, &ms) in times.iter().enumerate() {
            b.circle(ms * MS, to_fx(50.0 + (i as f32 * 97.0) % 400.0), to_fx(60.0 + (i as f32 * 53.0) % 300.0));
        }
        // A fast three-pass slider with ticks, straight into a spinner,
        // straight into a circle.
        let p = |x: f32, y: f32| (to_fx(x), to_fx(y));
        b.slider(
            3_200_000,
            &[p(100.0, 100.0), p(300.0, 100.0), p(300.0, 250.0)],
            3,
            150_000,
            &[
                (3_275_000, NestedKind::Tick),
                (3_350_000, NestedKind::Repeat),
                (3_425_000, NestedKind::Tick),
                (3_500_000, NestedKind::Repeat),
                (3_575_000, NestedKind::Tick),
                (3_614_000, NestedKind::Tail),
            ],
        );
        b.spinner(3_700_000, 5_000_000);
        b.circle(5_050_000, to_fx(400.0), to_fx(300.0));
        let chart = Arc::new(b.build());
        for rate in [0, 133, 1000] {
            let inputs = autoplay(&chart, rate);
            assert!(inputs.windows(2).all(|w| w[0].t <= w[1].t));
            let s = Game::simulate(chart.clone(), 0, &inputs, 6_000_000);
            assert_eq!((s.n300, s.n100, s.n50, s.miss), (13, 0, 0, 0), "rate {rate}");
            // 11 circles, 7 slider parts, 1 spinner.
            assert_eq!(s.max_combo, 19, "rate {rate}");
        }
    }
}
