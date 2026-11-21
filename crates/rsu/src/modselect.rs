//! The mod selection, laid out as McOsu's grid (`OsuModSelector`). The
//! mods rsu does not have yet keep their places and are shown faintly.

use crate::skin::{Skin, MOD_IMAGES};
use crate::text::StackStr;
use crate::ui::{self, Input, Metrics, Rect};
use std::fmt::Write;
use rsu_core::mods;
use rsu_formats::osu::PracticeDifficulty;
use rsu_gfx::Batch;

/// The bit osu! gives the autoplay mod; here it only marks the button.
const AUTOPLAY: u32 = 2048;

/// A place in the grid: the mod's bit, its short name, the key McOsu binds
/// to it, and which kind it is (0 easier, 1 harder, 2 special).
struct Cell {
    bit: u32,
    name: &'static str,
    key: u32,
    kind: usize,
    column: usize,
    row: usize,
}

const fn cell(bit: u32, name: &'static str, key: u32, kind: usize, column: usize, row: usize) -> Cell {
    Cell { bit, name, key, kind, column, row }
}

const GRID: [Cell; 12] = [
    cell(2, "EZ", 16, 0, 0, 0),
    cell(1, "NF", 17, 0, 1, 0),
    cell(256, "HT", 18, 0, 2, 0),
    cell(16, "HR", 30, 1, 0, 1),
    cell(32, "SD", 31, 1, 1, 1),
    cell(64, "DT", 32, 1, 2, 1),
    cell(8, "HD", 33, 1, 3, 1),
    cell(1024, "FL", 34, 1, 4, 1),
    cell(128, "RX", 44, 2, 0, 2),
    cell(8192, "AP", 45, 2, 1, 2),
    cell(4096, "SO", 46, 2, 2, 2),
    cell(AUTOPLAY, "AT", 47, 2, 3, 2),
];
/// The mods that do something in rsu.
const AVAILABLE: u32 = mods::NO_FAIL | mods::EASY | mods::HARD_ROCK | mods::HIDDEN | mods::HALF_TIME | mods::DOUBLE_TIME | mods::NIGHTCORE | mods::RELAX | mods::SUDDEN_DEATH | mods::SPUN_OUT | AUTOPLAY;
const COLOURS: [[u8; 3]; 3] = [[90, 190, 70], [220, 70, 70], [70, 150, 235]];
const KEY_1: u32 = 2;
const KEY_2: u32 = 3;

/// The speed slider's far end, in percent (McOsu: 2.5x); its far left is
/// "off", which leaves the speed to the mods.
const SLIDER_MAX_PCT: f32 = mods::RATE_MAX_PCT as f32;
/// What a drag or a wheel notch moves the rate by, in percent.
const RATE_STEP: i32 = 5;
const PRACTICE_ROWS: [(&str, f32, f32); 4] = [
    ("CS", -1.0, PracticeDifficulty::CS_MAX),
    ("AR", -1.0, 10.0),
    ("OD", -1.0, 10.0),
    ("HP", -1.0, PracticeDifficulty::HP_MAX),
];

pub struct ModSelect {
    shown: f32,
    grow: [f32; 12],
    /// The speed slider is being dragged.
    dragging: bool,
    practice_drag: Option<usize>,
    experimental: bool,
}

impl ModSelect {
    pub fn new() -> ModSelect {
        ModSelect { shown: 0.0, grow: [0.0; 12], dragging: false, practice_drag: None, experimental: false }
    }

    fn experimental_panel(m: &Metrics) -> Rect {
        Rect::new(0.0, 0.0, m.osu(300.0).min(m.w * 0.3), m.h)
    }

    fn experimental_flip(m: &Metrics) -> Rect {
        let panel = Self::experimental_panel(m);
        Rect::new(m.osu(10.0), m.h * 0.075, panel.w - m.osu(20.0), m.h * 0.05)
    }

    fn experimental_flip_up_down(m: &Metrics) -> Rect {
        let mut row = Self::experimental_flip(m);
        row.y += m.h * 0.06;
        row
    }

    fn experimental_fading_cursor(m: &Metrics) -> Rect {
        let mut row = Self::experimental_flip(m);
        row.y += m.h * 0.12;
        row
    }

    fn experimental_no_spinners(m: &Metrics) -> Rect {
        let mut row = Self::experimental_flip(m);
        row.y += m.h * 0.18;
        row
    }

    fn experimental_no_50s(m: &Metrics) -> Rect {
        let mut row = Self::experimental_flip(m);
        row.y += m.h * 0.24;
        row
    }

    pub fn hide_experimental(&mut self) { self.experimental = false; }

    pub fn slider_captured(&self) -> bool { self.dragging || self.practice_drag.is_some() }

