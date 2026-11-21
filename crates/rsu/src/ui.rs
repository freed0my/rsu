//! What the menus are built from: the input of one frame, sizes that follow
//! the window the way McOsu's do, text placed by pixel size, and the few
//! widgets (button, slider, checkbox, scrolling) the screens share.

use crate::text;
use rsu_gfx::{Batch, BACKGROUND_SIZE, PAGE_BACKGROUND};

pub const WHITE: [u8; 4] = [255, 255, 255, 255];
pub const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Font sizes of McOsu's interface, in pixels of the em at 1080 lines.
pub const FONT_DEFAULT: f32 = 20.0;
pub const FONT_SUBTITLE: f32 = 28.0;
pub const FONT_TITLE: f32 = 80.0;

pub const KEY_ESC: u32 = 1;
pub const KEY_MINUS: u32 = 12;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_ENTER: u32 = 28;
pub const KEY_SPACE: u32 = 57;
pub const KEY_F1: u32 = 59;
pub const KEY_F2: u32 = 60;
pub const KEY_F3: u32 = 61;
pub const KEY_UP: u32 = 103;
pub const KEY_PAGE_UP: u32 = 104;
pub const KEY_LEFT: u32 = 105;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_DOWN: u32 = 108;
pub const KEY_PAGE_DOWN: u32 = 109;

/// The character a key types, by physical position on a US layout. Search
/// only needs letters, digits and a few separators.
pub fn key_char(code: u32) -> Option<char> {
    const ROWS: [(u32, &str); 4] = [(2, "1234567890"), (16, "qwertyuiop"), (30, "asdfghjkl"), (44, "zxcvbnm")];
    match code {
        KEY_SPACE => Some(' '),
        KEY_MINUS => Some('-'),
        _ => ROWS.iter().find_map(|&(first, keys)| keys.chars().nth(code.checked_sub(first)? as usize)),
    }
}

/// What the player did since the last frame, as far as a menu cares.
#[derive(Clone, Copy)]
pub struct Input<'a> {
    pub x: f32,
    pub y: f32,
    /// The button (or pen tip) went down in this frame.
    pub pressed: bool,
    /// It is being held.
    pub down: bool,
    /// Wheel notches; positive is down.
    pub scroll: f32,
    /// Codes of the keys pressed in this frame, in order.
    pub keys: &'a [u32],
    /// Characters typed in this frame, in the user's own layout.
    pub text: &'a str,
    /// Seconds since the previous frame.
    pub dt: f32,
    pub now_ns: u64,
}

impl<'a> Input<'a> {
    /// The same moment for a screen that something else is covering: time
    /// passes, but nothing is pointed at, pressed or typed.
    pub fn covered(&self) -> Input<'static> {
        Input {
            x: -1.0e6,
            y: -1.0e6,
            pressed: false,
            down: false,

            scroll: 0.0,
            keys: &[],
            text: "",
            dt: self.dt,
            now_ns: self.now_ns,
        }
    }

    pub fn over(&self, r: Rect) -> bool {
        r.contains(self.x, self.y)
    }

    pub fn clicked(&self, r: Rect) -> bool {
        self.pressed && self.over(r)
    }

    pub fn key(&self, code: u32) -> bool {
        self.keys.contains(&code)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn centre(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
}

/// The window, and the two units McOsu lays its interface out in.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub w: f32,
    pub h: f32,
    /// Pixels per pixel of osu!'s 640x480 screen, fitted into the window.
    osu: f32,
    /// Pixels per pixel of a 1080-line screen: what sizes given in plain
    /// pixels are multiplied by.
    pub dpi: f32,
}

impl Metrics {
    pub fn new(w: f32, h: f32) -> Metrics {
        let fitted = (w * 0.75).min(h);
        Metrics { w, h, osu: fitted / 480.0, dpi: (h / 1080.0).clamp(0.5, 4.0) }
    }

    /// A length given in osu! pixels (McOsu's `getUIScale(osu, x)`).
    pub fn osu(&self, x: f32) -> f32 {
        x * self.osu
    }

