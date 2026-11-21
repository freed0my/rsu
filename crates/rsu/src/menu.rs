//! The main menu, after McOsu's: a square in the middle that beats with
//! the music; clicked, it moves aside and Play, Options and Exit come out
//! from behind it. In the top right corner is a music player: the song,
//! previous / pause / next, and a bar to move through it.

use crate::text::StackStr;
use crate::ui::{self, Input, Metrics, Rect};
use rsu_gfx::Batch;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuAction {
    None,
    Play,
    Options,
    Exit,
    PreviousSong,
    PlayPause,
    NextSong,
    /// Go to this fraction of the song.
    Seek(f32),
}

/// The song that plays, as the player shows it.
#[derive(Clone, Copy, Debug)]
pub struct MusicView<'a> {
    pub title: &'a str,
    pub seconds: f64,
    /// `None` while the length of the track is not known.
    pub length: Option<f64>,
    pub paused: bool,
}

const LABELS: [&str; 3] = ["Play", "Options", "Exit"];
const KEY_E: u32 = 18;
const KEY_O: u32 = 24;
const KEY_P: u32 = 25;
const KEY_C: u32 = 46;
const KEY_F4: u32 = 62;

pub struct MainMenu {
    /// Whether the buttons are out.
    pub open: bool,
    /// 0 with the square in the middle, 1 with it moved aside.
    slide: f32,
    /// Grows from 0 as the menu first appears.
    startup: f32,
    hover: f32,
    hovers: [f32; 3],
    /// The player's buttons: previous, pause, next.
    music_hovers: [f32; 3],
    /// Where the bar is being dragged to, while it is.
    seeking: Option<f32>,
}

/// Where the parts of the music player are.
struct Player {
    title_y: f32,
    buttons: [Rect; 3],
    bar: Rect,
    px: f32,
}

fn player(m: &Metrics) -> Player {
    let margin = m.px(16.0);
    let px = m.px(ui::FONT_DEFAULT);
    let size = m.px(30.0);
    let gap = m.px(8.0);
    let right = m.w - margin;
    let row = margin + ui::cap(px) + m.px(12.0);
    let buttons = [2.0, 1.0, 0.0].map(|i| Rect::new(right - size - i * (size + gap), row, size, size));
    let bar_w = m.px(360.0).min(m.w * 0.45);
    let bar = Rect::new(right - bar_w, row + size + m.px(14.0), bar_w, m.px(6.0));
    Player { title_y: margin, buttons, bar, px }
}

/// m:ss
fn clock_text(out: &mut impl std::fmt::Write, seconds: f64) {
    let s = seconds.max(0.0) as u64;
    let _ = write!(out, "{}:{:02}", s / 60, s % 60);
}

/// A triangle pointing right (or left) inside `r`, drawn as one-pixel columns.
fn triangle(batch: &mut Batch, r: Rect, right: bool, colour: [u8; 4]) {
    let n = r.w.ceil().max(1.0) as usize;
    for j in 0..n {
        let t = (j as f32 + 0.5) / n as f32;
        let h = r.h * if right { 1.0 - t } else { t };
        batch.rect(r.x + j as f32 * r.w / n as f32, r.y + (r.h - h) * 0.5, r.w / n as f32 + 0.5, h, colour);
    }
}

impl MainMenu {
    pub fn new() -> MainMenu {
        MainMenu {
            open: false,
            slide: 0.0,
            startup: 0.0,
            hover: 0.0,
            hovers: [0.0; 3],
            music_hovers: [0.0; 3],
            seeking: None,
        }
    }

    fn square(&self, m: &Metrics) -> Rect {
        let size = m.osu(324.0).min(m.w * 0.45);
        Rect::new(m.w * 0.5 - size * 0.5 - self.slide * size * 0.5, m.h * 0.5 - size * 0.5, size, size)
    }

    /// The buttons, as far out from behind the square as it has moved.
    fn buttons(&self, m: &Metrics) -> [Rect; 3] {
        let main = self.square(m);
        let size = main.w;
        let padding = (size * 0.075).floor();
        let height = (size / 3.0 - 2.0 * padding).floor();
        let extra = size * 0.06;
        let x = main.x + size * self.slide - extra * self.slide + extra * (1.0 - self.slide);
        let w = size + extra * self.slide - 2.0 * extra * (1.0 - self.slide);
        let top = main.y + (size - height * 3.0 - padding * 2.0) * 0.5;
        [0.0, 1.0, 2.0].map(|i| Rect::new(x, top + i * (height + padding), w, height))
    }