    pub fn update_experimental(&mut self, input: &Input, m: &Metrics, practice: &mut PracticeDifficulty) -> bool {
        let panel = Self::experimental_panel(m);
        if !self.experimental && input.x >= 0.0 && input.x < m.w * 0.05 && input.y >= 0.0 && input.y < m.h {
            self.experimental = true;
        }
        if self.experimental && input.clicked(Self::experimental_flip(m)) {
            practice.flip_left_right = !practice.flip_left_right;
        }
        if self.experimental && input.clicked(Self::experimental_flip_up_down(m)) {
            practice.flip_up_down = !practice.flip_up_down;
        }
        if self.experimental && input.clicked(Self::experimental_fading_cursor(m)) {
            practice.fading_cursor = !practice.fading_cursor;
        }
        if self.experimental && input.clicked(Self::experimental_no_spinners(m)) {
            practice.no_spinners = !practice.no_spinners;
        }
        if self.experimental && input.clicked(Self::experimental_no_50s(m)) {
            practice.no_50s = !practice.no_50s;
        }
        if self.experimental && !input.over(panel) {
            self.experimental = false;
        }
        self.experimental && input.over(panel)
    }

    pub fn draw_experimental(&self, batch: &mut Batch, m: &Metrics, input: &Input, practice: PracticeDifficulty) {
        if !self.experimental {
            let tab = Rect::new(0.0, m.h * 0.45, (m.w * 0.05).max(1.0), m.h * 0.1);
            ui::button(batch, tab, [70, 70, 80], "EXP", m.px(ui::FONT_SUBTITLE) * 0.45, input.over(tab));
            return;
        }
        let panel = Self::experimental_panel(m);
        batch.rect(panel.x, panel.y, panel.w, panel.h, [24, 24, 28, 245]);
        let px = (m.px(ui::FONT_SUBTITLE) * 0.75).min(panel.w / 15.0);
        ui::text(batch, m.osu(10.0), m.osu(14.0), px, ui::WHITE, "Experimental Mods (!)");
        let row = Self::experimental_flip(m);
        let label = if practice.flip_left_right { "[X] Flip Left/Right" } else { "[ ] Flip Left/Right" };
        ui::button(batch, row, if practice.flip_left_right { [65, 120, 175] } else { [70, 70, 80] }, label, px, input.over(row));
        let row = Self::experimental_flip_up_down(m);
        let label = if practice.flip_up_down { "[X] Flip Up/Down" } else { "[ ] Flip Up/Down" };
        ui::button(batch, row, if practice.flip_up_down { [65, 120, 175] } else { [70, 70, 80] }, label, px, input.over(row));
        let row = Self::experimental_fading_cursor(m);
        let label = if practice.fading_cursor { "[X] Fading Cursor" } else { "[ ] Fading Cursor" };
        ui::button(batch, row, if practice.fading_cursor { [65, 120, 175] } else { [70, 70, 80] }, label, px, input.over(row));
        let row = Self::experimental_no_spinners(m);
        let label = if practice.no_spinners { "[X] No Spinners" } else { "[ ] No Spinners" };
        ui::button(batch, row, if practice.no_spinners { [65, 120, 175] } else { [70, 70, 80] }, label, px, input.over(row));
        let row = Self::experimental_no_50s(m);
        let label = if practice.no_50s { "[X] No 50s" } else { "[ ] No 50s" };
        ui::button(batch, row, if practice.no_50s { [65, 120, 175] } else { [70, 70, 80] }, label, px, input.over(row));
        ui::text(batch, m.osu(10.0), row.y + row.h + m.osu(12.0), px * 0.8, [255, 200, 80, 255], "Results and replay not saved");
    }

    /// An override row above the mod grid: name, slider, then value.
    pub(crate) fn override_row(m: &Metrics, skin: &Skin, index: usize) -> Rect {
        let top = Self::cells(m, skin)[0].y;
        let unit = m.h / 768.0;
        let height = 25.0 * unit;
        let gap = ((top - 5.0 * height) / 4.0) * 0.35;
        let start = top * 0.5 - (5.0 * height + 4.0 * gap) / 1.75;
        let width = (250.0 * unit).min(m.w * 0.45);
        Rect::new((m.w - width) * 0.5, start + index as f32 * (height + gap), width, height)
    }

    fn lock_button(m: &Metrics, skin: &Skin, i: usize) -> Rect {
        let row = Self::override_row(m, skin, i);
        let x = (row.x - m.osu(150.0)).max(0.0);
        Rect::new(x, row.y, row.x - x - m.osu(10.0), row.h)
    }

    fn speed_row(m: &Metrics, skin: &Skin) -> Rect { Self::override_row(m, skin, 4) }
    fn speed_slider(row: Rect) -> Rect { row }

