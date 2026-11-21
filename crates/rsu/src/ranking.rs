//! The result screen, laid out as McOsu's (`OsuRankingScreen`): the map
//! and the player across the top, the panel of counts below it on the
//! left, the grade on the right.

use crate::browser::draw_back;
use crate::session::Outcome;
use crate::skin::{Skin, Sprite, MOD_IMAGES};
use crate::text::StackStr;
use crate::ui::{self, Backdrop, Input, Metrics, Rect};
use std::fmt::Write;
use rsu_core::game::{Input as GameInput, Summary};
use rsu_core::{mods, Time};
use rsu_formats::osr::Frame;
use rsu_gfx::Batch;

const KEY_W: u32 = 17;
const KEY_R: u32 = 19;

/// The letter grade, by osu!'s rules: the share of 300s decides, misses
/// and 50s cap it.
pub fn rank(s: &Summary) -> &'static str {
    let total = s.n300 + s.n100 + s.n50 + s.miss;
    if total == 0 {
        return "-";
    }
    let share = |n: u32| n as f64 / total as f64;
    let (r300, r50, clean) = (share(s.n300), share(s.n50), s.miss == 0);
    if s.n300 == total {
        "SS"
    } else if r300 > 0.9 && r50 <= 0.01 && clean {
        "S"
    } else if (r300 > 0.8 && clean) || r300 > 0.9 {
        "A"
    } else if (r300 > 0.7 && clean) || r300 > 0.8 {
        "B"
    } else if r300 > 0.6 {
        "C"
    } else {
        "D"
    }
}

/// The grade with the mods: hidden turns SS and S silver ("SSH", "SH").
pub fn graded(s: &Summary, game_mods: u32) -> &'static str {
    match rank(s) {
        "SS" if game_mods & mods::HIDDEN != 0 => "SSH",
        "S" if game_mods & mods::HIDDEN != 0 => "SH",
        g => g,
    }
}

/// The letters a grade is written with ("SSH" is a silver "SS").
pub fn grade_letters(grade: &str) -> &str {
    grade.strip_suffix('H').unwrap_or(grade)
}

/// Where a grade's images are in `skin::Menu::grades`.
pub fn grade_index(grade: &str) -> usize {
    crate::skin::GRADES.iter().position(|g| *g == grade).unwrap_or(5)
}