    /// A length given in pixels of a 1080-line screen.
    pub fn px(&self, x: f32) -> f32 {
        x * self.dpi
    }

    /// The factor that makes an image of `w` x `h` pixels cover `size` osu!
    /// pixels in both directions (McOsu's `getImageScale`).
    pub fn image_scale(&self, w: f32, h: f32, size: f32) -> f32 {
        (self.osu(size) / w).max(self.osu(size) / h)
    }
}

/// Move `value` towards `target`, covering most of the way in `seconds`.
pub fn approach(value: &mut f32, target: f32, dt: f32, seconds: f32) {
    if seconds <= 0.0 || (*value - target).abs() < 1.0e-4 {
        *value = target;
    } else {
        *value += (target - *value) * (1.0 - (-dt * 4.0 / seconds).exp());
    }
}

pub fn alpha(colour: [u8; 4], a: f32) -> [u8; 4] {
    [colour[0], colour[1], colour[2], (colour[3] as f32 * a.clamp(0.0, 1.0)) as u8]
}

pub fn rgb(c: [u8; 3], a: f32) -> [u8; 4] {
    [c[0], c[1], c[2], (255.0 * a.clamp(0.0, 1.0)) as u8]
}

/// Text with an em of `px` pixels and the top of its capitals at `y`.
pub fn text(batch: &mut Batch, x: f32, y: f32, px: f32, colour: [u8; 4], s: &str) -> f32 {
    text::draw(batch, x, y, px / 7.0, colour, s)
}

pub fn text_width(px: f32, s: &str) -> f32 {
    text::width(px / 7.0, s)
}

/// The longest beginning of `s` that fits into `width`.
pub fn fit(px: f32, s: &str, width: f32) -> &str {
    text::fit(px / 7.0, s, width)
}

/// How tall the capitals of text with an em of `px` are.
pub fn cap(px: f32) -> f32 {
    px * 5.0 / 7.0
}

/// The em that makes capitals `height` tall.
pub fn em_for(height: f32) -> f32 {
    height * 7.0 / 5.0
}

/// Text with a dark copy behind it, for reading over pictures.
pub fn shadowed(batch: &mut Batch, x: f32, y: f32, px: f32, colour: [u8; 4], s: &str) -> f32 {
    let d = (px / 18.0).max(1.0);
    text(batch, x + d, y + d, px, [0, 0, 0, colour[3]], s);
    text(batch, x, y, px, colour, s)
}

/// Text centred on a point.
pub fn centred(batch: &mut Batch, cx: f32, cy: f32, px: f32, colour: [u8; 4], s: &str) {
    text(batch, (cx - text_width(px, s) * 0.5).round(), (cy - cap(px) * 0.5).round(), px, colour, s);
}

/// Text ending at `right`.
pub fn right_aligned(batch: &mut Batch, right: f32, y: f32, px: f32, colour: [u8; 4], s: &str) {
    text(batch, (right - text_width(px, s)).round(), y, px, colour, s);
}

/// A button the way McOsu's menus have them: a coloured slab with its
/// label in the middle, lighter under the pointer.
pub fn button(batch: &mut Batch, r: Rect, colour: [u8; 3], label: &str, px: f32, hot: bool) {
    let lift = |c: u8| if hot { c.saturating_add(40) } else { c };
    let radius = 4.0;
    batch.rrect(r.x, r.y, r.w, r.h, radius, [lift(colour[0]), lift(colour[1]), lift(colour[2]), 235]);
    batch.frame(r.x, r.y, r.w, r.h, radius, (r.h * 0.04).max(1.0), [255, 255, 255, if hot { 200 } else { 90 }]);
    let (cx, cy) = r.centre();
    centred(batch, cx, cy, px, WHITE, fit(px, label, r.w - r.h * 0.4));
}