    fn practice_mouse(&mut self, input: &Input, m: &Metrics, skin: &Skin, practice: &mut PracticeDifficulty) {
        if input.pressed {
            self.practice_drag = (0..PRACTICE_ROWS.len()).find(|&i| input.over(Self::override_row(m, skin, i)));
        }
        if !input.down { self.practice_drag = None; }
        if let Some(i) = self.practice_drag {
            let (_, min, max) = PRACTICE_ROWS[i];
            let fraction = ui::slider_value_at(Self::override_row(m, skin, i), input.x);
            let value = ((min + fraction * (max - min)) * 10.0).round() / 10.0;
            *[&mut practice.cs, &mut practice.ar, &mut practice.od, &mut practice.hp][i] = (value >= 0.0).then_some(value);
            if fraction == 0.0 {
                if i == 1 { practice.ar_lock = false; }
                if i == 2 { practice.od_lock = false; }
            }
        }
    }

    pub fn update_practice(
        &mut self,
        input: &Input,
        m: &Metrics,
        skin: &Skin,
        practice: &mut PracticeDifficulty,
        base: Option<rsu_core::Difficulty>,
    ) {
        if input.clicked(Self::actions(m, skin)[0]) || input.keys.iter().any(|&key| matches!(key, KEY_1 | ui::KEY_BACKSPACE)) {
            *practice = PracticeDifficulty::default();
            self.practice_drag = None;
            return;
        }
        self.practice_mouse(input, m, skin, practice);
        for (row, locked, value, fallback) in [
            (1, &mut practice.ar_lock, &mut practice.ar, base.map(|d| d.ar)),
            (2, &mut practice.od_lock, &mut practice.od, base.map(|d| d.od)),
        ] {
            if input.clicked(Self::lock_button(m, skin, row)) {
                *locked = !*locked;
                if *locked && value.is_none() {
                    *value = fallback;
                }
            }
        }
        practice_od_input(input, m, skin, &mut practice.od);
        practice_ar_input(input, m, skin, &mut practice.ar);
    }

    /// The rate for a slider position: far left is off.
    fn rate_at(fraction: f32) -> u16 {
        let pct = (fraction * SLIDER_MAX_PCT / RATE_STEP as f32).round() as i32 * RATE_STEP;
        if pct < mods::RATE_MIN_PCT as i32 {
            0
        } else {
            pct.min(mods::RATE_MAX_PCT as i32) as u16
        }
    }

    fn cell_size(m: &Metrics, skin: &Skin) -> (f32, f32) {
        let unit = m.h / 768.0;
        let any = skin.menu.mods.iter().flatten().find(|s| s.visible());
        let (w, h) = any.map_or((68.0 * unit, 66.0 * unit), |s| (s.w * unit, s.h * unit));
        // A skin with big mod images must not push the speed slider and the
        // buttons below the grid off the screen.
        let fit = (m.h * 0.088 / h).min(1.0);
        (w * fit, h * fit)
    }

    fn cells(m: &Metrics, skin: &Skin) -> [Rect; 12] {
        let (w, h) = Self::cell_size(m, skin);
        let (gap_x, gap_y) = (w, h * 0.25);
        let x0 = m.w * 0.5 - (w * 6.0 + gap_x * 5.0) * 0.5;
        let y0 = m.h * 0.5 - (h * 3.0 + gap_y * 2.0) * 0.5;
        std::array::from_fn(|i| {
            let c = &GRID[i];
            Rect::new(x0 + c.column as f32 * (w + gap_x), y0 + c.row as f32 * (h + gap_y), w, h)
        })
    }

    /// Reset and close, below the grid.
    fn actions(m: &Metrics, skin: &Skin) -> [Rect; 2] {
        let grid = Self::cells(m, skin);
        let bottom = grid[11].y + grid[11].h;
        let (w, h) = (m.osu(448.0).min(m.w * 0.9), grid[0].h * 0.75);
        let gap = h * 0.5;
        let y0 = bottom + (m.h - bottom) * 0.5 - (2.0 * h + gap) * 0.5;
        [0.0, 1.0].map(|i| Rect::new(m.w * 0.5 - w * 0.5, y0 + i * (h + gap), w, h))
    }

