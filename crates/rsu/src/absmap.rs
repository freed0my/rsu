//! Mapping raw absolute device coordinates to window pixels.
//!
//! evdev gives positions in device units; where those land on screen is the
//! compositor's business (which output the tablet is bound to, where our
//! window sits) and cannot be queried portably. Instead of guessing, the
//! mapping is *learned*: every pointer event the window system delivers is
//! paired with the evdev sample that caused it, and a line is fitted per
//! axis. Until the fit is good the window-system position is used as is, so
//! the cursor is always correct; once it converges (about a second of
//! movement) the game switches to the direct evdev path, which is earlier
//! and carries kernel timestamps. This works the same with a kernel tablet
//! driver, OpenTabletDriver or anything else that produces an absolute
//! pointer, on any monitor layout.

use rsu_platform::evdev::{CursorSample, DeviceInfo};

#[derive(Clone, Copy, Default)]
struct AxisFit {
    n: f64,
    sx: f64,
    sp: f64,
    sxx: f64,
    sxp: f64,
    spp: f64,
}

impl AxisFit {
    fn add(&mut self, x: f64, p: f64) {
        self.n += 1.0;
        self.sx += x;
        self.sp += p;
        self.sxx += x * x;
        self.sxp += x * p;
        self.spp += p * p;
    }

    /// Halve the weight of history so a changed mapping can be relearned.
    fn decay(&mut self) {
        for v in [&mut self.n, &mut self.sx, &mut self.sp, &mut self.sxx, &mut self.sxp, &mut self.spp] {
            *v *= 0.5;
        }
    }

    /// Least squares `p = a*x + b`: (a, b, rms residual, std-dev of p).
    fn solve(&self) -> Option<(f64, f64, f64, f64)> {
        if self.n < 2.0 {
            return None;
        }
        let (mx, mp) = (self.sx / self.n, self.sp / self.n);
        let var_x = self.sxx / self.n - mx * mx;
        let var_p = self.spp / self.n - mp * mp;
        let cov = self.sxp / self.n - mx * mp;
        if var_x <= 1e-9 {
            return None;
        }
        let a = cov / var_x;
        let rms = (var_p - a * cov).max(0.0).sqrt();
        Some((a, mp - a * mx, rms, var_p.max(0.0).sqrt()))
    }
}

/// What is known about a device before anything has been learned.
#[derive(Clone, Copy, Default)]
struct DevConfig {
    /// Movement (device units) below which the pointer counts as slow:
    /// about one pixel on a typical screen.
    slow: (i32, i32),
    /// The mapping to try first: the device stretched over our fullscreen
    /// window, which is right whenever the tablet is bound to this output.
    trial: Option<([f64; 2], [f64; 2])>,
}

#[derive(Clone, Copy, Default)]
struct DevFit {
    fx: AxisFit,
    fy: AxisFit,
    a: [f64; 2],
    b: [f64; 2],
    valid: bool,
    rms: f32,
    /// Consecutive window-system events that disagreed with the fit.
    bad: u32,
    trial_hits: u32,
    trial_misses: u32,
}

impl DevFit {
    /// Pairs needed, and how far they must be spread out (std-dev, pixels)
    /// on both axes, before a least-squares fit is trusted.
    const MIN_PAIRS: f64 = 24.0;
    const MIN_SPREAD_PX: f64 = 40.0;
    const MAX_RMS_PX: f64 = 1.0;

    fn add(&mut self, s: &CursorSample, px: f32, py: f32) {
        self.fx.add(s.x as f64, px as f64);
        self.fy.add(s.y as f64, py as f64);
        if self.fx.n > 4000.0 {
            self.fx.decay();
            self.fy.decay();
        }
        if self.fx.n < Self::MIN_PAIRS {
            return;
        }
        if let (Some((ax, bx, rx, sx)), Some((ay, by, ry, sy))) = (self.fx.solve(), self.fy.solve()) {
            let rms = rx.max(ry);
            if sx > Self::MIN_SPREAD_PX && sy > Self::MIN_SPREAD_PX && rms < Self::MAX_RMS_PX {
                self.a = [ax, ay];
                self.b = [bx, by];
                self.rms = rms as f32;
                self.valid = true;
            }
        }
    }

