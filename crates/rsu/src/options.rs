//! The options: a panel that slides in from the left, as in McOsu, with
//! what can be changed while the program runs.

use crate::text::StackStr;
use crate::ui::{self, Input, Metrics, Rect, Scroll};
use std::fmt::Write;
use std::path::PathBuf;
use rsu_gfx::Batch;

/// The settings the panel changes directly. Whoever shows the panel
/// compares before and after and acts on the difference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tunables {
    /// Applies to music and effects alike; the wheel changes this one.
    pub master: f32,
    pub music: f32,
    pub effects: f32,
    pub offset_ms: f32,
    /// How much of the beatmap's background is taken away during play.
    pub dim: f32,
    /// Cursor size, a multiple of the skin image's size (McOsu: 1.5).
    pub cursor_scale: f32,
    pub replay_smooth_trail: bool,
    pub songselect_top: bool,
    pub songselect_bottom: bool,
    /// Frames per second for the modes that do not wait for the display;
    /// 0 is no limit.
    pub fps_cap: u32,
    pub beatmap_hitsounds: bool,
    pub show_overlay: bool,
    /// Keys and buttons, in the order of `binds::ACTIONS`.
    pub binds: crate::binds::Binds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionsAction {
    None,
    Close,
    NextPresentMode,
    ApplySongs,
    /// Use the skin with this index in `Options::skins`.
    Skin(usize),
}

pub const CURSOR_SCALE_MIN: f32 = 0.1;
pub const CURSOR_SCALE_MAX: f32 = 3.0;

pub const FPS_CAPS: [u32; 7] = [240, 360, 480, 720, 1000, 2000, 0];

pub struct Options {
    shown: f32,
    scroll: Scroll,
    /// The slider being dragged.
    grabbed: Option<usize>,
    /// Installed skins by name; the first entry is rsu's own look.
    pub skins: Vec<(String, Option<PathBuf>)>,
    pub skin: usize,
    skins_open: bool,
    /// The list was just opened and may have to be scrolled into view.
    reveal: bool,
    skin_scroll: Scroll,
    /// Shown, not changed here.
    pub songs: String,
    editing_songs: bool,
    songs_before_edit: String,
    pub present: &'static str,
    content: f32,
    /// The key binding waiting for a key, if any.
    capture: Option<usize>,
}

impl Options {
    pub fn new(skins: Vec<(String, Option<PathBuf>)>, skin: usize, songs: String, present: &'static str) -> Options {
        Options {
            shown: 0.0,
            scroll: Scroll::default(),
            grabbed: None,
            skins,
            skin,
            skins_open: false,
            reveal: false,
            skin_scroll: Scroll::default(),
            songs,
            editing_songs: false,
            songs_before_edit: String::new(),
            present,
            content: 0.0,
            capture: None,
        }
    }

    fn panel(&self, m: &Metrics) -> Rect {
        let w = (m.w * 0.36).max(m.px(640.0)).min(m.w);
        Rect::new(-w * (1.0 - self.shown), 0.0, w, m.h)
    }