    /// Switch one mod; the others stay, except the one it excludes
    /// (double time and half time, easy and hard rock).
    fn toggle(bit: u32, mods: &mut u32, auto: &mut bool) {
        const SPEED: u32 = mods::DOUBLE_TIME | mods::NIGHTCORE | mods::HALF_TIME;
        match bit {
            AUTOPLAY => {
                *auto = !*auto;
                if *auto { *mods &= !mods::SPUN_OUT; }
            }
            mods::SPUN_OUT => {
                *mods ^= mods::SPUN_OUT;
                if *mods & mods::SPUN_OUT != 0 { *auto = false; }
            }
            mods::SUDDEN_DEATH => {
                let pair = mods::SUDDEN_DEATH | mods::PERFECT;
                let next = match *mods & pair { 0 => mods::SUDDEN_DEATH, mods::SUDDEN_DEATH => pair, _ => 0 };
                *mods = (*mods & !(pair | mods::NO_FAIL)) | next;
            }
            mods::NO_FAIL => *mods = (*mods & !(mods::SUDDEN_DEATH | mods::PERFECT)) ^ mods::NO_FAIL,
            // Double time goes on to nightcore before it goes off.
            mods::DOUBLE_TIME => {
                *mods = (*mods & !SPEED)
                    | match *mods & (mods::DOUBLE_TIME | mods::NIGHTCORE) {
                        0 => mods::DOUBLE_TIME,
                        mods::DOUBLE_TIME => mods::DOUBLE_TIME | mods::NIGHTCORE,
                        _ => 0,
                    }
            }
            mods::HALF_TIME => *mods = (*mods & !SPEED) | (!*mods & mods::HALF_TIME),
            mods::EASY => *mods = (*mods & !mods::HARD_ROCK) ^ mods::EASY,
            mods::HARD_ROCK => *mods = (*mods & !mods::EASY) ^ mods::HARD_ROCK,
            _ => *mods ^= bit,
        }
    }

    fn is_on(bit: u32, mods: u32, auto: bool) -> bool {
        if bit == AUTOPLAY {
            auto
        } else {
            mods & bit != 0
        }
    }

    /// Returns true when the selection should close.
    pub fn update(
        &mut self,
        input: &Input,
        m: &Metrics,
        skin: &Skin,
        mods: &mut u32,
        auto: &mut bool,
        rate_pct: &mut u16,
    ) -> bool {
        ui::approach(&mut self.shown, 1.0, input.dt, 0.2);
        let mut close = false;
        let row = Self::speed_row(m, skin);
        let slider = Self::speed_slider(row);
        if input.pressed && input.over(slider) {
            self.dragging = true;
        }
        self.dragging &= input.down;
        if self.dragging {
            *rate_pct = Self::rate_at(ui::slider_value_at(slider, input.x));
        } else if input.over(row) && input.scroll != 0.0 {
            // The wheel nudges the rate, up on a turn away from the user.
            let from = if *rate_pct == 0 { (mods::speed(*mods) * 100.0).round() as i32 } else { *rate_pct as i32 };
            let to = (from - input.scroll.signum() as i32 * RATE_STEP).clamp(mods::RATE_MIN_PCT as i32, SLIDER_MAX_PCT as i32);
            *rate_pct = to as u16;
        }
        for (i, r) in Self::cells(m, skin).into_iter().enumerate() {
            let c = &GRID[i];
            if AVAILABLE & c.bit != 0 && input.clicked(r) {
                Self::toggle(c.bit, mods, auto);
            }
            ui::approach(&mut self.grow[i], Self::is_on(c.bit, *mods, *auto) as u8 as f32, input.dt, 0.12);
        }
        let [reset, done] = Self::actions(m, skin);
        if input.clicked(reset) {
            (*mods, *auto, *rate_pct) = (0, false, 0);
        }
        close |= input.clicked(done);
        for &code in input.keys {
            match code {
                ui::KEY_ESC | ui::KEY_F1 | KEY_2 | ui::KEY_ENTER => close = true,
                KEY_1 => (*mods, *auto, *rate_pct) = (0, false, 0),
                _ => {
                    if let Some(c) = GRID.iter().find(|c| c.key == code && AVAILABLE & c.bit != 0) {
                        Self::toggle(c.bit, mods, auto);
                    }
                }
            }
        }
        close
    }

