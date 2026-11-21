//! The pause menu: continue, retry or leave, as in McOsu (`OsuPauseMenu`).

use crate::skin::Skin;
use crate::ui::{self, Input, Metrics, Rect};
use rsu_gfx::Batch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseAction {
    None,
    Continue,
    Retry,
    Quit,
}

const ACTIONS: [PauseAction; 3] = [PauseAction::Continue, PauseAction::Retry, PauseAction::Quit];
const LABELS: [&str; 3] = ["Continue", "Retry", "Back to menu"];
const COLOURS: [[u8; 3]; 3] = [[60, 150, 60], [170, 130, 30], [170, 50, 50]];

pub struct PauseMenu {
    /// The play ended because the health ran out: no way to continue it.
    failed: bool,
    selected: Option<usize>,
    dim: f32,
    since_ns: u64,
}

impl PauseMenu {
    pub fn new(now_ns: u64) -> PauseMenu {
        PauseMenu { failed: false, selected: None, dim: 0.0, since_ns: now_ns }
    }

    /// The menu after a fail: retry or leave.
    pub fn failed(now_ns: u64) -> PauseMenu {
        PauseMenu { failed: true, selected: Some(1), ..PauseMenu::new(now_ns) }
    }

    fn buttons(&self, m: &Metrics, skin: &Skin) -> [Rect; 3] {
        // The images are made for a button 411 pixels wide that takes up
        // 256 of osu!'s; without a skin the buttons have those proportions.
        let scale = m.osu(256.0) / 411.0;
        let size = |i: usize| match skin.menu.pause[i].filter(|s| s.visible()) {
            Some(s) => (s.w * scale, s.h * scale),
            None => (m.osu(256.0), m.osu(256.0) * 0.26),
        };
        let (w, h) = (0..3).map(size).fold((0.0f32, 0.0f32), |a, b| (a.0.max(b.0), a.1.max(b.1)));
        let slot = m.h / 3.0;
        // The outer two are pulled towards the middle one.
        let pinch = (slot * 0.5 - h * 0.5).max(0.0);
        [0usize, 1, 2].map(|i| {
            let y = (i as f32 + 0.5) * slot - h * 0.5 + pinch * (1.0 - i as f32);
            Rect::new(m.w * 0.5 - w * 0.5, y, w, h)
        })
    }

    pub fn update(&mut self, input: &Input, m: &Metrics, skin: &Skin) -> PauseAction {
        ui::approach(&mut self.dim, 1.0, input.dt, 0.25);
        let buttons = self.buttons(m, skin);
        let mut action = PauseAction::None;
        for (i, r) in buttons.into_iter().enumerate() {
            if input.over(r) && !(self.failed && i == 0) {
                // The pointer picks a button only when it moves onto it, so
                // that the keys can pick another while it rests there.
                if input.pressed {
                    action = ACTIONS[i];
                }
                if input.pressed || self.selected.is_none() {
                    self.selected = Some(i);
                }
            }
        }
        for &code in input.keys {
            match code {
                ui::KEY_ESC => action = if self.failed { PauseAction::Quit } else { PauseAction::Continue },
                ui::KEY_UP if self.failed => self.selected = Some(if self.selected == Some(2) { 1 } else { 2 }),
                ui::KEY_DOWN | ui::KEY_TAB if self.failed => self.selected = Some(if self.selected == Some(1) { 2 } else { 1 }),
                ui::KEY_UP => self.selected = Some(self.selected.map_or(2, |s| (s + 2) % 3)),
                ui::KEY_DOWN | ui::KEY_TAB => self.selected = Some(self.selected.map_or(0, |s| (s + 1) % 3)),
                ui::KEY_ENTER | ui::KEY_SPACE => {
                    if let Some(s) = self.selected {
                        action = ACTIONS[s];
                    }
                }
                _ => {}
            }
        }
        action
    }

