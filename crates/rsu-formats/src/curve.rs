//! Slider geometry: control points to a polyline of a given length.
//!
//! The algorithms and tolerances are the ones osu! uses today (adaptive
//! Bezier flattening, circular arcs through three points, Catmull-Rom for
//! very old maps), so a slider has the shape and length players expect.
//!
//! This is floating-point code, but it runs once at load time and uses only
//! IEEE-exact operations plus the `libm` crate, so the resulting path is the
//! same on every machine. The core receives it already quantised.

pub type P = [f64; 2];

fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1]]
}
fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}
fn mul(a: P, k: f64) -> P {
    [a[0] * k, a[1] * k]
}
fn len_sq(a: P) -> f64 {
    a[0] * a[0] + a[1] * a[1]
}
pub fn dist(a: P, b: P) -> f64 {
    len_sq(sub(a, b)).sqrt()
}

const BEZIER_TOLERANCE: f64 = 0.25;
const ARC_TOLERANCE: f64 = 0.1;
const CATMULL_DETAIL: usize = 50;

fn bezier_is_flat(cp: &[P]) -> bool {
    cp.windows(3).all(|w| {
        let second = add(sub(w[0], mul(w[1], 2.0)), w[2]);
        len_sq(second) <= BEZIER_TOLERANCE * BEZIER_TOLERANCE * 4.0
    })
}

/// De Casteljau split at t = 0.5 into `left` and `right` (each `n` points).
fn bezier_subdivide(cp: &[P], left: &mut [P], right: &mut [P], scratch: &mut [P]) {
    let n = cp.len();
    scratch[..n].copy_from_slice(cp);
    for i in 0..n {
        left[i] = scratch[0];
        right[n - i - 1] = scratch[n - i - 1];
        for j in 0..n - i - 1 {
            scratch[j] = mul(add(scratch[j], scratch[j + 1]), 0.5);
        }
    }
}

/// Emit a flat-enough curve: one more subdivision, then a smoothing filter
/// over the joined halves.
fn bezier_approximate(cp: &[P], out: &mut Vec<P>, joined: &mut [P], right: &mut [P], scratch: &mut [P]) {
    let n = cp.len();
    let (left, _) = joined.split_at_mut(n);
    bezier_subdivide(cp, left, right, scratch);
    for i in 0..n - 1 {
        joined[n + i] = right[i + 1];
    }
    out.push(cp[0]);
    for i in 1..n - 1 {
        let k = 2 * i;
        out.push(mul(add(add(joined[k - 1], mul(joined[k], 2.0)), joined[k + 1]), 0.25));
    }
}

pub fn bezier(cp: &[P], out: &mut Vec<P>) {
    let n = cp.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        out.push(cp[0]);
        return;
    }
    let mut stack: Vec<Vec<P>> = vec![cp.to_vec()];
    let mut joined = vec![[0.0; 2]; 2 * n - 1];
    let mut right = vec![[0.0; 2]; n];
    let mut scratch = vec![[0.0; 2]; n];
    let mut left = vec![[0.0; 2]; n];
    while let Some(parent) = stack.pop() {
        if bezier_is_flat(&parent) {
            bezier_approximate(&parent, out, &mut joined, &mut right, &mut scratch);
            continue;
        }
        bezier_subdivide(&parent, &mut left, &mut right, &mut scratch);
        // The left half is processed first, so it goes on the stack last.
        stack.push(right.clone());
        stack.push(left.clone());
    }
    out.push(cp[n - 1]);
}

/// Arc through three points. Returns false (and emits nothing) when they
/// are collinear, in which case the caller falls back to a Bezier.
pub fn circular_arc(a: P, b: P, c: P, out: &mut Vec<P>) -> bool {
    let cross = (b[1] - a[1]) * (c[0] - a[0]) - (b[0] - a[0]) * (c[1] - a[1]);
    if cross.abs() < 1e-3 {
        return false;
    }
    let d = 2.0 * (a[0] * (b[1] - c[1]) + b[0] * (c[1] - a[1]) + c[0] * (a[1] - b[1]));
    let (a2, b2, c2) = (len_sq(a), len_sq(b), len_sq(c));
    let centre = [
        (a2 * (b[1] - c[1]) + b2 * (c[1] - a[1]) + c2 * (a[1] - b[1])) / d,
        (a2 * (c[0] - b[0]) + b2 * (a[0] - c[0]) + c2 * (b[0] - a[0])) / d,
    ];
    let (da, dc) = (sub(a, centre), sub(c, centre));
    let r = len_sq(da).sqrt();
    let theta_start = libm::atan2(da[1], da[0]);
    let mut theta_end = libm::atan2(dc[1], dc[0]);
    while theta_end < theta_start {
        theta_end += std::f64::consts::TAU;
    }
    let mut dir = 1.0;
    let mut range = theta_end - theta_start;
    // Which way round: the side of the chord AC that B lies on decides.
    let ortho = [c[1] - a[1], -(c[0] - a[0])];
    let ab = sub(b, a);
    if ortho[0] * ab[0] + ortho[1] * ab[1] < 0.0 {
        dir = -1.0;
        range = std::f64::consts::TAU - range;
    }
    let points = if 2.0 * r <= ARC_TOLERANCE {
        2
    } else {
        ((range / (2.0 * libm::acos(1.0 - ARC_TOLERANCE / r))).ceil() as usize).clamp(2, 100_000)
    };
    for i in 0..points {
        let theta = theta_start + dir * (i as f64 / (points - 1) as f64) * range;
        out.push(add(centre, [libm::cos(theta) * r, libm::sin(theta) * r]));
    }
    true
}