/// A horizontal slider: a line, and a block where the value is. Returns
/// the value asked for while it is being dragged.
pub fn slider(batch: &mut Batch, input: &Input, r: Rect, value: f32, grabbed: bool) -> Option<f32> {
    let block = r.h * 0.8;
    let (x0, x1) = (r.x + block * 0.5, r.x + r.w - block * 0.5);
    let at = x0 + (x1 - x0) * value.clamp(0.0, 1.0);
    let line = (r.h * 0.08).max(1.0);
    let cy = r.y + r.h * 0.5;
    batch.rect(r.x, cy - line * 0.5, r.w, line, [255, 255, 255, 110]);
    batch.rect(r.x, cy - line * 0.5, at - r.x, line, [0, 190, 255, 255]);
    let hot = grabbed || input.over(r);
    batch.rrect(
        at - block * 0.5,
        cy - block * 0.5,
        block,
        block,
        block * 0.2,
        if hot { WHITE } else { [210, 210, 210, 255] },
    );
    grabbed.then(|| ((input.x - x0) / (x1 - x0)).clamp(0.0, 1.0))
}

/// The value (0 to 1) a slider in `r` shows with its block at window x.
pub fn slider_value_at(r: Rect, x: f32) -> f32 {
    let block = r.h * 0.8;
    let (x0, x1) = (r.x + block * 0.5, r.x + r.w - block * 0.5);
    ((x - x0) / (x1 - x0)).clamp(0.0, 1.0)
}

/// A tick box with its label to the right.
pub fn checkbox(batch: &mut Batch, input: &Input, r: Rect, on: bool, label: &str, px: f32) {
    let side = r.h * 0.7;
    let (bx, by) = (r.x, r.y + (r.h - side) * 0.5);
    let hot = input.over(r);
    batch.frame(bx, by, side, side, side * 0.15, (side * 0.1).max(1.0), if hot { WHITE } else { [200, 200, 200, 255] });
    if on {
        let inset = side * 0.24;
        batch.rrect(bx + inset, by + inset, side - 2.0 * inset, side - 2.0 * inset, side * 0.08, [0, 190, 255, 255]);
    }
    text(batch, bx + side * 1.5, r.y + (r.h - cap(px)) * 0.5, px, WHITE, label);
}

/// A scrolling area that glides: the wheel and keys move a target the
/// position follows, and a drag with the pointer moves it directly and
/// lets go with whatever speed it had.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scroll {
    pub pos: f32,
    target: f32,
    velocity: f32,
    /// Where the pointer went down, and the position at that moment.
    grab: Option<(f32, f32)>,
    /// Whether the pointer has moved far enough since going down for this
    /// to be a drag and not a click.
    pub dragging: bool,
}

impl Scroll {
    /// Advance by one frame. `area` is where a drag may start, `content`
    /// how tall what is being scrolled is in all, `view` how much of it
    /// shows. Returns true if the pointer was let go without a drag: a
    /// click.
    pub fn update(&mut self, input: &Input, area: Rect, content: f32, view: f32, notch: f32, slop: f32) -> bool {
        let max = (content - view).max(0.0);
        let mut click = false;
        if input.pressed && input.over(area) {
            self.grab = Some((input.y, self.pos));
            self.dragging = false;
            self.velocity = 0.0;
        }
        match self.grab {
            Some((from, start)) if input.down => {
                if (input.y - from).abs() > slop {
                    self.dragging = true;
                }
                if self.dragging {
                    let before = self.pos;
                    self.pos = start - (input.y - from);
                    if input.dt > 0.0 {
                        let now = (self.pos - before) / input.dt;
                        self.velocity = self.velocity * 0.6 + now * 0.4;
                    }
                    self.target = self.pos;
                }
            }
            Some(_) => {
                click = !self.dragging;
                if self.dragging {
                    self.target = self.pos + self.velocity * 0.35;
                }
                self.grab = None;
                self.dragging = false;
            }
            None => {}
        }
        if input.scroll != 0.0 && input.over(area) {
            self.target += input.scroll * notch;
        }
        if self.grab.is_none() {
            // Past the ends it gives a little and comes back.
            self.target = self.target.clamp(0.0, max);
            approach(&mut self.pos, self.target, input.dt, 0.3);
        }
        click
    }

    pub fn to(&mut self, target: f32) {
        self.target = target;
    }

