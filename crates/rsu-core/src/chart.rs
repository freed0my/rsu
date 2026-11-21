//! A beatmap in the form the rules need: objects in time order, slider
//! paths as integer polylines, and the values derived from difficulty.

use crate::{to_fx, Fx, Time, FX_ONE, MS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Circle,
    Slider,
    Spinner,
}

#[derive(Clone, Copy, Debug)]
pub struct HitObject {
    pub t: Time,
    /// When the object is over: `t` for circles, the last moment of a
    /// slider or spinner.
    pub end: Time,
    pub x: Fx,
    pub y: Fx,
    pub kind: ObjectKind,
    /// 1-based number shown on the object.
    pub combo_number: u16,
    /// Which combo colour to use (counted across the map, to be wrapped by
    /// the skin's palette size).
    pub combo_index: u16,
    /// Sliders: index into [`Chart::sliders`]. Spinners: half rotations
    /// needed to clear it.
    pub extra: u32,
}

/// One vertex of a slider path with its distance from the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathPoint {
    pub x: Fx,
    pub y: Fx,
    pub d: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NestedKind {
    Tick,
    Repeat,
    /// The slider's end, judged slightly before it actually ends.
    Tail,
}

/// A point inside a slider where the player has to be following the ball.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nested {
    pub t: Time,
    pub kind: NestedKind,
    /// Where the ball is at `t`.
    pub x: Fx,
    pub y: Fx,
}

#[derive(Clone, Copy, Debug)]
pub struct Slider {
    /// Range in [`Chart::path`].
    pub path_start: u32,
    pub path_len: u32,
    /// Length of one pass over the path, in [`Fx`] units.
    pub length: i64,
    /// Number of passes (1 = no repeats).
    pub spans: u32,
    pub span_duration: Time,
    /// Range in [`Chart::nested`], in time order.
    pub nested_start: u32,
    pub nested_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Difficulty {
    pub cs: f32,
    pub od: f32,
    pub ar: f32,
    pub hp: f32,
}

#[derive(Clone, Debug)]
pub struct Chart {
    /// Sorted by start time.
    pub objects: Vec<HitObject>,
    pub sliders: Vec<Slider>,
    pub path: Vec<PathPoint>,
    pub nested: Vec<Nested>,
    pub difficulty: Difficulty,
    /// Hit circle radius.
    pub radius: Fx,
    /// How far the cursor may stray from the slider ball once following.
    pub follow_radius: Fx,
    /// How long before its time an object appears, and how long it fades in.
    pub preempt: Time,
    pub fade_in: Time,
    /// Largest |hit time - object time| that still earns each result.
    pub window_300: Time,
    pub window_100: Time,
    pub window_50: Time,
    /// The score formula's difficulty multiplier.
    pub score_multiplier: u32,
    /// Break periods, in map time; health does not drain in them.
    pub breaks: Vec<(Time, Time)>,
    /// Health drained per microsecond of play, in [`crate::hp::FULL`] units.
    pub drain_rate: i64,
}

/// osu!'s piecewise-linear difficulty scale: `min` at 0, `mid` at 5, `max` at 10.
pub fn difficulty_range(d: f32, min: f32, mid: f32, max: f32) -> f32 {
    if d > 5.0 {
        mid + (max - mid) * (d - 5.0) / 5.0
    } else if d < 5.0 {
        mid - (mid - min) * (5.0 - d) / 5.0
    } else {
        mid
    }
}

/// Round half to even, as C#'s `Math.Round` does in the original formulas.
fn round_half_even(v: f64) -> f64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - v.signum()
    } else {
        r
    }
}

/// How early a click can land on an object at all: inside this range but
/// outside the 50 window it is a miss.
pub const HITTABLE_RANGE: Time = 400 * MS;

/// Centre of the playfield, which spinners turn around.
pub const CENTRE: (Fx, Fx) = (256 * FX_ONE, 192 * FX_ONE);

/// Object scale for a circle size: 1.0 is a 64 px radius.
pub fn object_scale(cs: f32) -> f32 {
    (1.0 - 0.7 * (cs - 5.0) / 5.0) / 2.0
}