    pub fn draw(&self, batch: &mut Batch, m: &Metrics, skin: &Skin, input: &Input) {
        batch.rect(0.0, 0.0, m.w, m.h, [20, 20, 20, (self.dim * 0.58 * 255.0) as u8]);
        if let Some(overlay) = skin.menu.pause_overlay.filter(|s| s.visible()) {
            let scale = (m.w / overlay.w).max(m.h / overlay.h);
            let (w, h) = (overlay.w * scale, overlay.h * scale);
            overlay.draw_in(batch, (m.w - w) * 0.5, (m.h - h) * 0.5, w, h, ui::alpha(ui::WHITE, self.dim));
        }
        let a = 1.0 - (1.0 - self.dim).powi(3);
        if self.failed {
            ui::centred(batch, m.w * 0.5, m.h * 0.1, ui::em_for(m.h * 0.06), ui::alpha([235, 70, 70, 255], a), "You failed");
        }
        let buttons = self.buttons(m, skin);
        for (i, r) in buttons.into_iter().enumerate() {
            let hot = (input.over(r) && !(self.failed && i == 0)) || self.selected == Some(i);
            let a = if self.failed && i == 0 { a * 0.3 } else { a };
            match skin.menu.pause[i].filter(|s| s.visible()) {
                Some(s) => {
                    let scale = m.osu(256.0) / 411.0 * if hot { 1.05 } else { 1.0 };
                    let (cx, cy) = r.centre();
                    s.draw(batch, cx, cy, scale, ui::alpha(ui::WHITE, a));
                }
                None if skin.menu.pause[i].is_some() => {}
                None => {
                    let px = ui::em_for(r.h * 0.3);
                    batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), ui::rgb(COLOURS[i], a * if hot { 1.0 } else { 0.82 }));
                    batch.frame(
                        r.x,
                        r.y,
                        r.w,
                        r.h,
                        m.px(4.0),
                        (r.h * 0.04).max(1.0),
                        [255, 255, 255, (a * if hot { 255.0 } else { 110.0 }) as u8],
                    );
                    let (cx, cy) = r.centre();
                    ui::centred(batch, cx, cy, px, ui::alpha(ui::WHITE, a), LABELS[i]);
                }
            }
        }
        // Arrows on both sides of the selected button, swinging in and out.
        if let Some(r) = self.selected.map(|s| buttons[s]) {
            let t = ((input.now_ns.saturating_sub(self.since_ns)) as f32 / 1e9 * 3.2) % 2.0;
            let t = if t > 1.0 { 2.0 - t } else { t };
            let swing = -t * (t - 2.0);
            let offset = m.osu(20.0 + 45.0 * swing);
            let px = ui::em_for(r.h * 0.45);
            let colour = [0, 114, 255, (255.0 * a) as u8];
            let cy = r.y + r.h * 0.5;
            ui::centred(batch, r.x - offset, cy, px, colour, ">");
            ui::centred(batch, r.x + r.w + offset, cy, px, colour, "<");
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
    fn keys_and_clicks_choose() {
        let (m, skin) = (Metrics::new(1920.0, 1080.0), Skin::default());
        let mut p = PauseMenu::new(0);
        let key = |p: &mut PauseMenu, code: u32| p.update(&input(-1.0, -1.0, false, &[code]), &m, &skin);
        assert_eq!(key(&mut p, ui::KEY_ENTER), PauseAction::None, "nothing is selected at first");
        assert_eq!(key(&mut p, ui::KEY_DOWN), PauseAction::None);
        assert_eq!(key(&mut p, ui::KEY_ENTER), PauseAction::Continue);
        key(&mut p, ui::KEY_DOWN);
        assert_eq!(key(&mut p, ui::KEY_ENTER), PauseAction::Retry);
        key(&mut p, ui::KEY_UP);
        key(&mut p, ui::KEY_UP);
        assert_eq!(key(&mut p, ui::KEY_ENTER), PauseAction::Quit, "the selection wraps around");
        assert_eq!(key(&mut p, ui::KEY_ESC), PauseAction::Continue);
        let buttons = p.buttons(&m, &skin);
        assert!(buttons[0].y < buttons[1].y && buttons[1].y < buttons[2].y);
        let (cx, cy) = buttons[1].centre();
        assert_eq!(p.update(&input(cx, cy, true, &[]), &m, &skin), PauseAction::Retry);
    }
}