    pub fn jump(&mut self, pos: f32) {
        self.pos = pos;
        self.target = pos;
        self.velocity = 0.0;
    }

    pub fn by(&mut self, delta: f32) {
        self.target += delta;
    }
}

/// The background image now in the renderer's background texture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Backdrop {
    pub w: f32,
    pub h: f32,
}

impl Backdrop {
    /// Draw the image so that it fills the window, cut where it has to be
    /// (McOsu's `getImageScaleToFillResolution`), tinted by `colour`.
    pub fn draw(&self, batch: &mut Batch, m: &Metrics, colour: [u8; 4]) {
        if self.w < 1.0 || self.h < 1.0 || colour[3] == 0 {
            return;
        }
        // The part of the image that shows, in its own pixels.
        let scale = (m.w / self.w).max(m.h / self.h);
        let (vw, vh) = (m.w / scale, m.h / scale);
        let (x0, y0) = ((self.w - vw) * 0.5, (self.h - vh) * 0.5);
        // Half a pixel in from the edges, where filtering would reach
        // what is beside the image in the texture.
        let u = |x: f32| ((x.clamp(0.5, self.w - 0.5) / BACKGROUND_SIZE.0 as f32) * 65535.0) as u16;
        let v = |y: f32| ((y.clamp(0.5, self.h - 0.5) / BACKGROUND_SIZE.1 as f32) * 65535.0) as u16;
        let uv = [u(x0), v(y0), u(x0 + vw), v(y0 + vh)];
        batch.sprite_on(PAGE_BACKGROUND, m.w * 0.5, m.h * 0.5, [m.w * 0.5, m.h * 0.5], uv, colour, 0.0);
    }
}

/// A message that shows for a moment in a bar across the middle of the
/// screen, as McOsu's notifications do.
pub struct Notice {
    text: text::StackStr<160>,
    since_ns: u64,
    colour: [u8; 4],
}

impl Notice {
    pub fn new() -> Notice {
        Notice { text: text::StackStr::new(), since_ns: 0, colour: WHITE }
    }

    pub fn show(&mut self, now_ns: u64, colour: [u8; 4], args: std::fmt::Arguments) {
        self.text.clear();
        let _ = std::fmt::Write::write_fmt(&mut self.text, args);
        self.since_ns = now_ns.max(1);
        self.colour = colour;
    }

    pub fn draw(&self, batch: &mut Batch, m: &Metrics, now_ns: u64) {
        const HOLD: f32 = 1.2;
        const FADE: f32 = 0.5;
        if self.since_ns == 0 {
            return;
        }
        let age = now_ns.saturating_sub(self.since_ns) as f32 / 1e9;
        if age > HOLD + FADE {
            return;
        }
        let a = (1.0 - (age - HOLD) / FADE).clamp(0.0, 1.0);
        let px = m.px(FONT_SUBTITLE);
        let height = cap(px) * 3.0;
        batch.rect(0.0, (m.h - height) * 0.5, m.w, height, [0, 0, 0, (150.0 * a) as u8]);
        centred(batch, m.w * 0.5, m.h * 0.5, px, alpha(self.colour, a), self.text.as_str());
    }
}

/// The volume, shown while it is being changed: McOsu's bars at the
/// bottom right.
pub struct VolumeOverlay {
    until_ns: u64,
}

impl VolumeOverlay {
    pub fn new() -> VolumeOverlay {
        VolumeOverlay { until_ns: 0 }
    }

    pub fn poke(&mut self, now_ns: u64) {
        self.until_ns = now_ns + 1_500_000_000;
    }

