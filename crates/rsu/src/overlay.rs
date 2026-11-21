//! Built-in measurements: what is collected every frame and how it is drawn.
//!
//! Everything here is a software measurement taken with CLOCK_MONOTONIC.
//! "X -> PRESENT" ends when the present call returns. "X -> SCREEN" ends
//! when the compositor says the frame started to show (Wayland
//! presentation-time), which adds GPU time, compositor and the wait for
//! the display's refresh; only the display's own processing and the
//! panel's response stay invisible.

use crate::absmap::CursorSource;
use crate::text::{self, StackStr};
use std::fmt::Write;
use rsu_audio::clock::ClockModel;
use rsu_audio::SAMPLE_RATE;
use rsu_gfx::{Batch, Gfx, PresentMode};
use rsu_platform::evdev::EvdevInput;
use rsu_platform::window::{FrameFeedback, FEEDBACK_HW_CLOCK, FEEDBACK_VSYNC, FEEDBACK_ZERO_COPY};
use rsu_rt::alloc_guard::hot_allocs;
use rsu_telemetry::{Hist, Ring, Running};

const BUCKET_NS: u64 = 50_000;
const GRAPH_FRAMES: usize = 512;

pub struct Metrics {
    pub frame_time: Hist,
    frame_ring: Ring<GRAPH_FRAMES>,
    /// evdev sample timestamp -> present returned, first frame showing it.
    pub cursor_to_present: Hist,
    /// Same for window-system pointer events (arrival time, so a lower bound).
    pub window_cursor_to_present: Hist,
    pub key_to_present: Hist,
    /// Key press -> the moment its hit sound is audible, as far as the
    /// audio server knows (its reported device delay included).
    pub key_to_sound: Hist,
    /// Time between consecutive pointer reports: the device's real rate.
    pub pointer_interval: Hist,
    /// |prediction error| of the audio clock model per cycle.
    pub clock_resid: Hist,
    pub hit_error: Running,
    /// What the compositor said about the frames.
    pub screen: ScreenStats,
    pub present_mode: PresentMode,
    pub cursor_source: CursorSource,
    stage_us: [f32; 5],
    /// GPU-measured execution time of a frame, smoothed (us).
    gpu_exec_us: f32,
    frames: u64,
    allocs_base: u64,
    shown: Shown,
    shown_at: u64,
    shown_frames: u64,
}

/// Numbers as displayed; refreshed a few times a second so they are readable.
#[derive(Clone, Copy, Default)]
struct Shown {
    fps: f32,
    frame: [f32; 3],  // p50, p99, max (ms)
    cursor: [f32; 2], // p50, p99 (ms)
    key: [f32; 2],
    sound: [f32; 2],
    pointer_hz: f32,
    resid_p99_us: f32,
    on_screen: [f32; 2],
    cursor_screen: [f32; 2],
    key_screen: [f32; 2],
    graph_top_ms: f32,
}

fn ms(ns: u64) -> f32 {
    ns as f32 / 1e6
}

impl Metrics {
    pub fn new() -> Self {
        Metrics {
            frame_time: Hist::new(BUCKET_NS),
            frame_ring: Ring::new(),
            cursor_to_present: Hist::new(BUCKET_NS),
            window_cursor_to_present: Hist::new(BUCKET_NS),
            key_to_present: Hist::new(BUCKET_NS),
            key_to_sound: Hist::new(BUCKET_NS),
            pointer_interval: Hist::new(BUCKET_NS),
            clock_resid: Hist::new(1_000),
            hit_error: Running::default(),
            screen: ScreenStats::new(),
            present_mode: PresentMode::Fifo,
            cursor_source: CursorSource::None,
            stage_us: [0.0; 5],
            gpu_exec_us: 0.0,
            frames: 0,
            allocs_base: hot_allocs(),
            shown: Shown::default(),
            shown_at: 0,
            shown_frames: 0,
        }
    }

    pub fn reset(&mut self) {
        let (mode, source) = (self.present_mode, self.cursor_source);
        *self = Metrics::new();
        self.present_mode = mode;
        self.cursor_source = source;
    }