    pub fn draw(&self, batch: &mut Batch, m: &Metrics, skin: &Skin, input: &Input, mods: u32, auto: bool, rate_pct: u16) {
        let a = self.shown;
        batch.rect(0.0, 0.0, m.w, m.h, [0, 0, 0, (a * 215.0) as u8]);
        let px = m.px(ui::FONT_SUBTITLE);
        for (i, r) in Self::cells(m, skin).into_iter().enumerate() {
            let c = &GRID[i];
            let available = AVAILABLE & c.bit != 0;
            let on = Self::is_on(c.bit, mods, auto);
            // Nightcore takes double time's place while it is on.
            let nightcore = c.bit == mods::DOUBLE_TIME && mods & mods::NIGHTCORE != 0;
            let perfect = c.bit == mods::SUDDEN_DEATH && mods & mods::PERFECT != 0;
            let image_bit = if nightcore { mods::NIGHTCORE } else if perfect { mods::PERFECT } else { c.bit };
            let image = MOD_IMAGES.iter().position(|(bit, _)| *bit == image_bit).and_then(|n| skin.menu.mods[n]);
            let (cx, cy) = r.centre();
            let grow = 1.0 + 0.2 * self.grow[i];
            let turn = 0.13 * self.grow[i];
            let fade = a * if available { 1.0 } else { 0.22 };
            match image.filter(|s| s.visible()) {
                Some(s) => s.draw_turned(batch, cx, cy, m.h / 768.0 * grow, ui::alpha(ui::WHITE, fade), turn, 0),
                None if image.is_some() => {}
                None => {
                    let (w, h) = (r.w * grow, r.h * grow);
                    let colour = COLOURS[c.kind];
                    let lit = if on { 1.0 } else { 0.55 };
                    batch.rrect(
                        cx - w * 0.5,
                        cy - h * 0.5,
                        w,
                        h,
                        m.px(4.0),
                        ui::rgb(colour.map(|v| (v as f32 * lit) as u8), fade),
                    );
                    let edge = if on { ui::WHITE } else { [255, 255, 255, 90] };
                    batch.frame(cx - w * 0.5, cy - h * 0.5, w, h, m.px(4.0), (h * 0.05).max(1.0), ui::alpha(edge, fade));
                    let name = if nightcore { "NC" } else if perfect { "PF" } else { c.name };
                    ui::centred(batch, cx, cy, ui::em_for(h * 0.34), ui::alpha(ui::WHITE, fade), name);
                }
            }
            if !available && input.over(r) {
                ui::centred(
                    batch,
                    cx,
                    r.y + r.h + ui::cap(px * 0.6),
                    px * 0.6,
                    [200, 200, 200, 255],
                    "not in rsu yet",
                );
            }
        }
        let grid = Self::cells(m, skin);
        let [reset, done] = Self::actions(m, skin);
        // How the score is multiplied, between the grid and the buttons.
        let mut line = StackStr::<48>::new();
        let multiplier = mods::score_ppm(mods) as f64 / 1e6 * if mods & mods::SPUN_OUT != 0 { 0.9 } else { 1.0 };
        let _ = write!(line, "Score Multiplier: {:.2}X", multiplier);
        let bottom = grid[11].y + grid[11].h;
        ui::centred(batch, m.w * 0.5, bottom + (reset.y - bottom) * 0.28, px, ui::alpha(ui::WHITE, a), line.as_str());
        if mods & mods::SPUN_OUT != 0 {
            ui::centred(batch, m.w * 0.5, bottom + (reset.y - bottom) * 0.72, px * 0.65, ui::alpha([255, 200, 80, 255], a), "SO: result and replay not saved");
        }
        // The speed: its own rate when set, else what the mods give.
        let row = Self::speed_row(m, skin);
        let slider = Self::speed_slider(row);
        let speed = mods::speed_with(mods, rate_pct);
        let lit = if rate_pct != 0 { ui::WHITE } else { [160, 160, 170, 255] };
        ui::text(batch, row.x - m.osu(210.0), row.y + (row.h - ui::cap(px)) * 0.5, px * 0.75, ui::alpha(lit, a), "Speed/BPM Multiplier");
        let value = if rate_pct == 0 { 0.0 } else { rate_pct as f32 / SLIDER_MAX_PCT };
        ui::slider(batch, input, slider, value, self.dragging);
        let mut shown = StackStr::<24>::new();
        let _ = write!(shown, "{}x", (speed * 100.0).round() / 100.0);
        ui::text(batch, row.x + row.w + m.osu(10.0), row.y + (row.h - ui::cap(px)) * 0.5, px, ui::alpha(lit, a), shown.as_str());
        let label_px = ui::em_for(reset.h * 0.3).min(px * 1.2);
        ui::button(batch, reset, [150, 40, 40], "1. Reset All Mods", label_px, input.over(reset));
        ui::button(batch, done, [70, 70, 80], "2. Close", label_px, input.over(done));

    }
}

/// Keyboard adjustment is deliberately separate from speed and mods.
fn practice_od_input(input: &Input, m: &Metrics, skin: &Skin, od: &mut Option<f32>) {
    if input.clicked(ModSelect::actions(m, skin)[0]) { *od = None; }
    for &key in input.keys {
        match key {
            ui::KEY_LEFT => *od = Some((od.unwrap_or(5.0) - 0.5).max(0.0)),
            ui::KEY_RIGHT => *od = Some((od.unwrap_or(5.0) + 0.5).min(10.0)),
            ui::KEY_BACKSPACE | KEY_1 => *od = None,
            _ => {}
        }
    }
}

fn practice_ar_input(input: &Input, m: &Metrics, skin: &Skin, ar: &mut Option<f32>) {
    if input.clicked(ModSelect::actions(m, skin)[0]) { *ar = None; }
    for &key in input.keys {
        match key {
            ui::KEY_DOWN => *ar = Some((ar.unwrap_or(5.0) - 0.5).max(0.0)),
            ui::KEY_UP => *ar = Some((ar.unwrap_or(5.0) + 0.5).min(10.0)),
            ui::KEY_BACKSPACE | KEY_1 => *ar = None,
            _ => {}
        }
    }
}