    /// `levels` are master, music and effects, each 0..1.
    pub fn draw(&self, batch: &mut Batch, m: &Metrics, now_ns: u64, levels: [f32; 3]) {
        if now_ns >= self.until_ns {
            return;
        }
        let a = ((self.until_ns - now_ns) as f32 / 0.4e9).clamp(0.0, 1.0);
        let (w, gap) = (m.px(360.0), m.px(8.0));
        let x = m.w - w - m.px(16.0);
        let mut y = m.h - m.px(16.0);
        let px = m.px(FONT_DEFAULT) * 0.8;
        // Effects at the bottom, music above it, the master above both
        // and larger: it is the one the wheel moves.
        for (label, value, h) in
            [("EFFECTS", levels[2], m.px(22.0)), ("MUSIC", levels[1], m.px(22.0)), ("MASTER", levels[0], m.px(34.0))]
        {
            y -= h;
            batch.rrect(x, y, w, h, m.px(4.0), [0, 0, 0, (190.0 * a) as u8]);
            batch.rrect(x, y, w * value.clamp(0.0, 1.0), h, m.px(4.0), [255, 255, 255, (215.0 * a) as u8]);
            batch.frame(x, y, w, h, m.px(4.0), 1.0, alpha(WHITE, a));
            let mut line = text::StackStr::<32>::new();
            let _ = std::fmt::Write::write_fmt(&mut line, format_args!("{label} {:.0}%", value * 100.0));
            shadowed(batch, x + gap, y + (h - cap(px)) * 0.5, px, alpha([255, 80, 160, 255], a), line.as_str());
            y -= gap;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_uses_subtle_matching_rounded_fill_and_border() {
        let mut instances = [std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 128];
        let mut batch = unsafe { Batch::over(instances.as_mut_ptr().cast(), instances.len()) };
        button(&mut batch, Rect::new(0.0, 0.0, 200.0, 60.0), [30, 40, 50], "", 20.0, false);
        assert!(batch.len() >= 2);
        let fill = unsafe { instances[0].assume_init() };
        let border = unsafe { instances[1].assume_init() };
        assert_eq!(fill.kind, rsu_gfx::KIND_RRECT);
        assert_eq!(fill.p0, 4.0);
        assert_eq!(border.p0, fill.p0);
    }

    fn input(y: f32, pressed: bool, down: bool) -> Input<'static> {
        Input { x: 10.0, y, pressed, down, scroll: 0.0, keys: &[], text: "", dt: 0.01, now_ns: 0 }
    }

    #[test]
    fn sizes_follow_the_window_as_mcosu_has_them() {
        let m = Metrics::new(1920.0, 1080.0);
        assert_eq!((m.osu(64.0), m.px(20.0)), (144.0, 20.0));
        // A wide and low window is laid out for the 4:3 that fits into it.
        assert_eq!(Metrics::new(4000.0, 480.0).osu(10.0), 10.0);
        assert_eq!(Metrics::new(640.0, 2000.0).osu(10.0), 10.0);
        // An image of 699x103 comes out 64 osu! pixels tall.
        assert!((m.image_scale(699.0, 103.0, 64.0) * 103.0 - 144.0).abs() < 1e-3);
    }

    #[test]
    fn a_press_and_release_in_place_is_a_click_and_a_drag_is_not() {
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        let mut s = Scroll::default();
        assert!(!s.update(&input(50.0, true, true), area, 1000.0, 100.0, 30.0, 8.0));
        assert!(s.update(&input(52.0, false, false), area, 1000.0, 100.0, 30.0, 8.0), "a click");
        assert_eq!(s.pos, 0.0);

        assert!(!s.update(&input(80.0, true, true), area, 1000.0, 100.0, 30.0, 8.0));
        assert!(!s.update(&input(40.0, false, true), area, 1000.0, 100.0, 30.0, 8.0));
        assert_eq!(s.pos, 40.0, "the content follows the pointer");
        assert!(!s.update(&input(40.0, false, false), area, 1000.0, 100.0, 30.0, 8.0), "a drag is not a click");
        // Let go, it keeps going for a while, and never past the end.
        for _ in 0..1000 {
            s.update(&input(40.0, false, false), area, 1000.0, 100.0, 30.0, 8.0);
        }
        assert!(s.pos > 40.0 && s.pos <= 900.0, "{}", s.pos);
    }

    #[test]
    fn keys_type_by_position() {
        assert_eq!(key_char(16), Some('q'));
        assert_eq!(key_char(11), Some('0'));
        assert_eq!(key_char(KEY_SPACE), Some(' '));
        assert_eq!(key_char(KEY_ENTER), None);
    }
}
