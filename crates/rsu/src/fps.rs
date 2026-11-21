//! The frame counter in the bottom right corner, as McOsu draws it: the
//! frames per second, and under it how long a frame takes in milliseconds.
//! And, for the first seconds of a map, the refresh rate of the display
//! (stable says this too, so that a player knows what they are playing on).

use crate::ui::{self, Metrics};
use std::fmt::Write;
use rsu_gfx::Batch;

use crate::text::StackStr;

/// How long the refresh rate is shown from the start of a map.
pub const HZ_SHOWN_S: f32 = 5.0;

#[derive(Default)]
pub struct FpsMeter {
    fps: f32,
}

/// Size of the text and where its lines are: the milliseconds on the
/// bottom edge, the frames per second above, McOsu's `drawFps`.
fn lines(m: &Metrics) -> (f32, f32, f32, f32) {
    let px = m.px(ui::FONT_DEFAULT) * 0.85;
    let margin = m.px(3.0).round();
    let y_ms = m.h - margin - ui::cap(px);
    let step = ui::cap(px) * 1.6;
    (px, m.w - margin, y_ms - step, y_ms)
}

impl FpsMeter {
    /// One frame that took `dt_ns` from the previous one; the rate is
    /// smoothed the way McOsu does (`osu_hud_fps_smoothing`).
    pub fn frame(&mut self, dt_ns: u64) {
        let dt = (dt_ns as f32 / 1e9).max(1e-6);
        let now = 1.0 / dt;
        let keep = 0.05f32.powf(dt);
        self.fps = if self.fps <= 0.0 { now } else { keep * self.fps + (1.0 - keep) * now };
    }

    pub fn draw(&self, batch: &mut Batch, m: &Metrics, now_ns: u64) {
        if self.fps <= 0.0 {
            return;
        }
        let fps = self.fps.round();
        let (px, right, y_fps, y_ms) = lines(m);
        // White from 200, yellow from 120, below that a red that pulses.
        let colour = if fps >= 200.0 {
            ui::WHITE
        } else if fps >= 120.0 {
            [221, 221, 0, 255]
        } else {
            let pulse = (now_ns as f32 / 1e9 * 4.0).sin().abs();
            [255, (66.0 * pulse) as u8, (66.0 * pulse) as u8, 255]
        };
        let mut line = StackStr::<24>::new();
        let _ = write!(line, "{fps:.0} fps");
        let w = ui::text_width(px, line.as_str());
        ui::shadowed(batch, (right - w).round(), y_fps, px, colour, line.as_str());
        line.clear();
        let _ = write!(line, "{:.1} ms", 1000.0 / fps);
        let w = ui::text_width(px, line.as_str());
        ui::shadowed(batch, (right - w).round(), y_ms, px, colour, line.as_str());
    }
}

/// "240 Hz" right above the counter, fading out; `age` is seconds since
/// the map began.
pub fn draw_refresh(batch: &mut Batch, m: &Metrics, hz: f32, age: f32) {
    if age >= HZ_SHOWN_S {
        return;
    }
    let fade = (HZ_SHOWN_S - age).clamp(0.0, 1.0);
    let (px, right, y_fps, y_ms) = lines(m);
    let mut line = StackStr::<24>::new();
    let _ = write!(line, "{} Hz", hz.round() as u32);
    let w = ui::text_width(px, line.as_str());
    let y = y_fps - (y_ms - y_fps);
    ui::shadowed(batch, (right - w).round(), y, px, ui::alpha(ui::WHITE, fade), line.as_str());
}