    pub fn set_present_mode(&mut self, mode: PresentMode) {
        self.present_mode = mode;
    }

    pub fn frame(&mut self, dt_ns: u64) {
        self.frame_time.record(dt_ns);
        self.frame_ring.push(ms(dt_ns));
        self.frames += 1;
    }

    /// Durations of the frame's steps, smoothed for display.
    pub fn stages(&mut self, acquire: u64, logic: u64, record: u64, wait: u64, submit: u64) {
        for (s, v) in self.stage_us.iter_mut().zip([acquire, logic, record, wait, submit]) {
            *s += (v as f32 / 1e3 - *s) * 0.02;
        }
    }

    pub fn gpu_exec(&mut self, ns: u64) {
        self.gpu_exec_us += (ns as f32 / 1e3 - self.gpu_exec_us) * 0.02;
    }

    fn allocs(&self) -> u64 {
        hot_allocs() - self.allocs_base
    }

    /// The cursor histogram that matches the active input path.
    fn cursor_hist(&self) -> &Hist {
        if self.cursor_to_present.count() > 0 || self.window_cursor_to_present.count() == 0 {
            &self.cursor_to_present
        } else {
            &self.window_cursor_to_present
        }
    }

    fn refresh(&mut self, now: u64) {
        let dt = now.saturating_sub(self.shown_at);
        if dt < 250_000_000 && self.shown_at != 0 {
            return;
        }
        let p = |h: &Hist, q| ms(h.percentile_ns(q));
        let cursor = self.cursor_hist();
        let interval = self.pointer_interval.percentile_ns(0.5);
        let frame_p99 = p(&self.frame_time, 0.99);
        self.shown = Shown {
            fps: if self.shown_at == 0 { 0.0 } else { (self.frames - self.shown_frames) as f32 * 1e9 / dt as f32 },
            frame: [p(&self.frame_time, 0.5), frame_p99, ms(self.frame_time.max_ns())],
            cursor: [p(cursor, 0.5), p(cursor, 0.99)],
            key: [p(&self.key_to_present, 0.5), p(&self.key_to_present, 0.99)],
            sound: [p(&self.key_to_sound, 0.5), p(&self.key_to_sound, 0.99)],
            pointer_hz: if interval > 0 { 1e9 / interval as f32 } else { 0.0 },
            resid_p99_us: self.clock_resid.percentile_ns(0.99) as f32 / 1e3,
            on_screen: [p(&self.screen.present_to_screen, 0.5), p(&self.screen.present_to_screen, 0.99)],
            cursor_screen: [p(&self.screen.cursor_to_screen, 0.5), p(&self.screen.cursor_to_screen, 0.99)],
            key_screen: [p(&self.screen.key_to_screen, 0.5), p(&self.screen.key_to_screen, 0.99)],
            // Graph scale: the next of 1/2/5/10/20/50 ms above 1.5x p99.
            graph_top_ms: [1.0, 2.0, 5.0, 10.0, 20.0, 50.0].into_iter().find(|t| *t >= frame_p99 * 1.5).unwrap_or(50.0),
        };
        self.shown_at = now;
        self.shown_frames = self.frames;
    }