/// Assembles a [`Chart`]. Objects may be added in any order.
pub struct ChartBuilder {
    difficulty: Difficulty,
    /// What the score's difficulty multiplier is computed from: the map's
    /// own values, also when mods change `difficulty`.
    score_difficulty: Difficulty,
    breaks_ms: Vec<(i64, i64)>,
    objects: Vec<HitObject>,
    sliders: Vec<Slider>,
    path: Vec<PathPoint>,
    nested: Vec<Nested>,
}

impl ChartBuilder {
    pub fn new(difficulty: Difficulty) -> Self {
        ChartBuilder {
            difficulty,
            score_difficulty: difficulty,
            breaks_ms: Vec::new(),
            objects: Vec::new(),
            sliders: Vec::new(),
            path: Vec::new(),
            nested: Vec::new(),
        }
    }

    /// The unmodded difficulty, for the score multiplier.
    pub fn score_difficulty(&mut self, d: Difficulty) -> &mut Self {
        self.score_difficulty = d;
        self
    }

    /// Break periods, needed for the drain time behind the score multiplier.
    pub fn breaks(&mut self, breaks_ms: &[(i64, i64)]) -> &mut Self {
        self.breaks_ms = breaks_ms.to_vec();
        self
    }

    fn push(&mut self, t: Time, end: Time, x: Fx, y: Fx, kind: ObjectKind, extra: u32) -> &mut HitObject {
        self.objects.push(HitObject { t, end, x, y, kind, combo_number: 1, combo_index: 0, extra });
        self.objects.last_mut().unwrap()
    }

    /// Returns the object so the caller can set its combo fields.
    pub fn circle(&mut self, t: Time, x: Fx, y: Fx) -> &mut HitObject {
        self.push(t, t, x, y, ObjectKind::Circle, 0)
    }

    /// `points` is the path as a polyline starting at the slider's head.
    /// `nested` lists the ticks, repeats and the tail check with their
    /// times; positions are filled in from the path.
    pub fn slider(
        &mut self,
        t: Time,
        points: &[(Fx, Fx)],
        spans: u32,
        span_duration: Time,
        nested: &[(Time, NestedKind)],
    ) -> &mut HitObject {
        let (spans, span_duration) = (spans.max(1), span_duration.max(1));
        let end = t + spans as Time * span_duration;
        let path_start = self.path.len() as u32;
        let mut d = 0i64;
        let head = points.first().copied().unwrap_or((0, 0));
        let mut prev = head;
        for (i, &(x, y)) in points.iter().enumerate() {
            if i > 0 {
                let (dx, dy) = ((x - prev.0) as i64, (y - prev.1) as i64);
                let step = ((dx * dx + dy * dy) as u64).isqrt() as i64;
                if step == 0 {
                    continue; // repeated vertex: nothing to travel
                }
                d += step;
            }
            self.path.push(PathPoint { x, y, d });
            prev = (x, y);
        }
        if self.path.len() as u32 == path_start {
            self.path.push(PathPoint { x: head.0, y: head.1, d: 0 });
        }
        let nested_start = self.nested.len() as u32;
        let mut sorted: Vec<_> = nested.iter().map(|&(nt, kind)| (nt.clamp(t, end), kind)).collect();
        sorted.sort_by_key(|n| n.0);
        self.nested.extend(sorted.into_iter().map(|(t, kind)| Nested { t, kind, x: 0, y: 0 }));
        self.sliders.push(Slider {
            path_start,
            path_len: self.path.len() as u32 - path_start,
            length: d,
            spans,
            span_duration,
            nested_start,
            nested_len: self.nested.len() as u32 - nested_start,
        });
        let index = self.sliders.len() as u32 - 1;
        self.push(t, end, head.0, head.1, ObjectKind::Slider, index)
    }

    pub fn spinner(&mut self, t: Time, end: Time) -> &mut HitObject {
        self.push(t, end.max(t), CENTRE.0, CENTRE.1, ObjectKind::Spinner, 0)
    }