    /// One frame: act on the input and draw.
    pub fn frame(&mut self, batch: &mut Batch, input: &Input, m: &Metrics, t: &mut Tunables) -> OptionsAction {
        ui::approach(&mut self.shown, 1.0, input.dt, 0.25);
        let panel = self.panel(m);
        let mut action = OptionsAction::None;
        batch.rect(0.0, 0.0, m.w, m.h, [0, 0, 0, (110.0 * self.shown) as u8]);
        batch.rect(panel.x, 0.0, panel.w, m.h, [12, 12, 16, 242]);
        batch.rect(panel.x + panel.w, 0.0, m.px(2.0), m.h, [255, 255, 255, 60]);

        let px = m.px(ui::FONT_DEFAULT) * 1.1;
        let row_h = m.px(46.0);
        let pad = m.px(28.0);
        let footer_h = m.px(84.0);
        let (x, w) = (panel.x + pad, panel.w - 2.0 * pad);
        let body = Rect::new(panel.x, 0.0, panel.w, m.h - footer_h);

        // The list of skins, while open, has the wheel to itself.
        let mut list_input = *input;
        if self.skins_open {
            list_input.scroll = 0.0;
        }
        let over_widget = self.grabbed.is_some();
        let drag_area = if over_widget { Rect::default() } else { body };
        self.scroll.update(&list_input, drag_area, self.content, body.h, row_h * 1.5, m.px(12.0));
        if !input.down {
            self.grabbed = None;
        }
        // A binding waits for the next key: Esc leaves it as it is, Backspace
        // takes it away, anything else becomes it (and leaves any other
        // action that had it).
        let capturing = self.capture.is_some();
        if let Some(i) = self.capture {
            for &code in input.keys {
                if code == ui::KEY_ESC {
                    self.capture = None;
                } else if code == ui::KEY_BACKSPACE {
                    t.binds[i] = 0;
                    self.capture = None;
                } else if crate::binds::bindable(code) {
                    for other in t.binds.iter_mut().filter(|c| **c == code) {
                        *other = 0;
                    }
                    t.binds[i] = code;
                    self.capture = None;
                } else {
                    continue;
                }
                break;
            }
        }
        let top = m.px(24.0) - self.scroll.pos;
        let mut y = top;
        let mut line = StackStr::<200>::new();
        // A click counts only where the row can be seen.
        let live = |r: Rect| r.y + r.h > 0.0 && r.y < body.h;

        ui::text(batch, x, y, m.px(ui::FONT_TITLE) * 0.6, ui::WHITE, "Options");
        y += m.px(ui::FONT_TITLE) * 0.6;

        let heading = |batch: &mut Batch, y: &mut f32, name: &str| {
            *y += row_h * 0.5;
            let big = px * 1.5;
            ui::text(batch, x, *y, big, [0, 190, 255, 255], name);
            batch.rect(x, *y + ui::cap(big) * 1.5, w, m.px(2.0), [0, 190, 255, 120]);
            *y += ui::cap(big) * 1.5 + row_h * 0.35;
        };
        let centre_y = |y: f32| y + (row_h - ui::cap(px)) * 0.5;
        // While the list of skins is open, a press belongs to it.
        let list_open = self.skins_open;
        // A slider with its name on the left and its value on the right.
        let slider_row = |batch: &mut Batch,
                          y: &mut f32,
                          index: usize,
                          grabbed: &mut Option<usize>,
                          name: &str,
                          shown: &str,
                          value: f32|
         -> Option<f32> {
            let label_w = w * 0.36;
            let value_w = w * 0.2;
            let r = Rect::new(x + label_w, *y + row_h * 0.15, w - label_w - value_w, row_h * 0.7);
            ui::text(batch, x, centre_y(*y), px, ui::WHITE, ui::fit(px, name, label_w - m.px(8.0)));
            ui::text(batch, r.x + r.w + m.px(12.0), centre_y(*y), px, [200, 200, 200, 255], shown);
            if input.pressed && input.over(r) && live(r) && !list_open {
                *grabbed = Some(index);
            }
            let out = ui::slider(batch, input, r, value, *grabbed == Some(index));
            *y += row_h;
            out
        };

        heading(batch, &mut y, "General");
        ui::text(batch, x, centre_y(y), px, ui::WHITE, "osu! Songs folder:");
        y += row_h;
        let field = Rect::new(x, y + row_h * 0.1, w, row_h * 0.8);
        if input.clicked(field) && live(field) && !list_open && !self.editing_songs {
            self.songs_before_edit = self.songs.clone();
            self.editing_songs = true;
        }
        let editing = self.editing_songs;
        if editing {
            if input.key(ui::KEY_ESC) {
                self.songs.clone_from(&self.songs_before_edit);
                self.editing_songs = false;
            } else if input.key(ui::KEY_ENTER) {
                self.editing_songs = false;
                action = OptionsAction::ApplySongs;
            } else {
                for key in input.keys {
                    if *key == ui::KEY_BACKSPACE { self.songs.pop(); }
                }
                self.songs.extend(input.text.chars().filter(|c| !c.is_control()));
            }
        }
        let shown = if self.songs.is_empty() { "Click to enter a path" } else { &self.songs };
        let mut start = 0;
        while ui::text_width(px, &shown[start..]) > w - m.px(28.0) {
            match shown[start..].chars().next() {
                Some(c) => start += c.len_utf8(),
                None => break,
            }
        }
        ui::button(batch, field, if self.editing_songs { [70, 95, 120] } else { [50, 60, 80] }, &shown[start..], px, input.over(field));
        y += row_h;
        ui::text(
            batch,
            x,
            centre_y(y),
            px * 0.85,
            [160, 160, 170, 255],
            "Click to edit, Enter to apply, Esc to cancel.",
        );
        y += row_h;

        heading(batch, &mut y, "Graphics");
        let r = Rect::new(x, y + row_h * 0.1, w, row_h * 0.8);
        line.clear();
        let _ = write!(line, "Present mode: {}", self.present);
        ui::button(batch, r, [50, 60, 80], line.as_str(), px, input.over(r));
        if input.clicked(r) && live(r) {
            action = OptionsAction::NextPresentMode;
        }
        y += row_h;
        line.clear();
        match t.fps_cap {
            0 => {
                let _ = write!(line, "unlimited");
            }
            n => {
                let _ = write!(line, "{n}");
            }
        }
        let steps = (FPS_CAPS.len() - 1) as f32;
        let at = FPS_CAPS.iter().position(|&c| c == t.fps_cap).unwrap_or(4) as f32 / steps;
        if let Some(v) = slider_row(batch, &mut y, 0, &mut self.grabbed, "FPS limiter", line.as_str(), at) {
            t.fps_cap = FPS_CAPS[(v * steps).round() as usize];
        }
        let r = Rect::new(x, y, w, row_h);
        ui::checkbox(batch, input, r, t.show_overlay, "Show measurements (F9)", px);
        if input.clicked(r) && live(r) {
            t.show_overlay = !t.show_overlay;
        }
        y += row_h;

        heading(batch, &mut y, "Audio");
        line.clear();
        let _ = write!(line, "{:.0}%", t.master * 100.0);
        if let Some(v) = slider_row(batch, &mut y, 5, &mut self.grabbed, "Master", line.as_str(), t.master) {
            t.master = (v * 100.0).round() / 100.0;
        }
        line.clear();
        let _ = write!(line, "{:.0}%", t.music * 100.0);
        if let Some(v) = slider_row(batch, &mut y, 1, &mut self.grabbed, "Music", line.as_str(), t.music) {
            t.music = (v * 100.0).round() / 100.0;
        }
        line.clear();
        let _ = write!(line, "{:.0}%", t.effects * 100.0);
        if let Some(v) = slider_row(batch, &mut y, 2, &mut self.grabbed, "Effects", line.as_str(), t.effects) {
            t.effects = (v * 100.0).round() / 100.0;
        }
        line.clear();
        let _ = write!(line, "{:+.0} ms", t.offset_ms);
        let at = (t.offset_ms + 150.0) / 300.0;
        if let Some(v) = slider_row(batch, &mut y, 3, &mut self.grabbed, "Universal offset", line.as_str(), at) {
            t.offset_ms = (v * 300.0 - 150.0).round();
        }
        let r = Rect::new(x, y, w, row_h);
        ui::checkbox(batch, input, r, t.beatmap_hitsounds, "Beatmap hit sounds (as osu! plays them)", px);
        if input.clicked(r) && live(r) {
            t.beatmap_hitsounds = !t.beatmap_hitsounds;
        }
        y += row_h;

        heading(batch, &mut y, "Keys");
        for (i, action) in crate::binds::ACTIONS.iter().enumerate() {
            let r = Rect::new(x, y + row_h * 0.1, w, row_h * 0.8);
            line.clear();
            let waiting = self.capture == Some(i);
            if waiting {
                let _ = write!(line, "{}: press a key  (Esc cancels, Backspace clears)", action.label);
            } else {
                let _ = write!(line, "{}: {}", action.label, crate::binds::name(t.binds[i]).as_str());
            }
            let colour = if waiting { [110, 80, 30] } else { [50, 60, 80] };
            ui::button(batch, r, colour, line.as_str(), px, input.over(r));
            if input.clicked(r) && live(r) && !list_open {
                self.capture = Some(i);
            }
            y += row_h;
        }

        heading(batch, &mut y, "Skin");
        let r = Rect::new(x, y + row_h * 0.1, w, row_h * 0.8);
        line.clear();
        let _ = write!(line, "Skin: {}", self.skins.get(self.skin).map_or("?", |s| s.0.as_str()));
        ui::button(batch, r, [50, 60, 80], line.as_str(), px, input.over(r));
        let toggled = input.clicked(r) && live(r);
        y += row_h;
        if self.skins_open {
            // The list opens in place and scrolls by itself.
            let shown = self.skins.len().min(9) as f32;
            let list = Rect::new(x, y, w, shown * row_h * 0.8);
            let item_h = row_h * 0.8;
            if std::mem::take(&mut self.reveal) {
                self.scroll.by((list.y + list.h + row_h * 0.5 - body.h).max(0.0));
            }
            let clicked = self.skin_scroll.update(
                input,
                list,
                self.skins.len() as f32 * item_h,
                list.h,
                item_h * 2.0,
                m.px(12.0),
            );
            batch.rrect(list.x, list.y, list.w, list.h, m.px(4.0), [28, 28, 34, 255]);
            for (i, (name, _)) in self.skins.iter().enumerate() {
                let iy = list.y + i as f32 * item_h - self.skin_scroll.pos;
                if iy + item_h * 0.5 < list.y || iy + item_h * 0.6 > list.y + list.h {
                    continue;
                }
                let item = Rect::new(list.x, iy, list.w, item_h);
                let hot = input.over(item) && input.over(list);
                if hot {
                    batch.rrect(item.x, item.y, item.w, item.h, m.px(4.0), [255, 255, 255, 30]);
                }
                let colour = if i == self.skin { [0, 190, 255, 255] } else { ui::WHITE };
                ui::text(
                    batch,
                    item.x + m.px(14.0),
                    iy + (item_h - ui::cap(px)) * 0.5,
                    px,
                    colour,
                    ui::fit(px, name, w - m.px(28.0)),
                );
                if hot && clicked && live(item) {
                    action = OptionsAction::Skin(i);
                    self.skin = i;
                    self.skins_open = false;
                }
            }
            batch.frame(list.x, list.y, list.w, list.h, m.px(4.0), 1.0, [255, 255, 255, 110]);
            y += list.h + row_h * 0.2;
        }
        if toggled {
            self.skins_open = !self.skins_open;
            self.reveal = self.skins_open;
            // Room for the list, so that it can be scrolled to at once.
            y += if self.skins_open { self.skins.len().min(9) as f32 * row_h * 0.8 } else { 0.0 };
        }

        for (value, label) in [
            (&mut t.songselect_top, "Show song select top panel"),
            (&mut t.songselect_bottom, "Show song select bottom panel"),
        ] {
            let r = Rect::new(x, y, w, row_h);
            ui::checkbox(batch, input, r, *value, label, px);
            if input.clicked(r) && live(r) && !list_open {
                *value = !*value;
            }
            y += row_h;
        }

        line.clear();
        let _ = write!(line, "{:.2}x", t.cursor_scale);
        let span = CURSOR_SCALE_MAX - CURSOR_SCALE_MIN;
        let at = (t.cursor_scale - CURSOR_SCALE_MIN) / span;
        if let Some(v) = slider_row(batch, &mut y, 6, &mut self.grabbed, "Cursor size", line.as_str(), at) {
            t.cursor_scale = ((CURSOR_SCALE_MIN + v * span) * 20.0).round() / 20.0;
        }

        heading(batch, &mut y, "Gameplay");
        let r = Rect::new(x, y, w, row_h);
        ui::checkbox(batch, input, r, t.replay_smooth_trail, "Force smooth cursor trail on replays", px);
        if input.clicked(r) && live(r) && !list_open {
            t.replay_smooth_trail = !t.replay_smooth_trail;
        }
        y += row_h;
        line.clear();
        let _ = write!(line, "{:.0}%", t.dim * 100.0);
        if let Some(v) = slider_row(batch, &mut y, 4, &mut self.grabbed, "Background dim", line.as_str(), t.dim) {
            t.dim = (v * 100.0).round() / 100.0;
        }
        y += row_h;
        self.content = y - top + m.px(24.0);

        // The footer covers what scrolls under it.
        batch.rect(panel.x, m.h - footer_h, panel.w, footer_h, [12, 12, 16, 255]);
        batch.rect(panel.x, m.h - footer_h, panel.w, m.px(2.0), [255, 255, 255, 40]);
        let back = Rect::new(x, m.h - footer_h + m.px(16.0), m.px(200.0), footer_h - m.px(32.0));
        ui::button(batch, back, [200, 50, 130], "Back", px * 1.1, input.over(back));
        if input.clicked(back)
            || (input.pressed && input.x > panel.x + panel.w)
            || (input.key(ui::KEY_ESC) && !capturing && !editing)
        {
            action = OptionsAction::Close;
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsu_gfx::Inst;

    #[test]
    fn songs_path_can_be_edited_and_applied() {
        let m = Metrics::new(1920.0, 1080.0);
        let mut o = Options::new(vec![("rsu".into(), None)], 0, "/old".into(), "FIFO");
        let mut t = Tunables {
            master: 1.0, music: 0.4, effects: 1.0, offset_ms: 0.0, dim: 0.75,
            cursor_scale: 1.5, fps_cap: 1000, beatmap_hitsounds: false,
            replay_smooth_trail: false, songselect_top: true, songselect_bottom: true,
            show_overlay: false, binds: crate::binds::defaults(),
        };
        let idle = Input { x: 0.0, y: 0.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 1.0, now_ns: 0 };
        with_batch(|batch| {
            o.frame(batch, &idle, &m, &mut t);
            o.frame(batch, &Input { x: 100.0, y: 210.0, pressed: true, down: true, ..idle }, &m, &mut t);
            o.frame(batch, &Input { keys: &[ui::KEY_BACKSPACE], text: "/new", ..idle }, &m, &mut t);
            assert_eq!(o.songs, "/ol/new");
            assert_eq!(o.frame(batch, &Input { keys: &[ui::KEY_ENTER], ..idle }, &m, &mut t), OptionsAction::ApplySongs);
        });
    }

    /// A batch that draws into memory, for running a frame without a GPU.
    fn with_batch(run: impl FnOnce(&mut Batch)) {
        let mut store = vec![std::mem::MaybeUninit::<Inst>::uninit(); 4096];
        // SAFETY: the store outlives the batch and has room for its capacity.
        let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
        run(&mut batch);
    }

    #[test]
    fn replay_smooth_trail_checkbox_toggles() {
        let m = Metrics::new(1920.0, 1080.0);
        let idle = Input { x: 0.0, y: 0.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 1.0, now_ns: 0 };
        let mut found = false;
        for y in (0..2900).step_by(5) {
            let mut o = Options::new(vec![("rsu".into(), None)], 0, String::new(), "FIFO");
            let mut t = test_tunables();
            with_batch(|batch| {
                o.frame(batch, &idle, &m, &mut t);
                o.scroll.pos = (o.content - 900.0).max(0.0);
                let click = Input { x: 100.0, y: y as f32, pressed: true, down: true, ..idle };
                o.frame(batch, &click, &m, &mut t);
                if t.replay_smooth_trail {
                    found = true;
                    o.frame(batch, &click, &m, &mut t);
                    assert!(!t.replay_smooth_trail);
                }
            });
            if found { break; }
        }
        assert!(found, "checkbox was not clickable");
    }

    fn test_tunables() -> Tunables {
        Tunables {
            master: 1.0, music: 0.4, effects: 1.0, offset_ms: 0.0, dim: 0.75,
            cursor_scale: 1.5, songselect_top: true, songselect_bottom: true,
            replay_smooth_trail: false, fps_cap: 1000, beatmap_hitsounds: false,
            show_overlay: false, binds: crate::binds::defaults(),
        }
    }

    #[test]
    fn the_panel_changes_settings_and_closes() {
        let m = Metrics::new(1920.0, 1080.0);
        let skins = vec![("rsu".to_string(), None), ("Some Skin".to_string(), Some(PathBuf::from("/skins/some")))];
        let mut o = Options::new(skins, 0, "/songs".into(), "FIFO");
        let mut t = Tunables {
            master: 1.0,
            music: 0.4,
            effects: 1.0,
            offset_ms: 0.0,
            dim: 0.75,
            cursor_scale: 1.5,
            replay_smooth_trail: false,
            songselect_top: true,
            songselect_bottom: true,
            fps_cap: 1000,
            beatmap_hitsounds: false,
            show_overlay: false,
            binds: crate::binds::defaults(),
        };
        let idle = Input { x: 0.0, y: 0.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
        with_batch(|batch| {
            for _ in 0..100 {
                assert_eq!(o.frame(batch, &idle, &m, &mut t), OptionsAction::None);
            }
            let before = t;
            // A click outside the panel closes it and changes nothing.
            let outside = Input { x: 1800.0, y: 500.0, pressed: true, down: true, ..idle };
            assert_eq!(o.frame(batch, &outside, &m, &mut t), OptionsAction::Close);
            assert_eq!(t, before);
            assert_eq!(o.frame(batch, &Input { keys: &[ui::KEY_ESC], ..idle }, &m, &mut t), OptionsAction::Close);
            assert!(o.content > 500.0 && o.panel(&m).x == 0.0);
        });
    }
}