    pub fn update(&mut self, input: &Input, m: &Metrics, music: Option<&MusicView>) -> MenuAction {
        ui::approach(&mut self.startup, 1.0, input.dt, 0.6);
        ui::approach(&mut self.slide, self.open as u8 as f32, input.dt, 0.35);
        let main = self.square(m);
        let over_main = input.over(main);
        ui::approach(&mut self.hover, over_main as u8 as f32, input.dt, 0.15);
        let mut action = MenuAction::None;
        let buttons = self.buttons(m);
        let actions = [MenuAction::Play, MenuAction::Options, MenuAction::Exit];
        for i in 0..3 {
            let over = self.open && !over_main && input.over(buttons[i]);
            ui::approach(&mut self.hovers[i], over as u8 as f32, input.dt, 0.12);
            if over && input.pressed {
                action = actions[i];
            }
        }
        if input.pressed && over_main {
            self.open = !self.open;
        }
        if let Some(music) = music {
            let p = player(m);
            let song_actions = [MenuAction::PreviousSong, MenuAction::PlayPause, MenuAction::NextSong];
            for i in 0..3 {
                let over = input.over(p.buttons[i]);
                ui::approach(&mut self.music_hovers[i], over as u8 as f32, input.dt, 0.12);
                if over && input.pressed {
                    action = song_actions[i];
                }
            }
            let at = |x: f32| ((x - p.bar.x) / p.bar.w).clamp(0.0, 1.0);
            // The bar is thin; it can be taken a little above and below.
            let grip = Rect::new(p.bar.x - p.bar.h, p.bar.y - p.bar.h * 2.0, p.bar.w + p.bar.h * 2.0, p.bar.h * 5.0);
            if input.pressed && input.over(grip) && music.length.is_some() {
                self.seeking = Some(at(input.x));
            }
            if let Some(to) = &mut self.seeking {
                if input.down {
                    *to = at(input.x);
                } else {
                    action = MenuAction::Seek(*to);
                    self.seeking = None;
                }
            }
        } else {
            self.seeking = None;
        }
        for &code in input.keys {
            match code {
                KEY_C | KEY_F4 => action = MenuAction::PlayPause,
                ui::KEY_RIGHT | ui::KEY_F2 => action = MenuAction::NextSong,
                ui::KEY_LEFT => action = MenuAction::PreviousSong,
                ui::KEY_ENTER | KEY_P if self.open => action = MenuAction::Play,
                ui::KEY_ENTER | KEY_P => self.open = true,
                KEY_O => action = MenuAction::Options,
                KEY_E if self.open => action = MenuAction::Exit,
                ui::KEY_ESC if self.open => self.open = false,
                ui::KEY_ESC => action = MenuAction::Exit,
                _ => {}
            }
        }
        action
    }