    pub fn build(self) -> Chart {
        let ChartBuilder { difficulty: d, score_difficulty: sd, breaks_ms, mut objects, sliders, path, mut nested } = self;
        objects.sort_by_key(|o| o.t); // stable: equal times keep insertion order

        // stable compares integer milliseconds with `<` against the
        // truncated window, which in continuous time is "window - 0.5 ms".
        let window = |min, mid, max| difficulty_range(d.od, min, mid, max).floor() as Time * MS - MS / 2;

        // The 1.00041 factor is stable's playfield rounding allowance.
        let radius = to_fx(64.0 * object_scale(d.cs) * 1.00041);

        let preempt_ms = difficulty_range(d.ar, 1800.0, 1200.0, 450.0);
        let preempt = (preempt_ms as f64 * 1000.0).round() as Time;
        let fade_in = (400.0 * (preempt_ms as f64 / 450.0).min(1.0) * 1000.0).round() as Time;

        let score_multiplier = if objects.is_empty() {
            0
        } else {
            let ms = |t: Time| t.div_euclid(MS);
            let breaks: i64 = breaks_ms.iter().map(|(a, b)| b - a).sum();
            let drain_s = (ms(objects[objects.len() - 1].t) - ms(objects[0].t) - breaks) / 1000;
            // A zero drain time divides to infinity, which clamps to 16.
            let density =
                if drain_s > 0 { (objects.len() as f32 / drain_s as f32 * 8.0).clamp(0.0, 16.0) } else { 16.0 };
            round_half_even(((sd.hp + sd.od + sd.cs + density) / 38.0 * 5.0) as f64) as u32
        };

        // A spinner asks for a number of half turns proportional to its
        // length; how many per second depends on OD.
        let spins_per_second = difficulty_range(d.od, 3.0, 5.0, 7.5) as f64;
        for o in objects.iter_mut().filter(|o| o.kind == ObjectKind::Spinner) {
            o.extra = ((o.end - o.t) as f64 / 1e6 * spins_per_second) as u32;
        }

        let mut chart = Chart {
            objects,
            sliders,
            path,
            nested: Vec::new(),
            difficulty: d,
            radius,
            follow_radius: (radius as i64 * 24 / 10) as Fx,
            preempt,
            fade_in,
            window_300: window(80.0, 50.0, 20.0),
            window_100: window(140.0, 100.0, 60.0),
            window_50: window(200.0, 150.0, 100.0),
            score_multiplier,
            breaks: breaks_ms.iter().map(|&(a, b)| (a * MS, b * MS)).collect(),
            drain_rate: 0,
        };
        // The ball position at each nested point comes from the same
        // integer function the game uses while playing.
        for o in chart.objects.iter().filter(|o| o.kind == ObjectKind::Slider) {
            let s = chart.sliders[o.extra as usize];
            for n in &mut nested[s.nested_start as usize..(s.nested_start + s.nested_len) as usize] {
                (n.x, n.y) = chart.ball_at(o, n.t);
            }
        }
        chart.nested = nested;
        chart.drain_rate = crate::hp::drain_rate(&chart);
        chart
    }
}

impl Chart {
    pub fn slider(&self, o: &HitObject) -> &Slider {
        &self.sliders[o.extra as usize]
    }

    pub fn slider_path(&self, s: &Slider) -> &[PathPoint] {
        &self.path[s.path_start as usize..(s.path_start + s.path_len) as usize]
    }

    pub fn slider_nested(&self, s: &Slider) -> &[Nested] {
        &self.nested[s.nested_start as usize..(s.nested_start + s.nested_len) as usize]
    }

    /// Point at distance `d` (in [`Fx`] units) along a slider's path.
    pub fn position_at(&self, s: &Slider, d: i64) -> (Fx, Fx) {
        let path = self.slider_path(s);
        let d = d.clamp(0, s.length);
        // First vertex at or beyond d; d lies on the segment ending there.
        let i = path.partition_point(|p| p.d < d);
        if i == 0 {
            return (path[0].x, path[0].y);
        }
        let Some(b) = path.get(i) else {
            let last = path[path.len() - 1];
            return (last.x, last.y);
        };
        let a = path[i - 1];
        let (num, den) = (d - a.d, b.d - a.d);
        (a.x + ((b.x - a.x) as i64 * num / den) as Fx, a.y + ((b.y - a.y) as i64 * num / den) as Fx)
    }

