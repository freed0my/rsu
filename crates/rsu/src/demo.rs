//! The stage-1 test pattern: one circle pulsing to a metronome or a track,
//! with hits judged against the beat grid. Kept as the no-beatmap mode
//! because it isolates timing from everything else.

use crate::text::{self, StackStr};
use std::fmt::Write;
use rsu_audio::mixer::BeatGrid;
use rsu_audio::SAMPLE_RATE;
use rsu_gfx::Batch;

pub struct Demo {
    pub grid: BeatGrid,
    pub offset_frames: f64,
    /// Beat phase 0..1 at the time the current frame is expected on screen.
    pub phase: f32,
    /// Last hit errors in ms, newest last, for the error bar.
    recent_errors: [f32; 16],
    recent_len: usize,
    pub last_hit_ns: u64,
    /// Every tap's error in ms, for calibration. Fixed capacity.
    pub taps: Vec<f64>,
    calibrating: bool,
}

impl Demo {
    pub fn new(grid: BeatGrid, offset_frames: f64, calibrating: bool) -> Demo {
        Demo {
            grid,
            offset_frames,
            phase: 0.0,
            recent_errors: [0.0; 16],
            recent_len: 0,
            last_hit_ns: 0,
            taps: Vec::with_capacity(4096),
            calibrating,
        }
    }

    /// Signed distance, in ms, from stream position `pos` to the nearest beat.
    pub fn error_ms(&self, pos: f64) -> f64 {
        let beats = (pos - self.grid.offset_frames) / self.grid.period_frames;
        (beats - beats.round()) * self.grid.period_frames / SAMPLE_RATE as f64 * 1000.0
    }

    pub fn set_phase(&mut self, pos: f64) {
        let beats = (pos - self.grid.offset_frames) / self.grid.period_frames;
        self.phase = (beats - beats.floor()) as f32;
    }

    pub fn push_error(&mut self, e: f32) {
        if self.taps.len() < self.taps.capacity() {
            self.taps.push(e as f64);
        }
        if self.recent_len == self.recent_errors.len() {
            self.recent_errors.copy_within(1.., 0);
            self.recent_len -= 1;
        }
        self.recent_errors[self.recent_len] = e;
        self.recent_len += 1;
    }

    pub fn draw(&self, batch: &mut Batch, w: f32, h: f32, now: u64) {
        let (cx, cy) = (w * 0.5, h * 0.5);
        let r = (h * 0.09).max(24.0);
        // Approach ring: shrinks onto the circle exactly on the beat.
        let approach = r * (1.0 + 2.0 * (1.0 - self.phase));
        batch.ring(cx, cy, approach, 3.0, [120, 190, 255, (80.0 + 175.0 * self.phase) as u8], 0);
        // The circle flashes on the beat and decays.
        let flash = (-self.phase * 7.0).exp();
        let level = (70.0 + 185.0 * flash) as u8;
        batch.disc(cx, cy, r, [level / 3, level / 2, level, 255], 0);
        batch.ring(cx, cy, r, 4.0, [235, 235, 245, 255], 0);

        // Hit error bar: +-50 ms, early to the left.
        let (bar_w, bar_y) = (r * 5.0, cy + r * 3.6);
        batch.rect(cx - bar_w * 0.5, bar_y - 1.0, bar_w, 2.0, [90, 90, 100, 255]);
        batch.rect(cx - 1.0, bar_y - 8.0, 2.0, 16.0, [200, 200, 210, 255]);
        for (i, e) in self.recent_errors[..self.recent_len].iter().enumerate() {
            let newest = i + 1 == self.recent_len;
            let x = cx + (e / 50.0).clamp(-1.0, 1.0) * bar_w * 0.5;
            let alpha = (60 + 195 * (i + 1) / self.recent_len) as u8;
            let color = if e.abs() < 20.0 { [110, 220, 255, alpha] } else { [255, 170, 60, alpha] };
            let half = if newest { 12.0 } else { 7.0 };
            batch.rect(x - 1.0, bar_y - half, 2.0, half * 2.0, color);
        }
        if self.last_hit_ns != 0 && now.saturating_sub(self.last_hit_ns) < 120_000_000 {
            batch.ring(cx, cy, r * 1.15, 3.0, [255, 255, 255, 160], 0);
        }
        if self.calibrating {
            let scale = (h / 360.0).max(2.0).round();
            let mut line = StackStr::<96>::new();
            let _ = write!(line, "TAP Z OR X ON EVERY BEAT.  {} TAPS.  ESC SAVES", self.taps.len());
            let width = text::width(scale, line.as_str());
            let colour = if self.taps.len() >= 20 { [120, 230, 150, 255] } else { [240, 240, 245, 255] };
            text::draw(batch, cx - width * 0.5, cy - r * 4.2, scale, colour, line.as_str());
        }
    }
}