pub fn catmull(cp: &[P], out: &mut Vec<P>) {
    let n = cp.len();
    let point = |v1: P, v2: P, v3: P, v4: P, t: f64| -> P {
        let (t2, t3) = (t * t, t * t * t);
        let f = |i: usize| {
            0.5 * (2.0 * v2[i]
                + (-v1[i] + v3[i]) * t
                + (2.0 * v1[i] - 5.0 * v2[i] + 4.0 * v3[i] - v4[i]) * t2
                + (-v1[i] + 3.0 * v2[i] - 3.0 * v3[i] + v4[i]) * t3)
        };
        [f(0), f(1)]
    };
    for i in 0..n.saturating_sub(1) {
        let v1 = if i > 0 { cp[i - 1] } else { cp[i] };
        let v2 = cp[i];
        let v3 = if i + 1 < n { cp[i + 1] } else { sub(add(v2, v2), v1) };
        let v4 = if i + 2 < n { cp[i + 2] } else { sub(add(v3, v3), v2) };
        for c in 0..CATMULL_DETAIL {
            out.push(point(v1, v2, v3, v4, c as f64 / CATMULL_DETAIL as f64));
            out.push(point(v1, v2, v3, v4, (c + 1) as f64 / CATMULL_DETAIL as f64));
        }
    }
}

/// A slider path: points and the distance along the path at each of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub points: Vec<P>,
    pub cumulative: Vec<f64>,
}

impl Path {
    pub fn length(&self) -> f64 {
        self.cumulative.last().copied().unwrap_or(0.0)
    }

    pub fn end(&self) -> P {
        self.points.last().copied().unwrap_or([0.0; 2])
    }
}