    /// Where a slider's ball is at time `t` (clamped to the slider's life).
    pub fn ball_at(&self, o: &HitObject, t: Time) -> (Fx, Fx) {
        let s = self.slider(o);
        let rel = (t - o.t).clamp(0, o.end - o.t);
        let span = (rel / s.span_duration).min(s.spans as Time - 1);
        let into = rel - span * s.span_duration;
        let d = (s.length as i128 * into as i128 / s.span_duration as i128) as i64;
        // Odd passes run back towards the head.
        self.position_at(s, if span % 2 == 1 { s.length - d } else { d })
    }

    /// Where the slider's path ends up (the head again for an even number of passes).
    pub fn slider_end_position(&self, o: &HitObject) -> (Fx, Fx) {
        self.ball_at(o, o.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(cs: f32, od: f32, ar: f32) -> Chart {
        ChartBuilder::new(Difficulty { cs, od, ar, hp: 5.0 }).build()
    }

    #[test]
    fn hit_windows_match_stable() {
        // OD 10: 20/60/100 ms truncated, minus half a millisecond.
        let c = chart(4.0, 10.0, 9.0);
        assert_eq!((c.window_300, c.window_100, c.window_50), (19_500, 59_500, 99_500));
        // OD 8.5: 80 - 6*8.5 = 29, 140 - 8*8.5 = 72, 200 - 10*8.5 = 115.
        let c = chart(4.0, 8.5, 9.0);
        assert_eq!((c.window_300, c.window_100, c.window_50), (28_500, 71_500, 114_500));
        // Fractional results truncate: OD 7.3 gives 36.2 -> 36.
        assert_eq!(chart(4.0, 7.3, 9.0).window_300, 35_500);
        let c = chart(4.0, 0.0, 9.0);
        assert_eq!((c.window_300, c.window_100, c.window_50), (79_500, 139_500, 199_500));
    }

    #[test]
    fn approach_time_follows_ar() {
        assert_eq!(chart(4.0, 5.0, 0.0).preempt, 1_800_000);
        assert_eq!(chart(4.0, 5.0, 5.0).preempt, 1_200_000);
        assert_eq!(chart(4.0, 5.0, 9.0).preempt, 600_000);
        assert_eq!(chart(4.0, 5.0, 10.0).preempt, 450_000);
        assert_eq!(chart(4.0, 5.0, 10.0).fade_in, 400_000);
        assert_eq!(chart(4.0, 5.0, 5.0).fade_in, 400_000);
    }

    #[test]
    fn radius_follows_cs() {
        // CS 4: 64 * (1 - 0.7 * -0.2) / 2 = 36.48 osu! pixels.
        let c = chart(4.0, 5.0, 5.0);
        let r = crate::from_fx(c.radius);
        assert!((r - 36.48 * 1.00041).abs() < 0.01, "{r}");
        assert!((crate::from_fx(c.follow_radius) - r * 2.4).abs() < 0.01);
        let r = crate::from_fx(chart(7.0, 5.0, 5.0).radius);
        assert!((r - 23.04 * 1.00041).abs() < 0.01, "{r}");
    }

    #[test]
    fn score_multiplier_uses_drain_time_and_density() {
        let d = Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 6.0 };
        let build = |times: &mut dyn Iterator<Item = i64>, breaks: &[(i64, i64)]| {
            let mut b = ChartBuilder::new(d);
            b.breaks(breaks);
            for ms in times {
                b.circle(ms * MS, 0, 0);
            }
            b.build().score_multiplier
        };
        // 201 objects over 100 s: density 201/100*8 = 16.08 -> 16.
        // (6 + 8 + 4 + 16) / 38 * 5 = 4.47 -> 4.
        assert_eq!(build(&mut (0..=200).map(|i| i * 500), &[]), 4);
        // 50 s of breaks halve the drain time; density is already capped.
        assert_eq!(build(&mut (0..=200).map(|i| i * 500), &[(10_000, 60_000)]), 4);
        // Sparse: 11 objects over 100 s: density 0.88; 18.88/38*5 = 2.48 -> 2.
        assert_eq!(build(&mut (0..=10).map(|i| i * 10_000), &[]), 2);
    }

    #[test]
    fn rounding_is_half_to_even() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(3.5), 4.0);
        assert_eq!(round_half_even(2.4), 2.0);
        assert_eq!(round_half_even(2.6), 3.0);
    }

    fn px(x: f32, y: f32) -> (Fx, Fx) {
        (to_fx(x), to_fx(y))
    }

    /// An L-shaped slider: 100 px right, then 100 px down; 3 passes of 1 s.
    fn l_slider() -> Chart {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 5.0, hp: 5.0 });
        b.slider(
            1_000_000,
            &[px(100.0, 100.0), px(200.0, 100.0), px(200.0, 100.0), px(200.0, 200.0)],
            3,
            1_000_000,
            &[(1_500_000, NestedKind::Tick), (2_000_000, NestedKind::Repeat), (9_999_999, NestedKind::Tail)],
        );
        b.build()
    }

    #[test]
    fn the_ball_travels_the_path_and_turns_around() {
        let c = l_slider();
        let o = c.objects[0];
        assert_eq!(o.end, 4_000_000);
        assert_eq!(c.slider(&o).length, 200 * FX_ONE as i64);
        assert_eq!(c.slider(&o).path_len, 3, "the repeated vertex is dropped");
        assert_eq!(c.ball_at(&o, 0), px(100.0, 100.0), "before the start: at the head");
        assert_eq!(c.ball_at(&o, 1_250_000), px(150.0, 100.0));
        assert_eq!(c.ball_at(&o, 1_500_000), px(200.0, 100.0), "the corner");
        assert_eq!(c.ball_at(&o, 1_750_000), px(200.0, 150.0));
        assert_eq!(c.ball_at(&o, 2_000_000), px(200.0, 200.0), "end of the first pass");
        assert_eq!(c.ball_at(&o, 2_250_000), px(200.0, 150.0), "coming back");
        assert_eq!(c.ball_at(&o, 3_000_000), px(100.0, 100.0));
        assert_eq!(c.ball_at(&o, 3_500_000), px(200.0, 100.0), "third pass, forwards again");
        assert_eq!(c.ball_at(&o, 99_000_000), px(200.0, 200.0), "after the end: at the end");
        assert_eq!(c.slider_end_position(&o), px(200.0, 200.0));
    }

    #[test]
    fn nested_points_get_the_ball_position_and_are_clamped_to_the_slider() {
        let c = l_slider();
        let n = c.slider_nested(c.slider(&c.objects[0]));
        assert_eq!((n[0].x, n[0].y), px(200.0, 100.0));
        assert_eq!((n[1].x, n[1].y), px(200.0, 200.0));
        assert_eq!(n[2].t, 4_000_000, "a tail time past the end is pulled back to it");
    }

    #[test]
    fn spinners_ask_for_half_turns_by_length_and_od() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 5.0, hp: 5.0 });
        b.spinner(0, 2_000_000);
        // OD 5: 5 half turns per second.
        assert_eq!(b.build().objects[0].extra, 10);
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 10.0, ar: 5.0, hp: 5.0 });
        b.spinner(0, 1_500_000);
        assert_eq!(b.build().objects[0].extra, 11); // 1.5 * 7.5 = 11.25
    }

    #[test]
    fn a_degenerate_slider_is_a_point() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 5.0, hp: 5.0 });
        b.slider(1000, &[px(5.0, 5.0), px(5.0, 5.0)], 0, 0, &[]);
        let c = b.build();
        let o = c.objects[0];
        assert_eq!(c.ball_at(&o, 1000), px(5.0, 5.0));
        assert_eq!(c.ball_at(&o, o.end), px(5.0, 5.0));
        assert_eq!(o.end, 1001);
    }
}