    pub fn print_summary(&self, gfx: &Gfx, backend: &str, input: Option<&EvdevInput>, audio: bool, clock: ClockModel) {
        let h = |name: &str, h: &Hist| {
            if h.count() == 0 {
                println!("  {name:<28} no samples");
            } else {
                println!(
                    "  {name:<28} p50 {:>6.2} ms  p99 {:>6.2} ms  max {:>6.2} ms  (n={})",
                    ms(h.percentile_ns(0.5)),
                    ms(h.percentile_ns(0.99)),
                    ms(h.max_ns()),
                    h.count()
                );
            }
        };
        println!("rsu measurements ({backend}, {}, present {})", gfx.device_name(), self.present_mode.name());
        h("frame time", &self.frame_time);
        if self.frame_time.mean_ns() > 0 {
            println!("  {:<28} {:.0}", "mean fps", 1e9 / self.frame_time.mean_ns() as f64);
        }
        h("evdev cursor -> present", &self.cursor_to_present);
        h("window cursor -> present", &self.window_cursor_to_present);
        h("key -> present", &self.key_to_present);
        h("key -> sound (reported)", &self.key_to_sound);
        let sc = &self.screen;
        if sc.asked {
            h("present -> screen", &sc.present_to_screen);
            h("cursor -> screen", &sc.cursor_to_screen);
            h("key -> screen", &sc.key_to_screen);
            println!("  {:<28} {}", "frames on screen", sc.describe());
        } else {
            println!("  {:<28} not reported by this window system", "frames on screen");
        }
        h("pointer report interval", &self.pointer_interval);
        println!(
            "  {:<28} acquire {:.0}  logic+build {:.0}  record {:.0}  gpu-wait {:.0}  latch+submit+present {:.0}",
            "frame steps (us, smoothed)",
            self.stage_us[0],
            self.stage_us[1],
            self.stage_us[2],
            self.stage_us[3],
            self.stage_us[4]
        );
        println!("  {:<28} {:.0} us", "gpu execution (gpu clock)", self.gpu_exec_us);
        println!("  {:<28} {}", "cursor source", self.cursor_source.name());
        if let Some(i) = input {
            println!(
                "  {:<28} realtime={} queue_overflows={} kernel_drops={}",
                "input thread",
                i.stats.realtime.load(std::sync::atomic::Ordering::Relaxed),
                i.stats.queue_overflows.load(std::sync::atomic::Ordering::Relaxed),
                i.stats.kernel_drops.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
        if audio && clock.valid {
            // Drift is the device against the system clock; it only means
            // something at normal speed.
            let ratio = clock.rate / (SAMPLE_RATE as f64 / 1e9);
            let rate = if (ratio - 1.0).abs() < 0.01 {
                format!("drift {:+.0} ppm", (ratio - 1.0) * 1e6)
            } else {
                format!("running at {ratio:.2}x")
            };
            println!(
                "  {:<28} quantum {} frames, device delay {:.2} ms, model error p50 {:.1} us p99 {:.1} us (n={}), {rate}",
                "audio clock",
                clock.quantum,
                clock.delay_frames as f64 / SAMPLE_RATE as f64 * 1e3,
                self.clock_resid.percentile_ns(0.5) as f64 / 1e3,
                self.clock_resid.percentile_ns(0.99) as f64 / 1e3,
                self.clock_resid.count()
            );
        } else {
            println!("  {:<28} not running", "audio clock");
        }
        if self.hit_error.n() > 0 {
            println!(
                "  {:<28} mean {:+.2} ms  sd {:.2} ms  (n={})",
                "hit error",
                self.hit_error.mean(),
                self.hit_error.stddev(),
                self.hit_error.n()
            );
        }
        println!("  {:<28} {}", "rust allocations in frames", self.allocs());
    }
}

/// Facts the overlay shows that are not measurements.
pub struct Info {
    pub backend: &'static str,
    pub present: &'static str,
    pub fps_cap: u32,
    pub source: CursorSource,
    pub fit_rms: Option<f32>,
    pub input_rt: bool,
    pub evdev_keys: bool,
    pub queue_overflows: u64,
    pub kernel_drops: u64,
    pub clock: ClockModel,
    pub audio: bool,
    pub focused: bool,
}

const SCALE: f32 = 2.0;
const LINE: f32 = 14.0;
const TEXT: [u8; 4] = [225, 230, 235, 255];
const DIM: [u8; 4] = [140, 150, 160, 255];
const GOOD: [u8; 4] = [120, 230, 150, 255];
const WARN: [u8; 4] = [255, 180, 70, 255];

pub fn draw(batch: &mut Batch, m: &mut Metrics, info: &Info, _w: f32, _h: f32, now: u64) {
    m.refresh(now);
    let s = m.shown;
    let (x0, y0) = (16.0, 16.0);
    let panel_w = 592.0; // fits the longest text line at SCALE
    batch.rect(x0 - 8.0, y0 - 8.0, panel_w, LINE * 13.0 + 190.0, [0, 0, 0, 170]);

    let mut y = y0;
    let mut line = StackStr::<96>::new();
    let mut put = |batch: &mut Batch, line: &mut StackStr<96>, color: [u8; 4]| {
        text::draw_bitmap(batch, x0, y, SCALE, color, line.as_str());
        line.clear();
        y += LINE;
    };

    let _ = write!(line, "RSU  {}  PRESENT {}", info.backend, info.present);
    if info.fps_cap > 0 {
        let _ = write!(line, "  CAP {}", info.fps_cap);
    }
    put(batch, &mut line, TEXT);

    let _ =
        write!(line, "FPS {:.0}  FRAME P50 {:.2}  P99 {:.2}  MAX {:.1} MS", s.fps, s.frame[0], s.frame[1], s.frame[2]);
    put(batch, &mut line, TEXT);

    let st = m.stage_us;
    let _ = write!(
        line,
        "US: ACQ {:.0} LOGIC {:.0} REC {:.0} GPU-WAIT {:.0} SUBMIT {:.0} GPU-EXEC {:.0}",
        st[0], st[1], st[2], st[3], st[4], m.gpu_exec_us
    );
    put(batch, &mut line, DIM);

    let _ = write!(line, "CURSOR: {}", info.source.name());
    if let (CursorSource::EvdevFit, Some(rms)) = (info.source, info.fit_rms) {
        let _ = write!(line, " RMS {rms:.2} PX");
    }
    put(batch, &mut line, if info.source.is_evdev() { GOOD } else { WARN });

    let _ = write!(line, "CURSOR -> PRESENT  P50 {:.2}  P99 {:.2} MS", s.cursor[0], s.cursor[1]);
    if s.pointer_hz > 0.0 {
        let _ = write!(line, "   DEVICE {:.0} HZ", s.pointer_hz);
    }
    put(batch, &mut line, TEXT);

    let _ = write!(
        line,
        "KEY -> PRESENT     P50 {:.2}  P99 {:.2} MS   KEYS: {}",
        s.key[0],
        s.key[1],
        if info.evdev_keys { "EVDEV" } else { "WINDOW-SYSTEM" }
    );
    put(batch, &mut line, TEXT);

    let sc = &m.screen;
    if sc.asked && sc.answered() > 0 {
        let _ = write!(
            line,
            "SCREEN: CURSOR {:.2}/{:.2}  KEY {:.2}/{:.2}  PRESENT {:.2}/{:.2} MS",
            s.cursor_screen[0], s.cursor_screen[1], s.key_screen[0], s.key_screen[1], s.on_screen[0], s.on_screen[1]
        );
        put(batch, &mut line, TEXT);
        let pct = |n: u64| n as f32 * 100.0 / sc.answered().max(1) as f32;
        let _ = write!(
            line,
            "  {:.0} HZ  DROPPED {:.0}%  TORN {:.0}%  DIRECT {:.0}%  {}",
            sc.refresh_hz(),
            pct(sc.discarded),
            pct(sc.shown - sc.vsync),
            pct(sc.zero_copy),
            if sc.hw_clock == sc.shown { "HW CLOCK" } else { "SW CLOCK" }
        );
        put(batch, &mut line, DIM);
    } else {
        let _ = write!(line, "SCREEN: {}", if sc.asked { "NO ANSWER FROM COMPOSITOR" } else { "NOT REPORTED" });
        put(batch, &mut line, WARN);
        put(batch, &mut line, DIM);
    }

    let _ =
        write!(line, "KEY -> SOUND       P50 {:.2}  P99 {:.2} MS   (DEVICE DELAY AS REPORTED)", s.sound[0], s.sound[1]);
    put(batch, &mut line, TEXT);

    if info.audio && info.clock.valid {
        let _ = write!(
            line,
            "AUDIO: QUANTUM {}  DELAY {:.1} MS  CLOCK ERROR P99 {:.0} US",
            info.clock.quantum,
            info.clock.delay_frames / SAMPLE_RATE as f32 * 1e3,
            s.resid_p99_us
        );
        put(batch, &mut line, TEXT);
    } else {
        let _ = write!(line, "AUDIO: NOT RUNNING (SYSTEM CLOCK)");
        put(batch, &mut line, WARN);
    }

    if m.hit_error.n() > 0 {
        let _ = write!(
            line,
            "HIT ERROR: MEAN {:+.1} MS  SD {:.1} MS  N {}",
            m.hit_error.mean(),
            m.hit_error.stddev(),
            m.hit_error.n()
        );
    } else {
        let _ = write!(line, "HIT ERROR: PRESS Z / X ON THE BEAT");
    }
    put(batch, &mut line, TEXT);

    let allocs = m.allocs();
    let lost = info.queue_overflows + info.kernel_drops;
    let _ = write!(
        line,
        "ALLOCS IN FRAMES {}  INPUT LOST {}  INPUT THREAD {}{}",
        allocs,
        lost,
        if info.input_rt { "REALTIME" } else { "NORMAL" },
        if info.focused { "" } else { "  (NO FOCUS)" }
    );
    put(batch, &mut line, if allocs == 0 && lost == 0 { GOOD } else { WARN });

    // Frame-time graph: one pixel column per frame, newest on the right.
    let _ = write!(line, "FRAME TIME: LAST {} FRAMES  FULL SCALE {:.0} MS", GRAPH_FRAMES, s.graph_top_ms);
    put(batch, &mut line, DIM);
    let graph_h = 60.0;
    batch.rect(x0, y, GRAPH_FRAMES as f32, graph_h, [255, 255, 255, 14]);
    let n = m.frame_ring.len();
    let median = s.frame[0];
    for i in 0..n {
        let v = m.frame_ring.get(i);
        let bar = (v / s.graph_top_ms).min(1.0) * graph_h;
        // A frame that took twice the median is a visible hitch.
        let color = if v > median * 2.0 { WARN } else { [90, 200, 255, 255] };
        batch.rect(x0 + (GRAPH_FRAMES - n + i) as f32, y + graph_h - bar, 1.0, bar.max(1.0), color);
    }
    y += graph_h + 8.0;

    // Histogram of cursor -> present, 0..10 ms in 0.1 ms bars.
    let hist = m.cursor_hist();
    let _ = write!(line, "CURSOR -> PRESENT HISTOGRAM: 0..10 MS  N {}", hist.count());
    text::draw_bitmap(batch, x0, y, SCALE, DIM, line.as_str());
    y += LINE;
    let hist_h = 60.0;
    const BARS: usize = 100;
    let bar_w = 5.0;
    batch.rect(x0, y, BARS as f32 * bar_w, hist_h, [255, 255, 255, 14]);
    let per_bar = (100_000 / BUCKET_NS) as usize;
    let buckets = hist.buckets();
    let bar_count = |i: usize| buckets[i * per_bar..(i + 1) * per_bar].iter().sum::<u32>();
    let peak = (0..BARS).map(bar_count).max().unwrap_or(0).max(1);
    for i in 0..BARS {
        let c = bar_count(i);
        if c > 0 {
            let bar = (c as f32 / peak as f32 * hist_h).max(1.0);
            batch.rect(x0 + i as f32 * bar_w, y + hist_h - bar, bar_w - 1.0, bar, GOOD);
        }
    }
    // Millisecond ticks under the histogram.
    for t in 0..=10 {
        batch.rect(x0 + t as f32 * 10.0 * bar_w, y + hist_h, 1.0, 4.0, DIM);
    }
}

/// One presented frame, as far as the measurements need it, kept until the
/// compositor answers about it.
#[derive(Clone, Copy, Default)]
pub struct SentFrame {
    pub tag: u64,
    /// When the present call returned.
    pub present_ns: u64,
    /// Timestamp of the cursor sample the frame shows, if it was new.
    pub cursor_ns: u64,
    pub keys: [u64; 4],
    pub nkeys: usize,
}

const SENT: usize = 32;

/// Frames as the compositor saw them: when they reached the screen, or
/// that they were replaced before they could.
pub struct ScreenStats {
    /// Feedback was requested (the window system supports it).
    pub asked: bool,
    pub present_to_screen: Hist,
    pub cursor_to_screen: Hist,
    pub key_to_screen: Hist,
    pub shown: u64,
    pub discarded: u64,
    pub vsync: u64,
    pub zero_copy: u64,
    pub hw_clock: u64,
    /// Answers that came before the present call returned: the request
    /// was attached to the wrong commit. Should stay 0.
    pub early: u64,
    /// Answers for frames no longer remembered.
    pub unmatched: u64,
    /// Without a usable clock (compositor not on CLOCK_MONOTONIC).
    pub no_time: u64,
    refresh_ns: u32,
    sent: [SentFrame; SENT],
    /// Keys of discarded frames: they reach the screen with a later one.
    carried: [u64; 8],
    ncarried: usize,
}

impl ScreenStats {
    fn new() -> Self {
        ScreenStats {
            asked: false,
            present_to_screen: Hist::new(BUCKET_NS),
            cursor_to_screen: Hist::new(BUCKET_NS),
            key_to_screen: Hist::new(BUCKET_NS),
            shown: 0,
            discarded: 0,
            vsync: 0,
            zero_copy: 0,
            hw_clock: 0,
            early: 0,
            unmatched: 0,
            no_time: 0,
            refresh_ns: 0,
            sent: [SentFrame::default(); SENT],
            carried: [0; 8],
            ncarried: 0,
        }
    }

    pub fn answered(&self) -> u64 {
        self.shown + self.discarded
    }

    pub fn refresh_hz(&self) -> f32 {
        if self.refresh_ns > 0 {
            1e9 / self.refresh_ns as f32
        } else {
            0.0
        }
    }

    /// Remember a presented frame until its answer comes.
    pub fn sent(&mut self, frame: SentFrame) {
        self.asked = true;
        self.sent[(frame.tag % SENT as u64) as usize] = frame;
    }

    pub fn answer(&mut self, fb: &FrameFeedback) {
        let slot = &self.sent[(fb.tag % SENT as u64) as usize];
        if slot.tag != fb.tag || slot.present_ns == 0 {
            self.unmatched += 1;
            return;
        }
        let frame = *slot;
        if fb.discarded {
            self.discarded += 1;
            for &k in &frame.keys[..frame.nkeys] {
                if self.ncarried < self.carried.len() {
                    self.carried[self.ncarried] = k;
                    self.ncarried += 1;
                }
            }
            return;
        }
        self.shown += 1;
        self.vsync += (fb.flags & FEEDBACK_VSYNC != 0) as u64;
        self.zero_copy += (fb.flags & FEEDBACK_ZERO_COPY != 0) as u64;
        self.hw_clock += (fb.flags & FEEDBACK_HW_CLOCK != 0) as u64;
        if fb.refresh_ns > 0 {
            self.refresh_ns = fb.refresh_ns;
        }
        if fb.shown_ns == 0 {
            self.no_time += 1;
            return;
        }
        if fb.shown_ns < frame.present_ns {
            self.early += 1;
            return;
        }
        self.present_to_screen.record(fb.shown_ns - frame.present_ns);
        if frame.cursor_ns != 0 {
            self.cursor_to_screen.record(fb.shown_ns.saturating_sub(frame.cursor_ns));
        }
        for &k in frame.keys[..frame.nkeys].iter().chain(&self.carried[..self.ncarried]) {
            self.key_to_screen.record(fb.shown_ns.saturating_sub(k));
        }
        self.ncarried = 0;
    }

    fn describe(&self) -> String {
        let n = self.answered();
        let pct = |k: u64| k as f64 * 100.0 / n.max(1) as f64;
        format!(
            "{} answered: {} shown, {:.1}% dropped, {:.1}% torn, {:.1}% direct scanout, {:.1}% hw clock, \
             {:.0} Hz; early {} unmatched {} no-time {}",
            n,
            self.shown,
            pct(self.discarded),
            pct(self.shown - self.vsync),
            pct(self.zero_copy),
            pct(self.hw_clock),
            self.refresh_hz(),
            self.early,
            self.unmatched,
            self.no_time
        )
    }
}