impl ModSelect {
    pub fn draw_practice(&self, batch: &mut Batch, m: &Metrics, skin: &Skin, input: &Input, practice: PracticeDifficulty) {
        let px = m.px(ui::FONT_SUBTITLE) * 0.75;
        let values = [practice.cs, practice.ar, practice.od, practice.hp];
        for (i, ((name, min, max), value)) in PRACTICE_ROWS.into_iter().zip(values).enumerate() {
            let row = Self::override_row(m, skin, i);
            let colour = if value.is_some() { ui::WHITE } else { [160, 160, 170, 255] };
            let y = row.y + (row.h - ui::cap(px)) * 0.5;
            let lock = match i {
                1 => Some((practice.ar_lock, "AR LOCK: ON", "AR LOCK: OFF")),
                2 => Some((practice.od_lock, "OD LOCK: ON", "OD LOCK: OFF")),
                _ => None,
            };
            if let Some((locked, on, off)) = lock {
                let button = Self::lock_button(m, skin, i);
                let label = if locked { on } else { off };
                let label_px = px.min(px * (button.w * 0.90) / crate::text::width(px, label));
                let colour = if locked { [65, 120, 175] } else { [70, 70, 80] };
                ui::button(batch, button, colour, label, label_px, input.over(button));
            } else {
                ui::text(batch, row.x - m.osu(150.0), y, px, colour, name);
            }
            let fraction = (value.unwrap_or(min) - min) / (max - min);
            ui::slider(batch, input, row, fraction, self.practice_drag == Some(i));
            let mut text = StackStr::<16>::new();
            let shown = if let Some(v) = value {
                let _ = write!(text, "{v:.1}");
                text.as_str()
            } else { "off" };
            ui::text(batch, row.x + row.w + m.osu(10.0), y, px, colour, shown);
        }
        if practice.is_active() {
            ui::centred(batch, m.w * 0.5, Self::cells(m, skin)[0].y - m.osu(12.0), px, ui::WHITE, "PRACTICE - NOT SAVED (Backspace: off)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(x: f32, y: f32, pressed: bool, keys: &[u32]) -> Input<'_> {
        Input { x, y, pressed, down: pressed, scroll: 0.0, keys, text: "", dt: 0.05, now_ns: 0 }
    }

    #[test]
    fn spun_out_key_and_pointer_toggle_and_exclude_autoplay() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut selector = ModSelect::new();
        let (mut mods, mut auto, mut rate) = (0, true, 0);
        selector.update(&input(0.0, 0.0, false, &[46]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!(mods & rsu_core::mods::SPUN_OUT, rsu_core::mods::SPUN_OUT);
        assert!(!auto);
        let r = ModSelect::cells(&m, &skin)[10];
        selector.update(&input(r.x + r.w / 2.0, r.y + r.h / 2.0, true, &[]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!(mods & rsu_core::mods::SPUN_OUT, 0);
        selector.update(&input(0.0, 0.0, false, &[46]), &m, &skin, &mut mods, &mut auto, &mut rate);
        selector.update(&input(0.0, 0.0, false, &[47]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert!(auto);
        assert_eq!(mods & rsu_core::mods::SPUN_OUT, 0);
    }

    #[test]
    fn practice_locks_toggle_independently_and_reset() {
        for (w, h) in [(1920.0, 1080.0), (800.0, 600.0)] {
            let (m, skin) = (Metrics::new(w, h), Skin::default());
            let mut select = ModSelect::new();
            let mut practice = PracticeDifficulty { od: Some(0.0), ..Default::default() };
            let base = Some(rsu_core::Difficulty { cs: 4.0, ar: 7.0, od: 6.0, hp: 5.0 });
            let mut click = |i| {
                let (x, y) = ModSelect::lock_button(&m, &skin, i).centre();
                select.update_practice(&input(x, y, true, &[]), &m, &skin, &mut practice, base);
                (practice.ar_lock, practice.od_lock, practice.ar, practice.od)
            };
            assert_eq!(click(1), (true, false, Some(7.0), Some(0.0)));
            assert_eq!(click(2), (true, true, Some(7.0), Some(0.0)));
            assert_eq!(click(1), (false, true, Some(7.0), Some(0.0)));
            select.update_practice(&input(0.0, 0.0, false, &[ui::KEY_BACKSPACE]), &m, &skin, &mut practice, base);
            assert_eq!(practice, PracticeDifficulty::default());
        }
    }

    #[test]
    fn ar_and_od_slider_off_clears_only_that_override_and_lock() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        for i in [1, 2] {
            let mut select = ModSelect::new();
            let mut p = PracticeDifficulty { ar: Some(0.0), od: Some(7.0), ar_lock: true, od_lock: true, ..Default::default() };
            let row = ModSelect::override_row(&m, &skin, i);
            select.update_practice(&input(row.x, row.y + row.h / 2.0, true, &[]), &m, &skin, &mut p, None);
            if i == 1 { assert_eq!((p.ar, p.ar_lock, p.od, p.od_lock), (None, false, Some(7.0), true)); }
            else { assert_eq!((p.od, p.od_lock, p.ar, p.ar_lock), (None, false, Some(0.0), true)); }
        }
    }

    #[test]
    fn mouse_drag_tracks_each_override_and_stops_on_release() {
        for (w, h) in [(1920.0, 1080.0), (800.0, 600.0)] {
            let (m, skin) = (Metrics::new(w, h), Skin::default());
            let mut select = ModSelect::new();
            let mut practice = PracticeDifficulty::default();
            for (i, (_, min, max)) in PRACTICE_ROWS.into_iter().enumerate() {
                let row = ModSelect::override_row(&m, &skin, i);
                let y = row.y + row.h / 2.0;
                let field = |p: PracticeDifficulty| [p.cs, p.ar, p.od, p.hp][i];
                select.practice_mouse(&input(row.x + row.w / 2.0, y, true, &[]), &m, &skin, &mut practice);
                assert_eq!(field(practice), Some(((min + max) * 5.0).round() / 10.0));
                let moving = Input { x: row.x + row.w + 100.0, y: y + 100.0, pressed: false, down: true, ..input(0.0, 0.0, false, &[]) };
                select.practice_mouse(&moving, &m, &skin, &mut practice);
                assert_eq!(field(practice), Some(max));
                select.practice_mouse(&input(row.x, y, false, &[]), &m, &skin, &mut practice);
                assert_eq!(field(practice), Some(max));
            }
        }
    }

    #[test]
    fn reset_button_clears_cs_and_hp_even_during_a_drag() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut select = ModSelect::new();
        let mut practice = PracticeDifficulty { cs: Some(7.0), hp: Some(8.0), ..Default::default() };
        let (x, y) = ModSelect::override_row(&m, &skin, 0).centre();
        select.update_practice(&input(x, y, true, &[]), &m, &skin, &mut practice, None);
        let (x, y) = ModSelect::actions(&m, &skin)[0].centre();
        select.update_practice(&input(x, y, true, &[]), &m, &skin, &mut practice, None);
        assert_eq!(practice, PracticeDifficulty::default());
        assert_eq!(select.practice_drag, None);
    }

    #[test]
    fn practice_label_does_not_overlap_mod_hint() {
        for (w, h) in [(1920.0, 1080.0), (800.0, 600.0)] {
            let (m, skin) = (Metrics::new(w, h), Skin::default());
            let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
            let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
            let mut select = ModSelect::new();
            select.shown = 1.0;
            select.draw(&mut batch, &m, &skin, &input(0.0, 0.0, false, &[]), 0, false, 0);
            let split = batch.len();
            select.draw_practice(&mut batch, &m, &skin, &input(0.0, 0.0, false, &[]), PracticeDifficulty { cs: Some(7.0), hp: Some(8.0), ..Default::default() });
            let shapes = unsafe { std::slice::from_raw_parts(store.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
            for hint in shapes[..split].iter().filter(|s| s.color == [255, 255, 255, 150]) {
                for label in &shapes[split..] {
                    assert!((hint.center[1] - label.center[1]).abs() >= hint.half[1] + label.half[1], "hint and practice label overlap");
                }
            }
        }
    }

    #[test]
    fn practice_od_keys_change_clamp_and_clear() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut od = None;
        practice_od_input(&input(0.0, 0.0, false, &[ui::KEY_RIGHT]), &m, &skin, &mut od);
        assert_eq!(od, Some(5.5));
        for _ in 0..30 { practice_od_input(&input(0.0, 0.0, false, &[ui::KEY_RIGHT]), &m, &skin, &mut od); }
        assert_eq!(od, Some(10.0));
        practice_od_input(&input(0.0, 0.0, false, &[ui::KEY_BACKSPACE]), &m, &skin, &mut od);
        assert_eq!(od, None);
    }

    #[test]
    fn practice_ar_keys_clamp_and_reset() {
        for (w, h) in [(1920.0, 1080.0), (800.0, 600.0)] {
            let (m, skin) = (Metrics::new(w, h), Skin::default());
            let mut ar = None;
            for _ in 0..30 { practice_ar_input(&input(0.0, 0.0, false, &[ui::KEY_UP]), &m, &skin, &mut ar); }
            assert_eq!(ar, Some(10.0));
            for _ in 0..30 { practice_ar_input(&input(0.0, 0.0, false, &[ui::KEY_DOWN]), &m, &skin, &mut ar); }
            assert_eq!(ar, Some(0.0));
            let (x, y) = ModSelect::actions(&m, &skin)[0].centre();
            practice_ar_input(&input(x, y, true, &[]), &m, &skin, &mut ar);
            assert_eq!(ar, None);
            ar = Some(7.0);
            practice_ar_input(&input(0.0, 0.0, false, &[ui::KEY_BACKSPACE]), &m, &skin, &mut ar);
            assert_eq!(ar, None);
        }
    }

    #[test]
    fn practice_od_reset_button_clears_override() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut od = Some(8.0);
        let (x, y) = ModSelect::actions(&m, &skin)[0].centre();
        practice_od_input(&input(x, y, true, &[]), &m, &skin, &mut od);
        assert_eq!(od, None);
    }

    #[test]
    fn only_what_rsu_has_can_be_switched() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut s = ModSelect::new();
        let (mut mods, mut auto) = (0, false);
        let mut rate = 0u16;
        let cells = ModSelect::cells(&m, &skin);
        let click = |s: &mut ModSelect, r: Rect, mods: &mut u32, auto: &mut bool| {
            let (cx, cy) = r.centre();
            s.update(&input(cx, cy, true, &[]), &m, &skin, mods, auto, &mut 0)
        };
        click(&mut s, cells[4], &mut mods, &mut auto);
        assert_eq!(mods, 32);
        click(&mut s, cells[4], &mut mods, &mut auto);
        assert_eq!(mods, 32 | 16384);
        click(&mut s, cells[4], &mut mods, &mut auto);
        assert_eq!(mods, 0);
        click(&mut s, cells[0], &mut mods, &mut auto);
        click(&mut s, cells[6], &mut mods, &mut auto);
        assert_eq!(mods, mods::EASY | mods::HIDDEN);
        click(&mut s, cells[3], &mut mods, &mut auto);
        assert_eq!(mods, mods::HARD_ROCK | mods::HIDDEN, "hard rock replaces easy");
        click(&mut s, cells[3], &mut mods, &mut auto);
        click(&mut s, cells[6], &mut mods, &mut auto);
        assert_eq!(mods, 0);
        click(&mut s, cells[5], &mut mods, &mut auto);
        assert_eq!(mods, mods::DOUBLE_TIME);
        click(&mut s, cells[5], &mut mods, &mut auto);
        assert_eq!(mods, mods::DOUBLE_TIME | mods::NIGHTCORE);
        click(&mut s, cells[2], &mut mods, &mut auto);
        assert_eq!(mods, mods::HALF_TIME, "half time replaces double time");
        click(&mut s, cells[11], &mut mods, &mut auto);
        assert!(auto);
        let [reset, done] = ModSelect::actions(&m, &skin);
        assert!(!click(&mut s, reset, &mut mods, &mut auto));
        assert_eq!((mods, auto), (0, false));
        assert!(click(&mut s, done, &mut mods, &mut auto));
        assert!(reset.y > cells[11].y + cells[11].h && done.y + done.h < 1080.0);
        // The keys McOsu gives them.
        assert!(!s.update(&input(0.0, 0.0, false, &[32, 32, 47]), &m, &skin, &mut mods, &mut auto, &mut rate));
        assert_eq!((mods, auto), (mods::DOUBLE_TIME | mods::NIGHTCORE, true));
        assert!(s.update(&input(0.0, 0.0, false, &[ui::KEY_ESC]), &m, &skin, &mut mods, &mut auto, &mut rate));
    }

    #[test]
    fn the_speed_slider_sets_a_rate_of_its_own() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut s = ModSelect::new();
        let (mut mods, mut auto, mut rate) = (mods::DOUBLE_TIME, false, 0u16);
        let slider = ModSelect::speed_slider(ModSelect::speed_row(&m, &skin));
        let at = |f: f32| {
            let block = slider.h * 0.8;
            slider.x + block * 0.5 + (slider.w - block) * f
        };
        let cy = slider.y + slider.h * 0.5;
        // Grab it and drag to the middle: half of 2.5x, in steps of 0.05.
        s.update(&input(at(0.5), cy, true, &[]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!(rate, 125);
        // Dragging on, even off the slider, until let go.
        s.update(&input(at(0.8), cy + 200.0, true, &[]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!(rate, 200);
        s.update(&input(at(0.0), cy, false, &[]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!(rate, 200, "let go: no longer follows");
        // The far left puts it back to the mods.
        s.update(&input(at(0.0), cy, true, &[]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!(rate, 0);
        // Reset clears it with the mods.
        rate = 130;
        s.update(&input(0.0, 0.0, false, &[KEY_1]), &m, &skin, &mut mods, &mut auto, &mut rate);
        assert_eq!((mods, rate), (0, 0));
    }

    #[test]
    fn the_speed_slider_has_room_with_big_skin_images() {
        let m = Metrics::new(1920.0, 1080.0);
        let skin = Skin::default();
        let row = ModSelect::speed_row(&m, &skin);
        let [reset, done] = ModSelect::actions(&m, &skin);
        let grid = ModSelect::cells(&m, &skin);
        assert!(row.h > 15.0 && row.y + row.h < grid[0].y && row.y < reset.y);
        assert!(done.y + done.h < m.h);
    }
}