pub fn grade_colour(grade: &str) -> [u8; 4] {
    match grade {
        "SS" | "S" => [255, 215, 90, 255],
        "SSH" | "SH" => [215, 220, 235, 255],
        "A" => [120, 230, 150, 255],
        "B" => [120, 190, 255, 255],
        "C" => [200, 140, 255, 255],
        _ => [255, 110, 110, 255],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultsAction {
    None,
    Back,
    Retry,
    Watch,
}

pub struct Results {
    pub outcome: Outcome,
    /// "Artist - Title [Difficulty]".
    pub title: String,
    pub creator: String,
    pub player: String,
    /// "2026-11-15 12:34", local time; made when the screen is.
    pub played: String,
    pub mods: u32,
    /// Effective map rate, independent of the displayed mod icons.
    pub map_speed: f64,
    /// The play itself, for watching it again.
    pub recorded: Vec<GameInput>,
    /// Whether this result has a compatible recording in memory.
    pub watch_available: bool,
    /// Retry explicitly drops unsupported stable modifiers.
    pub retry_drops_mods: bool,
    /// Original stable frames for the viewer, never used for judgement.
    pub stable_frames: Option<Vec<Frame>>,
    pub end_time: Time,
    pub shown: f32,
    pub back_hover: f32,
}

/// A moment as local date and time.
pub fn date_time(unix_time: i64) -> String {
    let t = unix_time as libc::time_t;
    // SAFETY: both pointers are to live values of the right types;
    // localtime_r writes only into `tm`.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

impl Results {
    fn buttons(m: &Metrics) -> [Rect; 2] {
        let (w, h, gap) = (m.px(280.0).min(m.w * 0.45), m.px(54.0), m.px(12.0));
        let x = m.w - w - m.px(24.0);
        [1.0, 0.0].map(|i: f32| Rect::new(x, m.h - m.px(24.0) - h - i * (h + gap), w, h))
    }

    fn watch_button(slot: Rect, skin: &Skin) -> (Rect, Option<Sprite>) {
        if let Some(sprite) = skin.menu.ranking_replay.filter(|s| s.visible()) {
            let scale = (slot.w / sprite.w).min(slot.h / sprite.h);
            let (w, h) = (sprite.w * scale, sprite.h * scale);
            (Rect::new(slot.x + slot.w - w, slot.y + (slot.h - h) * 0.5, w, h), Some(sprite))
        } else {
            // A transparent asset can hide the skin art, but not the action.
            (slot, None)
        }
    }

    fn unstable_rate(&self) -> Option<f64> {
        let (n, sd) = (self.outcome.hit_count, self.outcome.hit_sd_ms);
        if n < 2 || !sd.is_finite() || sd < 0.0 {
            return None;
        }
        // Running reports sample SD (n-1); UR uses all observed hits (n), in map time.
        Some(10.0 * sd * ((n - 1) as f64 / n as f64).sqrt())
    }

    fn back(m: &Metrics, skin: &Skin) -> Rect {
        let unit = m.h / 768.0;
        match skin.menu.back.filter(|s| s.visible()) {
            Some(s) => {
                let h = (s.h * unit).min(m.h * 0.17);
                Rect::new(0.0, m.h - h, s.w * unit * h / (s.h * unit), h)
            }
            None => Rect::new(0.0, m.h - m.h * 0.084, m.osu(86.0), m.h * 0.084),
        }
    }

    pub fn can_watch(&self) -> bool {
        self.watch_available && (self.mods & mods::SPUN_OUT == 0 || self.outcome.replay_saved)
    }

    pub fn update(&mut self, input: &Input, m: &Metrics, skin: &Skin) -> ResultsAction {
        ui::approach(&mut self.shown, 1.0, input.dt, 0.4);
        let back = Self::back(m, skin);
        ui::approach(&mut self.back_hover, input.over(back) as u8 as f32, input.dt, 0.15);
        let [retry, watch_slot] = Self::buttons(m);
        let (watch, _) = Self::watch_button(watch_slot, skin);
        let mut action = ResultsAction::None;
        if input.clicked(back) {
            action = ResultsAction::Back;
        } else if input.clicked(retry) {
            action = ResultsAction::Retry;
        } else if self.can_watch() && input.clicked(watch) {
            action = ResultsAction::Watch;
        }
        for &code in input.keys {
            match code {
                ui::KEY_ENTER | ui::KEY_ESC | ui::KEY_SPACE => action = ResultsAction::Back,
                KEY_R => action = ResultsAction::Retry,
                KEY_W if self.can_watch() => action = ResultsAction::Watch,
                _ => {}
            }
        }
        action
    }

    pub fn draw(&self, batch: &mut Batch, m: &Metrics, skin: &Skin, backdrop: &Backdrop, input: &Input) {
        backdrop.draw(batch, m, [0x66, 0x66, 0x66, 255]);
        let o = &self.outcome;
        let sum = &o.summary;
        let bar_h = m.osu(59.0).floor();
        let panel_y = bar_h - 1.0;
        let osu = |x: f32| m.osu(x);
        let mut line = StackStr::<200>::new();

        // The panel: 622x505 pixels of image that take 317 of osu!'s.
        let panel_scale = m.image_scale(622.0, 505.0, 317.0);
        match skin.menu.ranking_panel.filter(|s| s.visible()) {
            Some(s) => s.draw_in(batch, 0.0, panel_y, s.w * panel_scale, s.h * panel_scale, ui::WHITE),
            None if skin.menu.ranking_panel.is_some() => {}
            None => {
                let (w, h) = (622.0 * panel_scale, 505.0 * panel_scale);
                batch.rrect(-h * 0.1, panel_y + osu(4.0), w + h * 0.1, h, osu(8.0), [0, 0, 0, 150]);
                batch.rect(0.0, panel_y + osu(240.0), w, osu(1.0).max(1.0), [255, 255, 255, 50]);
            }
        }

        // Numbers in the skin's score font, or in ours at the same size.
        let number = |batch: &mut Batch, x: f32, cy: f32, height: f32, text: &str| match &skin.score {
            Some(digits) => {
                let unit = height / digits.height(1.0).max(1.0);
                digits.draw(batch, x, cy, unit, ui::WHITE, text);
            }
            None => {
                let px = ui::em_for(height * 0.8);
                ui::shadowed(batch, x, cy - ui::cap(px) * 0.5, px, ui::WHITE, text);
            }
        };
        let (big, small) = (osu(26.0), osu(22.0));
        let _ = write!(line, "{:08}", sum.score);
        number(batch, osu(111.0), panel_y + osu(11.0) + big * 0.5, big, line.as_str());

        // Counts: 300, 100 and 50 down the left, misses at the bottom right.
        let unit = m.h / 768.0 * 0.5;
        let labels = [
            ("300", [120, 190, 255, 255]),
            ("100", [110, 230, 120, 255]),
            ("50", [255, 200, 80, 255]),
            ("X", [255, 90, 90, 255]),
        ];
        let cells = [
            (3usize, sum.n300, 40.0, 100.0),
            (2, sum.n100, 40.0, 160.0),
            (1, sum.n50, 40.0, 220.0),
            (0, sum.miss, 240.0, 220.0),
        ];
        for (n, (image, count, x, y)) in cells.into_iter().enumerate() {
            let (cx, cy) = (osu(x), panel_y + osu(y));
            match skin.hit[image].filter(|s| s.visible()) {
                Some(s) => s.draw(batch, cx, cy, unit.min(osu(44.0) / s.h.max(1.0)), ui::WHITE),
                None if skin.hit[image].is_some() => {}
                None => ui::centred(batch, cx, cy, ui::em_for(osu(15.0)), labels[n].1, labels[n].0),
            }
            line.clear();
            let _ = write!(line, "{count}x");
            number(batch, osu(x + 40.0), cy + osu(-16.0) + small * 0.5, small, line.as_str());
        }

        // Combo and accuracy, each under its label.
        let label =
            |batch: &mut Batch, sprite: Option<Sprite>, nominal: (f32, f32), size: f32, x: f32, y: f32, text: &str| {
                match sprite.filter(|s| s.visible()) {
                    Some(s) => {
                        let scale = m.image_scale(nominal.0, nominal.1, size);
                        s.draw(batch, osu(x) + s.w * scale * 0.5, panel_y + osu(y), scale, ui::WHITE);
                    }
                    None if sprite.is_some() => {}
                    None => {
                        let px = ui::em_for(osu(9.0));
                        ui::shadowed(
                            batch,
                            osu(x + 8.0),
                            panel_y + osu(y) - ui::cap(px) * 0.5,
                            px,
                            [220, 220, 220, 255],
                            text,
                        );
                    }
                }
            };
        let old = skin.menu.version <= 1.0 && !skin.name.is_empty();
        let lift = if old { 12.0 } else { 0.0 };
        label(batch, skin.menu.ranking_maxcombo, (162.0, 50.0), 32.0, 4.0, 255.0 + lift, "Combo");
        label(batch, skin.menu.ranking_accuracy, (192.0, 58.0), 36.0, 183.0, 257.0 + lift, "Accuracy");
        line.clear();
        let _ = write!(line, "{}x", sum.max_combo);
        number(batch, osu(15.0), panel_y + osu(270.0) + small * 0.5, small, line.as_str());
        line.clear();
        let _ = write!(line, "{:.2}%", sum.accuracy() * 100.0);
        number(batch, osu(195.0), panel_y + osu(270.0) + small * 0.5, small, line.as_str());

        // The grade.
        let grade = graded(sum, self.mods);
        let (gx, gy) = (m.w - osu(120.0), osu(if old { 170.0 } else { 200.0 }));
        let pop = 1.0 + 0.6 * (1.0 - self.shown).powi(2);
        match skin.menu.grades[grade_index(grade)].filter(|s| s.visible()) {
            Some(s) => {
                s.draw(batch, gx, gy, m.image_scale(369.0, 422.0, 230.0) * pop, ui::alpha(ui::WHITE, self.shown))
            }
            None if skin.menu.grades[grade_index(grade)].is_some() => {}
            None => {
                let letters = grade_letters(grade);
                let px = ui::em_for(osu(150.0)) * pop * if letters.len() > 1 { 0.7 } else { 1.0 };
                ui::centred(batch, gx + px * 0.03, gy + px * 0.03, px, [0, 0, 0, (150.0 * self.shown) as u8], letters);
                ui::centred(batch, gx, gy, px, ui::alpha(grade_colour(grade), self.shown), letters);
            }
        }

        // The mods the play had, from the right edge leftwards.
        let mut mx = m.w - osu(20.0);
        let shown = if self.mods & mods::NIGHTCORE != 0 { self.mods & !mods::DOUBLE_TIME } else { self.mods };
        for (n, (bit, _)) in MOD_IMAGES.iter().enumerate() {
            if shown & bit == 0 {
                continue;
            }
            match skin.menu.mods[n].filter(|s| s.visible()) {
                Some(s) => {
                    let unit = m.h / 768.0;
                    s.draw(batch, mx - s.w * unit * 0.5, osu(260.0), unit, ui::WHITE);
                }
                None if skin.menu.mods[n].is_some() => {}
                None => {
                    let (w, h) = (osu(34.0), osu(22.0));
                    batch.rrect(mx - w, osu(260.0) - h * 0.5, w, h, h * 0.3, [255, 200, 60, 255]);
                    ui::centred(batch, mx - w * 0.5, osu(260.0), ui::em_for(h * 0.5), ui::BLACK, &rsu_formats::osr::mod_names(*bit));
                }
            }
            mx -= osu(20.0);
        }

        // The top: which map, by whom, played by whom and when.
        batch.rect(0.0, 0.0, m.w, bar_h, ui::BLACK);
        let px = (bar_h / 4.4).min(m.px(ui::FONT_SUBTITLE) * 1.3);
        let x = m.px(10.0);
        let step = (bar_h - m.px(8.0)) / 3.0;
        let top = m.px(4.0) + (step - ui::cap(px)) * 0.5;
        ui::text(batch, x, top, px, ui::WHITE, ui::fit(px, &self.title, m.w * 0.7));
        line.clear();
        let _ = write!(line, "Beatmap by {}", self.creator);
        ui::text(batch, x, top + step, px * 0.78, ui::WHITE, line.as_str());
        line.clear();
        let _ = write!(
            line,
            "Played by {} on {}",
            if self.player.is_empty() { "Player" } else { &self.player },
            self.played
        );
        ui::text(batch, x, top + 2.0 * step, px * 0.78, ui::WHITE, line.as_str());
        match skin.menu.ranking_title.filter(|s| s.visible()) {
            Some(s) => {
                let scale = m.image_scale(s.w, s.h, 75.0);
                s.draw_in(batch, m.w - s.w * scale - osu(20.0), 0.0, s.w * scale, s.h * scale, ui::WHITE);
            }
            None if skin.menu.ranking_title.is_some() => {}
            None => ui::right_aligned(
                batch,
                m.w - osu(20.0),
                (bar_h - ui::cap(px * 1.8)) * 0.5,
                px * 1.8,
                ui::WHITE,
                "Ranking",
            ),
        }

        // What rsu adds: how fast the map ran, how the timing was, and what became of the replay.
        let [retry, watch_slot] = Self::buttons(m);
        let px = m.px(ui::FONT_DEFAULT);
        let x = Self::back(m, skin).w + m.px(40.0);
        let width = (retry.x - x - m.px(12.0)).max(0.0);
        let step = ui::cap(px) * 1.45;
        let y = m.h - m.px(24.0) - ui::cap(px);
        let dim = [215, 215, 220, 255];
        let footer = |batch: &mut Batch, y, colour, text: &str| {
            ui::shadowed(batch, x, y, px, colour, ui::fit(px, text, width));
        };
        line.clear();
        let _ = write!(line, "Map speed: {:.2}x", self.map_speed);
        if let Some(ur) = self.unstable_rate() {
            let _ = write!(line, "  |  UR: {ur:.1}");
        }
        footer(batch, y - step * 2.0, dim, line.as_str());
        if o.hit_count > 0 {
            line.clear();
            let side = if o.hit_mean_ms < 0.0 { "early" } else { "late" };
            let _ = write!(line, "Timing: {:.1} ms {side} on average", o.hit_mean_ms.abs());
            footer(batch, y - step, dim, line.as_str());
        }
        let wrong = o.verified == Some(false);
        let status = match (o.replay_saved, o.verified) {
            (_, Some(false)) => "The recording does not reproduce the play",
            (true, Some(true)) => "Replay saved and checked",
            (true, None) => "Replay saved",
            (false, _) => "Replay not saved",
        };
        footer(batch, y, if wrong { [255, 110, 110, 255] } else { dim }, status);

        let (watch, sprite) = Self::watch_button(watch_slot, skin);
        let label_px = m.px(ui::FONT_SUBTITLE) * 0.9;
        ui::button(batch, retry, [70, 120, 60], if self.retry_drops_mods { "Retry: drop mods (R)" } else { "Retry  (R)" }, label_px, input.over(retry));
        if !self.can_watch() {
            ui::button(batch, watch, [65, 65, 65], "Replay unavailable", label_px, false);
        } else if let Some(sprite) = sprite {
            let alpha = if input.over(watch) { 1.0 } else { 0.86 };
            sprite.draw_in(batch, watch.x, watch.y, watch.w, watch.h, ui::alpha(ui::WHITE, alpha));
        } else {
            ui::button(batch, watch, [60, 90, 150], "Watch replay  (W)", label_px, input.over(watch));
        }
        draw_back(batch, m, skin, Self::back(m, skin), self.back_hover);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n300: u32, n100: u32, n50: u32, miss: u32) -> Summary {
        Summary { n300, n100, n50, miss, ..Summary::default() }
    }

    #[test]
    fn grades_follow_the_share_of_300s_and_the_caps() {
        assert_eq!(rank(&s(100, 0, 0, 0)), "SS");
        assert_eq!(rank(&s(95, 5, 0, 0)), "S");
        assert_eq!(rank(&s(95, 4, 0, 1)), "A", "one miss rules out an S");
        assert_eq!(rank(&s(95, 3, 2, 0)), "A", "more than 1% of 50s rules out an S");
        assert_eq!(rank(&s(85, 15, 0, 0)), "A");
        assert_eq!(rank(&s(85, 14, 0, 1)), "B", "over 80% with a miss");
        assert_eq!(rank(&s(75, 25, 0, 0)), "B");
        assert_eq!(rank(&s(75, 24, 0, 1)), "C");
        assert_eq!(rank(&s(61, 0, 0, 39)), "C");
        assert_eq!(rank(&s(60, 40, 0, 0)), "D", "exactly 60% is not more than 60%");
        assert_eq!(rank(&s(0, 0, 0, 0)), "-");
        assert_eq!((grade_index("SS"), grade_index("A"), grade_index("-")), (0, 2, 5));
        assert_eq!((graded(&s(100, 0, 0, 0), mods::HIDDEN), graded(&s(95, 5, 0, 0), mods::HIDDEN)), ("SSH", "SH"));
        assert_eq!(graded(&s(85, 15, 0, 0), mods::HIDDEN), "A");
    }

    fn results() -> Results {
        Results {
            outcome: Outcome {
                summary: s(1, 0, 0, 0),
                hit_mean_ms: 0.0,
                hit_sd_ms: 0.0,
                hit_count: 0,
                verified: None,
                replay_saved: false,
                replay_path: None,
                unix_time: 0,
            },
            title: String::new(),
            creator: String::new(),
            player: String::new(),
            played: date_time(0),
            mods: 0,
            map_speed: 1.0,
            recorded: Vec::new(),
            watch_available: true,
            retry_drops_mods: false,
            stable_frames: None,
            end_time: 0,
            shown: 0.0,
            back_hover: 0.0,
        }
    }

    fn input(x: f32, y: f32, pressed: bool, keys: &[u32]) -> Input<'_> {
        Input { x, y, pressed, down: pressed, scroll: 0.0, keys, text: "", dt: 0.05, now_ns: 0 }
    }

    fn rendered(r: &Results, m: &Metrics, skin: &Skin) -> Vec<rsu_gfx::Inst> {
        let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 8192];
        // SAFETY: Batch writes into store, and only the written prefix is read.
        let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
        r.draw(&mut batch, m, skin, &Backdrop::default(), &input(-1.0, -1.0, false, &[]));
        unsafe { std::slice::from_raw_parts(store.as_ptr().cast(), batch.len()) }.to_vec()
    }

    #[test]
    fn results_render_speed_without_inventing_a_spread_for_zero_or_one_hit() {
        let (m, skin) = (Metrics::new(1280.0, 720.0), Skin::default());
        let mut r = results();
        let snapshot = |r: &Results| format!("{:?}", rendered(r, &m, &skin));
        let normal = snapshot(&r);
        r.map_speed = 1.25;
        let faster = snapshot(&r);
        assert!(normal != faster, "speed is shown even on a saved score without timing data");
        r.outcome.hit_sd_ms = 25.0;
        assert!(faster == snapshot(&r), "without hits there is no UR");
        r.outcome.hit_count = 1;
        r.outcome.hit_mean_ms = -12.0;
        let rendered_once = snapshot(&r);
        r.outcome.hit_sd_ms = 0.0;
        assert!(rendered_once == snapshot(&r), "one hit cannot provide a UR");
    }

    #[test]
    fn unstable_rate_uses_measured_population_spread_in_map_time() {
        let mut r = results();
        assert_eq!(r.unstable_rate(), None, "saved replay without timing data");
        let mut hits = rsu_telemetry::Running::default();
        for error in [-10.0, 10.0] {
            hits.push(error);
        }
        r.outcome.hit_count = hits.n();
        r.outcome.hit_sd_ms = hits.stddev();
        r.map_speed = 1.5;
        assert!((r.unstable_rate().unwrap() - 100.0).abs() < 1e-9);
        r.outcome.hit_sd_ms = 0.0;
        assert_eq!(r.unstable_rate(), Some(0.0), "measured zero is not missing data");
        r.outcome.hit_sd_ms = f64::NAN;
        assert_eq!(r.unstable_rate(), None);
    }

    #[test]
    fn missing_saved_replay_cannot_be_watched() {
        let (m, skin) = (Metrics::new(1280.0, 720.0), Skin::default());
        let mut r = results();
        r.outcome.replay_path = Some(std::path::PathBuf::from("missing.osr"));
        r.watch_available = false;
        assert_eq!(r.update(&input(0.0, 0.0, false, &[KEY_W]), &m, &skin), ResultsAction::None);
        let slot = Results::buttons(&m)[1];
        assert_eq!(r.update(&input(slot.x + 5.0, slot.y + 5.0, true, &[]), &m, &skin), ResultsAction::None);
    }

    #[test]
    fn unsaved_spun_out_result_cannot_offer_a_misleading_replay() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut r = results();
        r.mods = 4096;
        assert_eq!(r.update(&input(0.0, 0.0, false, &[KEY_W]), &m, &skin), ResultsAction::None);
        let [_, slot] = Results::buttons(&m);
        assert_eq!(r.update(&input(slot.x + 5.0, slot.y + 5.0, true, &[]), &m, &skin), ResultsAction::None);
        r.outcome.replay_saved = true;
        assert_eq!(r.update(&input(0.0, 0.0, false, &[KEY_W]), &m, &skin), ResultsAction::Watch);
    }

    #[test]
    fn watch_replay_draws_and_clicks_the_same_skin_rectangle() {
        let mut r = results();
        let mut skin = Skin::default();
        let sprite = Sprite { w: 80.0, h: 40.0, uv: [10, 20, 30, 40], page: rsu_gfx::PAGE_UI };
        for (w, h) in [(1920.0, 1080.0), (1280.0, 720.0), (400.0, 720.0)] {
            let m = Metrics::new(w, h);
            let [_, slot] = Results::buttons(&m);
            skin.menu.ranking_replay = Some(sprite);
            let items = rendered(&r, &m, &skin);
            let image = items.iter().find(|item| item.uv == sprite.uv).expect("replay image drawn");
            let [x, y] = image.center;
            assert!((image.half[0] / image.half[1] - 2.0).abs() < 1e-5);
            assert!(slot.contains(x - image.half[0] + 0.01, y - image.half[1] + 0.01));
            assert!(slot.contains(x + image.half[0] - 0.01, y + image.half[1] - 0.01));
            assert_eq!(r.update(&input(x, y, true, &[]), &m, &skin), ResultsAction::Watch);
            let empty = input(slot.x + 1.0, y, true, &[]);
            assert_eq!(r.update(&empty, &m, &skin), ResultsAction::None);
            assert_eq!(r.update(&input(-1.0, -1.0, false, &[KEY_W]), &m, &skin), ResultsAction::Watch);

            skin.menu.ranking_replay = Some(Sprite::default());
            assert!(rendered(&r, &m, &skin).iter().any(|item| item.color == [60, 90, 150, 235]));
            assert_eq!(r.update(&empty, &m, &skin), ResultsAction::Watch);
        }
    }

    #[test]
    fn keys_and_buttons_lead_on() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut r = results();
        assert_eq!(r.update(&input(0.0, 0.0, false, &[KEY_R]), &m, &skin), ResultsAction::Retry);
        assert_eq!(r.update(&input(0.0, 0.0, false, &[KEY_W]), &m, &skin), ResultsAction::Watch);
        assert_eq!(r.update(&input(0.0, 0.0, false, &[ui::KEY_ESC]), &m, &skin), ResultsAction::Back);
        let [retry, watch] = Results::buttons(&m);
        assert!(retry.y < watch.y && watch.y + watch.h < 1080.0);
        assert_eq!(r.update(&input(retry.x + 5.0, retry.y + 5.0, true, &[]), &m, &skin), ResultsAction::Retry);
        assert_eq!(r.update(&input(watch.x + 5.0, watch.y + 5.0, true, &[]), &m, &skin), ResultsAction::Watch);
        assert_eq!(r.update(&input(5.0, 1075.0, true, &[]), &m, &skin), ResultsAction::Back);
        assert_eq!(r.played.len(), 16);
    }
}