    /// `beat` is how far into the current beat of the music it is, 0 to 1;
    /// without music the square beats by the clock.
    pub fn draw(&self, batch: &mut Batch, m: &Metrics, beat: Option<f32>, music: Option<&MusicView>, now_ns: u64) {
        let pulse = match beat {
            Some(phase) => 1.0 - phase.clamp(0.0, 1.0),
            None => 1.0 - ((now_ns as f64 / 1e9) % 1.25 / 1.25) as f32,
        };
        // The buttons first: the square covers the ends they come out of.
        if self.slide > 0.01 {
            let a = self.slide;
            for (i, r) in self.buttons(m).into_iter().enumerate() {
                let hot = self.hovers[i];
                let shade = (50.0 * hot) as u8;
                batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), [shade, shade, shade, (255.0 * a) as u8]);
                batch.frame(r.x, r.y, r.w, r.h, m.px(4.0), (r.h / 40.0).max(1.0), ui::alpha(ui::WHITE, a));
                let px = ui::em_for(r.h * 0.3);
                let (cx, cy) = r.centre();
                ui::centred(batch, cx + r.w * 0.03, cy, px, ui::alpha(ui::WHITE, a), LABELS[i]);
            }
        }

        let base = self.square(m);
        let size = base.w * (1.0 - 0.05 * pulse) * (1.0 + 0.04 * self.hover) * self.startup;
        let (cx, cy) = base.centre();
        let r = Rect::new(cx - size * 0.5, cy - size * 0.5, size, size);
        let edge = (size * 0.02).max(2.0);
        batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), ui::BLACK);
        batch.frame(r.x, r.y, r.w, r.h, m.px(4.0), edge, ui::WHITE);
        // A second, fainter edge that the beat throws outwards.
        let out = size * 0.06 * (1.0 - pulse);
        batch.frame(
            r.x - out,
            r.y - out,
            r.w + 2.0 * out,
            r.h + 2.0 * out,
            m.px(4.0),
            edge * 0.5,
            [255, 255, 255, (90.0 * pulse) as u8],
        );
        let px = ui::em_for(size * 0.2);
        ui::centred(batch, cx, cy, px, ui::WHITE, "rsu");

        let px = m.px(ui::FONT_DEFAULT);
        let version = concat!("rsu ", env!("CARGO_PKG_VERSION"));
        ui::text(batch, m.px(8.0), m.h - ui::cap(px) - m.px(8.0), px, [255, 255, 255, 120], version);

        if let Some(music) = music {
            self.draw_player(batch, m, music);
        }
    }

    fn draw_player(&self, batch: &mut Batch, m: &Metrics, music: &MusicView) {
        let p = player(m);
        let fade = self.startup;
        let right = p.bar.x + p.bar.w;
        let title = ui::fit(p.px, music.title, p.bar.w.max(m.w * 0.5));
        ui::right_aligned(batch, right, p.title_y, p.px, ui::alpha(ui::WHITE, fade), title);

        for (i, r) in p.buttons.iter().enumerate() {
            let hot = self.music_hovers[i];
            batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), [255, 255, 255, (30.0 + 50.0 * hot) as u8]);
            let icon = Rect::new(r.x + r.w * 0.28, r.y + r.h * 0.25, r.w * 0.44, r.h * 0.5);
            let c = ui::alpha(ui::WHITE, fade);
            let bar_w = (icon.w * 0.22).max(1.0);
            match i {
                0 => {
                    batch.rect(icon.x, icon.y, bar_w, icon.h, c);
                    triangle(batch, Rect::new(icon.x + bar_w, icon.y, icon.w - bar_w, icon.h), false, c);
                }
                1 if music.paused => {
                    triangle(batch, Rect::new(icon.x + icon.w * 0.1, icon.y, icon.w * 0.85, icon.h), true, c)
                }
                1 => {
                    batch.rect(icon.x + icon.w * 0.1, icon.y, icon.w * 0.3, icon.h, c);
                    batch.rect(icon.x + icon.w * 0.6, icon.y, icon.w * 0.3, icon.h, c);
                }
                _ => {
                    triangle(batch, Rect::new(icon.x, icon.y, icon.w - bar_w, icon.h), true, c);
                    batch.rect(icon.x + icon.w - bar_w, icon.y, bar_w, icon.h, c);
                }
            }
        }

        // The bar: how far the song is, or where it is being dragged to.
        let b = p.bar;
        let shown = match (self.seeking, music.length) {
            (Some(to), _) => to,
            (None, Some(length)) if length > 0.0 => (music.seconds / length).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        };
        batch.rrect(b.x, b.y, b.w, b.h, b.h * 0.5, [255, 255, 255, (70.0 * fade) as u8]);
        batch.rrect(b.x, b.y, b.w * shown, b.h, b.h * 0.5, [0, 190, 255, (255.0 * fade) as u8]);
        batch.disc(
            b.x + b.w * shown,
            b.y + b.h * 0.5,
            b.h * (if self.seeking.is_some() { 1.6 } else { 1.1 }),
            ui::alpha(ui::WHITE, fade),
            0,
        );

        let mut times = StackStr::<32>::new();
        let seconds = match (self.seeking, music.length) {
            (Some(to), Some(length)) => to as f64 * length,
            _ => music.seconds,
        };
        clock_text(&mut times, seconds);
        if let Some(length) = music.length {
            let _ = std::fmt::Write::write_str(&mut times, " / ");
            clock_text(&mut times, length);
        }
        let small = p.px * 0.8;
        ui::right_aligned(
            batch,
            right,
            b.y + b.h + m.px(8.0),
            small,
            [255, 255, 255, (170.0 * fade) as u8],
            times.as_str(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(x: f32, y: f32, pressed: bool, keys: &[u32]) -> Input<'_> {
        Input { x, y, pressed, down: pressed, scroll: 0.0, keys, text: "", dt: 0.05, now_ns: 0 }
    }

    #[test]
    fn the_square_opens_the_menu_and_the_buttons_act() {
        let m = Metrics::new(1920.0, 1080.0);
        let mut menu = MainMenu::new();
        assert_eq!(menu.update(&input(960.0, 540.0, true, &[]), &m, None), MenuAction::None);
        assert!(menu.open);
        for _ in 0..200 {
            menu.update(&input(0.0, 0.0, false, &[]), &m, None);
        }
        let [play, options, exit] = menu.buttons(&m);
        assert!(play.x >= menu.square(&m).x + menu.square(&m).w * 0.9, "the buttons are out to the right");
        let click =
            |menu: &mut MainMenu, r: Rect| menu.update(&input(r.x + r.w * 0.8, r.y + r.h * 0.5, true, &[]), &m, None);
        assert_eq!(click(&mut menu, play), MenuAction::Play);
        assert_eq!(click(&mut menu, options), MenuAction::Options);
        assert_eq!(click(&mut menu, exit), MenuAction::Exit);
    }

    #[test]
    fn keys_do_the_same() {
        let m = Metrics::new(1280.0, 720.0);
        let mut menu = MainMenu::new();
        let key = |menu: &mut MainMenu, code: u32| menu.update(&input(0.0, 0.0, false, &[code]), &m, None);
        assert_eq!(key(&mut menu, ui::KEY_ENTER), MenuAction::None, "the first Enter opens");
        assert_eq!(key(&mut menu, ui::KEY_ENTER), MenuAction::Play);
        assert_eq!(key(&mut menu, ui::KEY_ESC), MenuAction::None, "Esc closes first");
        assert_eq!(key(&mut menu, ui::KEY_ESC), MenuAction::Exit);
        assert_eq!(key(&mut menu, KEY_O), MenuAction::Options);
    }

    #[test]
    fn the_player_pauses_skips_and_seeks() {
        let m = Metrics::new(1280.0, 720.0);
        let mut menu = MainMenu::new();
        let music = MusicView { title: "Artist - Title", seconds: 30.0, length: Some(120.0), paused: false };
        let p = player(&m);
        let at = |r: Rect| (r.x + r.w * 0.5, r.y + r.h * 0.5);
        let click = |menu: &mut MainMenu, (x, y): (f32, f32)| menu.update(&input(x, y, true, &[]), &m, Some(&music));
        assert_eq!(click(&mut menu, at(p.buttons[0])), MenuAction::PreviousSong);
        assert_eq!(click(&mut menu, at(p.buttons[1])), MenuAction::PlayPause);
        assert_eq!(click(&mut menu, at(p.buttons[2])), MenuAction::NextSong);
        // A click on the bar three quarters along goes there when let go.
        let (x, y) = (p.bar.x + p.bar.w * 0.75, p.bar.y + p.bar.h * 0.5);
        let mut held = input(x, y, true, &[]);
        held.down = true;
        assert_eq!(menu.update(&held, &m, Some(&music)), MenuAction::None, "nothing while held");
        let mut dragged = input(p.bar.x + p.bar.w * 0.25, y, false, &[]);
        dragged.down = true;
        assert_eq!(menu.update(&dragged, &m, Some(&music)), MenuAction::None);
        assert_eq!(menu.update(&input(0.0, 0.0, false, &[]), &m, Some(&music)), MenuAction::Seek(0.25));
        let key = |menu: &mut MainMenu, code: u32| menu.update(&input(0.0, 0.0, false, &[code]), &m, Some(&music));
        assert_eq!(key(&mut menu, KEY_C), MenuAction::PlayPause);
        assert_eq!(key(&mut menu, ui::KEY_RIGHT), MenuAction::NextSong);
        assert_eq!(key(&mut menu, ui::KEY_LEFT), MenuAction::PreviousSong);
    }
}