    fn map_with(a: &[f64; 2], b: &[f64; 2], s: &CursorSample) -> (f32, f32) {
        ((a[0] * s.x as f64 + b[0]) as f32, (a[1] * s.y as f64 + b[1]) as f32)
    }

    fn map(&self, s: &CursorSample) -> (f32, f32) {
        Self::map_with(&self.a, &self.b, s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorSource {
    /// Nothing has moved yet.
    None,
    /// Window-system pointer events (compositor path).
    Window,
    /// evdev through the learned mapping.
    EvdevFit,
    /// evdev stretched over a fullscreen window, when the window system
    /// delivers no pointer events to learn from.
    EvdevFullscreen,
}

impl CursorSource {
    pub fn name(self) -> &'static str {
        match self {
            CursorSource::None => "NONE",
            CursorSource::Window => "WINDOW-SYSTEM",
            CursorSource::EvdevFit => "EVDEV (LEARNED MAP)",
            CursorSource::EvdevFullscreen => "EVDEV (FULLSCREEN MAP)",
        }
    }
    pub fn is_evdev(self) -> bool {
        matches!(self, CursorSource::EvdevFit | CursorSource::EvdevFullscreen)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub x: f32,
    pub y: f32,
    /// Timestamp of the sample: kernel time for evdev, arrival time for
    /// window-system events. 0 if there is none.
    pub t_ns: u64,
    pub source: CursorSource,
}

const MAX_DEVICES: usize = 32;
const HISTORY: usize = 128;
/// How close a mapped sample must land to a window-system event to count
/// as its cause.
const MATCH_TOLERANCE_PX: f32 = 3.0;
const TRIAL_TOLERANCE_PX: f32 = 2.5;
/// Only pairs this close refine a mapping that is already in use: a pair
/// that merely "matches" may be an event paired with a neighbouring sample,
/// and learning from those would smear the fit.
const REFINE_TOLERANCE_PX: f32 = 1.0;
/// Events the fullscreen guess must explain before it is adopted.
const TRIAL_HITS: u32 = 24;
const MAX_CANDIDATES: usize = 32;

pub struct AbsMap {
    config: [DevConfig; MAX_DEVICES],
    fits: [DevFit; MAX_DEVICES],
    history: [CursorSample; HISTORY],
    next: usize,
    win_pos: (f32, f32),
    win_ns: u64,
    /// Arrival time of the last window-system motion no evdev sample
    /// explains, i.e. the pointer was moved by a device we do not read.
    win_unexplained_ns: u64,
    /// A window-system event nothing explained *yet*: its cause may be a
    /// sample that was read from the kernel in time but handed to us late.
    pending: Option<Pending>,
}

#[derive(Clone, Copy)]
struct Pending {
    x: f32,
    y: f32,
    arrival_ns: u64,
    dev: usize,
    /// What `win_unexplained_ns` was before this event claimed it.
    previous_unexplained_ns: u64,
}

impl AbsMap {
    pub fn new() -> Self {
        AbsMap {
            config: [DevConfig::default(); MAX_DEVICES],
            fits: [DevFit::default(); MAX_DEVICES],
            history: [CursorSample::default(); HISTORY],
            next: 0,
            win_pos: (0.0, 0.0),
            win_ns: 0,
            win_unexplained_ns: 0,
            pending: None,
        }
    }

    /// Describe the devices and the window. Call at start and whenever the
    /// window changes size: everything learned so far is discarded, because
    /// a changed window means a changed mapping.
    pub fn configure(&mut self, devices: &[DeviceInfo], fullscreen_size: Option<(u32, u32)>) {
        self.fits = [DevFit::default(); MAX_DEVICES];
        for (cfg, d) in self.config.iter_mut().zip(devices) {
            let (rx, ry) = ((d.x_range.1 - d.x_range.0) as f64, (d.y_range.1 - d.y_range.0) as f64);
            cfg.slow = (((rx / 2048.0) as i32).max(2), ((ry / 2048.0) as i32).max(2));
            cfg.trial = fullscreen_size.filter(|_| rx > 0.0 && ry > 0.0).map(|(w, h)| {
                let a = [w as f64 / rx, h as f64 / ry];
                (a, [-(d.x_range.0 as f64) * a[0], -(d.y_range.0 as f64) * a[1]])
            });
        }
    }

    pub fn fit_rms(&self, dev: u8) -> Option<f32> {
        let f = &self.fits[dev as usize % MAX_DEVICES];
        f.valid.then_some(f.rms)
    }

    /// One line per device that has seen events, for the exit summary: if
    /// the mapping does not converge on someone's machine, this says how
    /// far it got.
    pub fn describe(&self, devices: &[DeviceInfo]) -> Vec<String> {
        devices
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let f = &self.fits[i % MAX_DEVICES];
                if f.fx.n == 0.0 && f.trial_hits + f.trial_misses == 0 {
                    return None;
                }
                let residual = match (f.fx.solve(), f.fy.solve()) {
                    (Some(x), Some(y)) => format!("residual {:.2} px, spread {:.0}x{:.0} px", x.2.max(y.2), x.3, y.3),
                    _ => "no solution yet".into(),
                };
                Some(format!(
                    "{}: {}; fullscreen guess matched {} of {} events; {} pairs, {}",
                    d.name,
                    if f.valid { "learned" } else { "NOT learned" },
                    f.trial_hits,
                    f.trial_hits + f.trial_misses,
                    f.fx.n as u64,
                    residual
                ))
            })
            .collect()
    }

    /// Record an evdev sample drained from the input queue.
    pub fn on_sample(&mut self, s: CursorSample) {
        self.history[self.next] = s;
        self.next = (self.next + 1) % HISTORY;

        // Does this sample explain a window event that came in before it?
        // Only a sample older than the event can have caused it.
        let Some(p) = self.pending else { return };
        if s.t_ns > p.arrival_ns {
            self.pending = None;
            return;
        }
        let fit = &mut self.fits[p.dev];
        if s.dev as usize % MAX_DEVICES == p.dev && fit.valid {
            let (px, py) = fit.map(&s);
            let distance = (px - p.x).hypot(py - p.y);
            if distance < MATCH_TOLERANCE_PX {
                if distance < REFINE_TOLERANCE_PX {
                    fit.add(&s, p.x, p.y);
                }
                fit.bad = 0;
                if self.win_unexplained_ns == p.arrival_ns {
                    self.win_unexplained_ns = p.previous_unexplained_ns;
                }
                self.pending = None;
            }
        }
    }

    /// `i` = 0 is the newest recorded sample.
    fn recent(&self, i: usize) -> &CursorSample {
        &self.history[(self.next + HISTORY - 1 - i) % HISTORY]
    }

    /// The samples that could have caused a window-system event, newest
    /// first, all from one device.
    ///
    /// Compositors stamp pointer events either with the time of the input
    /// event that moved the pointer or with the time they processed it. In
    /// both cases the cause lies a few milliseconds before the stamp, so a
    /// short window before it is searched; which sample it actually was is
    /// decided by position, not by time.
    fn candidates(&self, time_ms: u32, arrival_ns: u64, out: &mut [CursorSample; MAX_CANDIDATES]) -> usize {
        let arrival_ms = (arrival_ns / 1_000_000) as u32;
        // Only trust the stamp if it is on our clock at all.
        let same_clock = (arrival_ms.wrapping_sub(time_ms) as i32).abs() < 100;
        let mut n = 0;
        for i in 0..HISTORY {
            let s = *self.recent(i);
            if s.t_ns == 0 {
                break;
            }
            if same_clock {
                let after = ((s.t_ns / 1_000_000) as u32).wrapping_sub(time_ms) as i32;
                if after < -6 {
                    break;
                }
                if after > 1 {
                    continue;
                }
            } else if arrival_ns.saturating_sub(s.t_ns) > 25_000_000 {
                break;
            }
            if n > 0 && out[0].dev != s.dev {
                continue;
            }
            if n < out.len() {
                out[n] = s;
                n += 1;
            }
        }
        // A slow compositor: fall back to the newest sample if it is recent.
        if n == 0 {
            let s = *self.recent(0);
            if s.t_ns != 0 && arrival_ns.saturating_sub(s.t_ns) < 25_000_000 {
                out[0] = s;
                n = 1;
            }
        }
        n
    }

    /// A pointer event from the window system, in surface pixels.
    pub fn on_window_motion(&mut self, x: f32, y: f32, time_ms: u32, arrival_ns: u64) {
        self.win_pos = (x, y);
        self.win_ns = arrival_ns;

        let mut buf = [CursorSample::default(); MAX_CANDIDATES];
        let n = self.candidates(time_ms, arrival_ns, &mut buf);
        let candidates = &buf[..n];
        let mut explained = false;
        let mut unmatched_on = None;
        if let Some(newest) = candidates.first() {
            let di = newest.dev as usize % MAX_DEVICES;
            let cfg = self.config[di];
            let fit = &mut self.fits[di];
            // The candidate a mapping puts closest to the event, and how close.
            let nearest = |a: &[f64; 2], b: &[f64; 2]| {
                candidates
                    .iter()
                    .map(|c| {
                        let (px, py) = DevFit::map_with(a, b, c);
                        ((px - x).hypot(py - y), *c)
                    })
                    .fold((f32::MAX, *newest), |best, cur| if cur.0 < best.0 { cur } else { best })
            };
            if fit.valid {
                let (distance, cause) = nearest(&fit.a, &fit.b);
                if distance < MATCH_TOLERANCE_PX {
                    if distance < REFINE_TOLERANCE_PX {
                        fit.add(&cause, x, y);
                    }
                    fit.bad = 0;
                    explained = true;
                } else {
                    unmatched_on = Some(di);
                    fit.bad += 1;
                    if fit.bad > 16 {
                        *fit = DevFit::default(); // the mapping changed: learn again
                    }
                }
            } else {
                // Shortcut: does the fullscreen guess explain what we see?
                if let Some((a, b)) = cfg.trial {
                    if nearest(&a, &b).0 < TRIAL_TOLERANCE_PX {
                        fit.trial_hits += 1;
                    } else {
                        fit.trial_misses += 1;
                    }
                    if fit.trial_hits >= TRIAL_HITS && fit.trial_hits >= 8 * fit.trial_misses {
                        (fit.a, fit.b) = (a, b);
                        fit.valid = true;
                        fit.rms = 0.0;
                    }
                }
                // General case: learn from moments when the pointer is slow
                // enough that every candidate is in practically one place,
                // so it does not matter which of them was the cause.
                let (lo, hi) = candidates.iter().fold(((i32::MAX, i32::MAX), (i32::MIN, i32::MIN)), |(lo, hi), c| {
                    ((lo.0.min(c.x), lo.1.min(c.y)), (hi.0.max(c.x), hi.1.max(c.y)))
                });
                if !fit.valid && hi.0 - lo.0 <= cfg.slow.0 && hi.1 - lo.1 <= cfg.slow.1 {
                    fit.add(newest, x, y);
                }
            }
        }
        if !explained {
            // Unexplained for now; `on_sample` may still find the cause.
            self.pending = unmatched_on.map(|dev| Pending {
                x,
                y,
                arrival_ns,
                dev,
                previous_unexplained_ns: self.win_unexplained_ns,
            });
            self.win_unexplained_ns = arrival_ns;
        }
    }

    /// The position to draw the cursor at, from the newest evdev sample
    /// (`latest`, read from the seqlock at latch time) where possible.
    pub fn cursor(
        &self,
        latest: Option<CursorSample>,
        devices: &[DeviceInfo],
        fullscreen_size: Option<(u32, u32)>,
        now_ns: u64,
    ) -> Cursor {
        if let Some(s) = latest.filter(|s| s.t_ns != 0) {
            let fit = &self.fits[s.dev as usize % MAX_DEVICES];
            if fit.valid && s.t_ns > self.win_unexplained_ns {
                let (x, y) = fit.map(&s);
                return Cursor { x, y, t_ns: s.t_ns, source: CursorSource::EvdevFit };
            }
            // No pointer events to learn from (some compositors route pens
            // only through the tablet protocol): assume the device is mapped
            // onto the output we cover.
            let silent = now_ns.saturating_sub(self.win_ns) > 500_000_000;
            let moving = now_ns.saturating_sub(s.t_ns) < 500_000_000;
            if let (false, true, true, Some((w, h)), Some(d)) =
                (fit.valid, silent, moving, fullscreen_size, devices.get(s.dev as usize))
            {
                let fx = (s.x - d.x_range.0) as f32 / (d.x_range.1 - d.x_range.0) as f32;
                let fy = (s.y - d.y_range.0) as f32 / (d.y_range.1 - d.y_range.0) as f32;
                return Cursor {
                    x: fx * w as f32,
                    y: fy * h as f32,
                    t_ns: s.t_ns,
                    source: CursorSource::EvdevFullscreen,
                };
            }
        }
        if self.win_ns != 0 {
            return Cursor { x: self.win_pos.0, y: self.win_pos.1, t_ns: self.win_ns, source: CursorSource::Window };
        }
        Cursor { x: -1000.0, y: -1000.0, t_ns: 0, source: CursorSource::None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsu_platform::evdev::DeviceKind;

    const DEV: u8 = 3;

    fn devices() -> Vec<DeviceInfo> {
        let d =
            DeviceInfo { name: String::new(), kind: DeviceKind::AbsPointer, x_range: (0, 15200), y_range: (0, 9500) };
        vec![d.clone(), d.clone(), d.clone(), d]
    }

    /// A compositor: maps the tablet onto a 1920x1080 output and reports
    /// positions relative to a window at `origin`.
    #[derive(Clone, Copy)]
    struct Compositor {
        origin: (f32, f32),
        /// Stamp events with the processing time instead of the input time.
        own_stamps: bool,
        /// How long after the sample the compositor processes it.
        lag_ns: u64,
    }

    impl Compositor {
        fn screen(&self, s: &CursorSample) -> (f32, f32) {
            (s.x as f32 * 1920.0 / 15200.0 - self.origin.0, s.y as f32 * 1080.0 / 9500.0 - self.origin.1)
        }
        fn stamp(&self, s: &CursorSample) -> u32 {
            ((s.t_ns + if self.own_stamps { self.lag_ns } else { 0 }) / 1_000_000) as u32
        }
    }

    /// Pen motion: sweeps over most of the tablet with pauses in between,
    /// sampled every `period_ns`.
    fn pen(i: u64, period_ns: u64) -> CursorSample {
        let t_s = (i * period_ns) as f64 / 1e9;
        // 0.6 s moving, 0.15 s resting, repeat; each sweep goes somewhere else.
        let phase = t_s % 0.75;
        let leg = (t_s / 0.75).floor();
        let spot = |k: f64| (2000.0 + (k * 4801.0) % 11000.0, 1200.0 + (k * 3307.0) % 7000.0);
        let (from, to) = (spot(leg), spot(leg + 1.0));
        let f = (phase / 0.6).min(1.0);
        let ease = f * f * (3.0 - 2.0 * f);
        CursorSample {
            t_ns: 1_000_000_000 + i * period_ns,
            x: (from.0 + (to.0 - from.0) * ease) as i32,
            y: (from.1 + (to.1 - from.1) * ease) as i32,
            dev: DEV,
        }
    }

    /// Feed `samples` pen samples through a frame loop: once per
    /// `frame_ns` the game handles the samples and the window events that
    /// have arrived (a window event arrives `lag_ns` after its sample).
    /// `events_first` handles a frame's window events before its samples,
    /// so an event is seen before the sample that caused it.
    /// Returns (sample index at which evdev first drove the cursor, frames
    /// that fell back to the window after that, worst cursor error in px).
    fn run_ordered(
        m: &mut AbsMap,
        c: Compositor,
        period_ns: u64,
        frame_ns: u64,
        samples: u64,
        events_first: bool,
    ) -> (Option<u64>, u32, f32) {
        let devs = devices();
        let (mut first, mut switches_back, mut worst) = (None, 0, 0.0f32);
        let mut was_evdev = false;
        let (mut next_sample, mut next_event) = (0u64, 0u64);
        let mut now = 1_000_000_000 + frame_ns;
        while next_sample < samples {
            let from = next_sample;
            while next_sample < samples && pen(next_sample, period_ns).t_ns <= now {
                next_sample += 1;
            }
            let feed_samples = |m: &mut AbsMap| (from..next_sample).for_each(|i| m.on_sample(pen(i, period_ns)));
            let mut feed_events = |m: &mut AbsMap| {
                while next_event < samples && pen(next_event, period_ns).t_ns + c.lag_ns <= now {
                    let p = pen(next_event, period_ns);
                    let (px, py) = c.screen(&p);
                    m.on_window_motion(px, py, c.stamp(&p), now);
                    next_event += 1;
                }
            };
            if events_first {
                feed_events(m);
                feed_samples(m);
            } else {
                feed_samples(m);
                feed_events(m);
            }
            if next_sample > from {
                let s = pen(next_sample - 1, period_ns);
                let cur = m.cursor(Some(s), &devs, None, now);
                if cur.source == CursorSource::EvdevFit {
                    first.get_or_insert(next_sample - 1);
                    let (px, py) = c.screen(&s);
                    worst = worst.max((cur.x - px).hypot(cur.y - py));
                } else if was_evdev {
                    switches_back += 1;
                }
                was_evdev = cur.source == CursorSource::EvdevFit;
            }
            now += frame_ns;
        }
        (first, switches_back, worst)
    }

    fn run(m: &mut AbsMap, c: Compositor, period_ns: u64, frame_ns: u64, samples: u64) -> (Option<u64>, u32, f32) {
        run_ordered(m, c, period_ns, frame_ns, samples, false)
    }

    const KHZ: u64 = 1_050_000; // the 1.05 ms report interval seen on real hardware
    const TABLET_133: u64 = 7_519_000;

    #[test]
    fn fullscreen_guess_is_adopted_at_once_even_at_1khz_with_late_stamps() {
        // The case that failed in the field: reports every millisecond and a
        // compositor that stamps pointer events when it processes them.
        let mut m = AbsMap::new();
        m.configure(&devices(), Some((1920, 1080)));
        let c = Compositor { origin: (0.0, 0.0), own_stamps: true, lag_ns: 700_000 };
        let (first, back, worst) = run(&mut m, c, KHZ, 1_000_000, 4000);
        assert!(first.is_some_and(|i| i < 200), "took {first:?} samples");
        assert_eq!(back, 0, "must not fall back once learned");
        assert!(worst < 1.0, "cursor was up to {worst} px off");
    }

    #[test]
    fn an_event_seen_before_its_sample_is_explained_once_the_sample_arrives() {
        // Found in the field: the frame loop looked at window events before
        // it had drained the samples that caused them, and most events
        // counted as "moved by something else".
        for (origin, lag_ns) in [((0.0, 0.0), 300_000), ((37.0, 12.0), 300_000), ((0.0, 0.0), 50_000)] {
            let mut m = AbsMap::new();
            m.configure(&devices(), Some((1920, 1080)));
            let c = Compositor { origin, own_stamps: true, lag_ns };
            let (first, back, worst) = run_ordered(&mut m, c, KHZ, 1_000_000, 8000, true);
            assert!(first.is_some(), "never learned (origin {origin:?})");
            assert_eq!(back, 0, "fell back to the window {back} times (origin {origin:?}, lag {lag_ns})");
            assert!(worst < 1.5, "cursor was up to {worst} px off (origin {origin:?}, lag {lag_ns})");
        }
    }

    #[test]
    fn learns_an_offset_window_from_slow_moments() {
        // Windowed (or a tablet mapped elsewhere): the guess is wrong and
        // the mapping has to be learned from the pauses in the motion.
        for (period, own_stamps) in [(KHZ, true), (KHZ, false), (TABLET_133, true), (TABLET_133, false)] {
            let mut m = AbsMap::new();
            m.configure(&devices(), Some((1920, 1080)));
            let c = Compositor { origin: (37.0, 12.0), own_stamps, lag_ns: 700_000 };
            let (first, back, worst) = run(&mut m, c, period, 1_000_000, 8_000_000_000 / period);
            assert!(first.is_some(), "never learned (period {period}, own stamps {own_stamps})");
            assert_eq!(back, 0, "period {period}, own stamps {own_stamps}");
            assert!(worst < 1.5, "cursor was up to {worst} px off (period {period}, own stamps {own_stamps})");
        }
    }

    #[test]
    fn works_at_low_frame_rates_too() {
        // 60 fps vsync: window events are seen up to 16 ms late.
        let mut m = AbsMap::new();
        m.configure(&devices(), None);
        let c = Compositor { origin: (100.0, 50.0), own_stamps: true, lag_ns: 700_000 };
        let (first, back, worst) = run(&mut m, c, KHZ, 16_666_667, 8000);
        assert!(first.is_some());
        assert_eq!(back, 0);
        assert!(worst < 1.5, "{worst}");
    }

    #[test]
    fn a_mouse_takes_the_cursor_back() {
        let mut m = AbsMap::new();
        m.configure(&devices(), Some((1920, 1080)));
        let c = Compositor { origin: (0.0, 0.0), own_stamps: true, lag_ns: 700_000 };
        run(&mut m, c, KHZ, 1_000_000, 1000);
        let last = pen(999, KHZ);
        assert_eq!(m.cursor(Some(last), &devices(), None, last.t_ns).source, CursorSource::EvdevFit);
        // A relative mouse moves the pointer: no evdev sample explains it.
        let later = last.t_ns + 50_000_000;
        m.on_window_motion(100.0, 100.0, (later / 1_000_000) as u32, later);
        let cur = m.cursor(Some(last), &devices(), None, later);
        assert_eq!(cur.source, CursorSource::Window);
        assert_eq!((cur.x, cur.y), (100.0, 100.0));
    }

    #[test]
    fn relearns_after_the_mapping_changes() {
        let mut m = AbsMap::new();
        m.configure(&devices(), Some((1920, 1080)));
        let before = Compositor { origin: (0.0, 0.0), own_stamps: false, lag_ns: 500_000 };
        run(&mut m, before, KHZ, 1_000_000, 1000);
        // The tablet is rebound: everything is 300 px off from now on. The
        // game is not told; it has to notice.
        let after = Compositor { origin: (300.0, 0.0), ..before };
        let (first, _, _) = run(&mut m, after, KHZ, 1_000_000, 8000);
        assert!(first.is_some(), "did not converge on the new mapping");
        let s = pen(7999, KHZ);
        let cur = m.cursor(Some(s), &devices(), None, s.t_ns);
        let (px, py) = after.screen(&s);
        assert_eq!(cur.source, CursorSource::EvdevFit);
        assert!((cur.x - px).hypot(cur.y - py) < 1.5);
    }

    #[test]
    fn fullscreen_fallback_when_the_window_system_is_silent() {
        let mut m = AbsMap::new();
        m.configure(&devices(), Some((1920, 1080)));
        let s = CursorSample { t_ns: 2_000_000_000, x: 7600, y: 9500, dev: 0 };
        let c = m.cursor(Some(s), &devices(), Some((1920, 1080)), s.t_ns + 1_000_000);
        assert_eq!(c.source, CursorSource::EvdevFullscreen);
        assert_eq!((c.x, c.y), (960.0, 1080.0));
    }
}