/// Build the path for a slider. `control` includes the slider's own
/// position as its first point. `expected_length` is the length stored in
/// the beatmap (0 = use whatever the curve measures): the curve is cut or
/// extended in a straight line to match it, as the game does.
pub fn build(curve_type: char, control: &[P], expected_length: f64) -> Path {
    let mut points = Vec::new();
    match curve_type {
        'L' => points.extend_from_slice(control),
        'P' if control.len() == 3 => {
            if !circular_arc(control[0], control[1], control[2], &mut points) {
                bezier(control, &mut points);
            }
        }
        'C' => catmull(control, &mut points),
        _ => {
            // Bezier ('B', and 'P' with the wrong number of points). A
            // repeated control point ends one curve and starts the next.
            let mut start = 0;
            for i in 0..control.len() {
                let last = i + 1 == control.len();
                if last || control[i] == control[i + 1] {
                    bezier(&control[start..=i], &mut points);
                    start = i + 1;
                }
            }
        }
    }
    if points.is_empty() {
        points.push(control.first().copied().unwrap_or([0.0; 2]));
    }

    let mut cumulative = Vec::with_capacity(points.len());
    let mut total = 0.0;
    cumulative.push(0.0);
    for w in points.windows(2) {
        total += dist(w[0], w[1]);
        cumulative.push(total);
    }

    if expected_length > 0.0 && total != expected_length && points.len() >= 2 {
        let n = points.len();
        // A path that ends in a repeated point is never extended.
        if !(points[n - 1] == points[n - 2] && expected_length > total) {
            cumulative.pop();
            let mut end = n - 1;
            if total > expected_length {
                while cumulative.last().is_some_and(|&c| c >= expected_length) {
                    cumulative.pop();
                    points.remove(end);
                    end -= 1;
                }
            }
            if end == 0 || cumulative.is_empty() {
                points.truncate(1);
                return Path { points, cumulative: vec![0.0] };
            }
            let d = sub(points[end], points[end - 1]);
            let l = len_sq(d).sqrt();
            let dir = if l > 0.0 { mul(d, 1.0 / l) } else { [0.0, 0.0] };
            let base = *cumulative.last().unwrap();
            points[end] = add(points[end - 1], mul(dir, expected_length - base));
            cumulative.push(expected_length);
        }
    }
    Path { points, cumulative }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: P, b: P, eps: f64) -> bool {
        dist(a, b) < eps
    }

    #[test]
    fn linear_path_is_cut_to_the_stored_length() {
        let p = build('L', &[[0.0, 0.0], [100.0, 0.0], [100.0, 100.0]], 150.0);
        assert_eq!(p.length(), 150.0);
        assert!(near(p.end(), [100.0, 50.0], 1e-9));
        assert_eq!(p.cumulative, vec![0.0, 100.0, 150.0]);
    }

    #[test]
    fn a_short_path_is_extended_in_a_straight_line() {
        let p = build('L', &[[0.0, 0.0], [30.0, 40.0]], 100.0);
        assert!(near(p.end(), [60.0, 80.0], 1e-9));
        assert_eq!(p.length(), 100.0);
    }

    #[test]
    fn cutting_can_remove_whole_segments() {
        let p = build('L', &[[0.0, 0.0], [10.0, 0.0], [20.0, 0.0], [30.0, 0.0]], 5.0);
        assert_eq!(p.points.len(), 2);
        assert!(near(p.end(), [5.0, 0.0], 1e-9));
    }

    #[test]
    fn zero_stored_length_keeps_the_measured_one() {
        let p = build('L', &[[0.0, 0.0], [30.0, 40.0]], 0.0);
        assert_eq!(p.length(), 50.0);
    }

    #[test]
    fn quadratic_bezier_stays_within_tolerance_of_the_true_curve() {
        let cp = [[0.0, 0.0], [100.0, 200.0], [200.0, 0.0]];
        let p = build('B', &cp, 0.0);
        assert!(p.points.len() > 8, "curved enough to need subdividing");
        assert!(near(p.points[0], cp[0], 1e-9) && near(p.end(), cp[2], 1e-9));
        // Every vertex must lie close to the analytic curve B(t).
        for q in &p.points {
            let best = (0..=2000)
                .map(|i| {
                    let t = i as f64 / 2000.0;
                    let u = 1.0 - t;
                    let b = [
                        u * u * cp[0][0] + 2.0 * u * t * cp[1][0] + t * t * cp[2][0],
                        u * u * cp[0][1] + 2.0 * u * t * cp[1][1] + t * t * cp[2][1],
                    ];
                    dist(*q, b)
                })
                .fold(f64::MAX, f64::min);
            assert!(best < 0.3, "vertex {q:?} is {best} px off the curve");
        }
        // Arc length of this parabola is about 295.8.
        assert!((p.length() - 295.8).abs() < 0.5, "{}", p.length());
    }

    #[test]
    fn a_repeated_point_makes_a_sharp_corner() {
        let cp = [[0.0, 0.0], [100.0, 0.0], [100.0, 0.0], [100.0, 100.0]];
        let p = build('B', &cp, 0.0);
        assert!((p.length() - 200.0).abs() < 1e-9);
        assert!(p.points.iter().any(|q| near(*q, [100.0, 0.0], 1e-9)));
    }

    #[test]
    fn perfect_circle_is_a_circle_through_all_three_points() {
        // Half circle of radius 50 around (50, 0), bulging towards -y.
        let p = build('P', &[[0.0, 0.0], [50.0, -50.0], [100.0, 0.0]], 0.0);
        for q in &p.points {
            assert!((dist(*q, [50.0, 0.0]) - 50.0).abs() < 1e-9);
            assert!(q[1] <= 1e-9, "arc went the wrong way round: {q:?}");
        }
        // An inscribed polygon is slightly shorter than the arc it follows.
        let arc = std::f64::consts::PI * 50.0;
        assert!(p.length() <= arc && arc - p.length() < 0.2, "{}", p.length());
        // The other side.
        let p = build('P', &[[0.0, 0.0], [50.0, 50.0], [100.0, 0.0]], 0.0);
        assert!(p.points.iter().all(|q| q[1] >= -1e-9));
    }

    #[test]
    fn collinear_perfect_curve_degrades_to_a_line() {
        let p = build('P', &[[0.0, 0.0], [50.0, 0.0], [100.0, 0.0]], 0.0);
        assert!((p.length() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn catmull_passes_through_its_control_points() {
        let cp = [[0.0, 0.0], [50.0, 80.0], [120.0, 20.0]];
        let p = build('C', &cp, 0.0);
        for c in cp {
            assert!(p.points.iter().any(|q| near(*q, c, 1e-9)), "{c:?} not on the path");
        }
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        assert_eq!(build('B', &[[5.0, 5.0]], 100.0).points.len(), 1);
        assert_eq!(build('B', &[], 100.0).length(), 0.0);
        let same = build('L', &[[5.0, 5.0], [5.0, 5.0]], 100.0);
        assert!(same.points.iter().all(|q| q[0].is_finite() && q[1].is_finite()));
        let p = build('P', &[[0.0, 0.0], [0.0, 0.0], [0.0, 0.0]], 50.0);
        assert!(p.points.iter().all(|q| q[0].is_finite()));
    }
}
