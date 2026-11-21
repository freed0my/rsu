//! Allocation-free text. Two fonts: a system font, rasterised into the atlas
//! once at start, for everything people read; and a 3x5 bitmap font drawn by
//! the shader from a 15-bit mask, which the measurements overlay keeps (its
//! columns want fixed-width characters) and which stands in when no system
//! font can be found. Plus a stack string to format into.
//!
//! Both are placed the same way: `scale` is the size of one pixel of the
//! bitmap font, capital letters are 5 of them tall and start at `y`.

use crate::atlas::Atlas;
use std::sync::OnceLock;
use rsu_gfx::{Batch, Inst, KIND_GLYPH};

/// The system font's em size in units of `scale`: capitals come out 5 tall.
const EM: f32 = 7.0;
/// Pixel sizes the font is rasterised at. Text is drawn from the smallest
/// one that is not noticeably smaller than what is asked for.
const SIZES: [f32; 5] = [18.0, 28.0, 44.0, 72.0, 128.0];
/// What is rasterised: Basic Latin, Latin-1, Latin Extended-A, Cyrillic and
/// a few signs that song titles like. The largest size has Basic Latin only.
const RANGES: [(u32, u32); 4] = [(0x20, 0x7e), (0xa0, 0x17f), (0x400, 0x45f), (0x2010, 0x2026)];
const EXTRA: [char; 8] =
    ['\u{2190}', '\u{2192}', '\u{2605}', '\u{2606}', '\u{2661}', '\u{2665}', '\u{266a}', '\u{266b}'];

fn slot(c: char) -> Option<usize> {
    let (c, mut base) = (c as u32, 0);
    for (lo, hi) in RANGES {
        if (lo..=hi).contains(&c) {
            return Some(base + (c - lo) as usize);
        }
        base += (hi - lo + 1) as usize;
    }
    EXTRA.iter().position(|&e| e as u32 == c).map(|i| base + i)
}

#[derive(Clone, Copy, Default)]
struct Glyph {
    uv: [u16; 4],
    /// Bitmap size, and where its top-left corner sits relative to the pen
    /// on the baseline; all in pixels of the size it was rasterised at.
    size: [f32; 2],
    offset: [f32; 2],
    advance: f32,
    present: bool,
}

pub struct Font {
    /// Per entry of `SIZES`, a glyph per slot.
    sizes: Vec<Vec<Glyph>>,
}

static FONT: OnceLock<Font> = OnceLock::new();

impl Font {
    /// Rasterise a TrueType or OpenType font into the atlas.
    pub fn rasterise(data: &[u8], atlas: &mut Atlas) -> Result<Font, String> {
        let font = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())?;
        let slots = RANGES.iter().map(|(lo, hi)| (hi - lo + 1) as usize).sum::<usize>() + EXTRA.len();
        let chars = RANGES.iter().flat_map(|&(lo, hi)| lo..=hi).filter_map(char::from_u32).chain(EXTRA);
        let chars: Vec<char> = chars.collect();
        let mut sizes = Vec::new();
        for (n, px) in SIZES.into_iter().enumerate() {
            let mut glyphs = vec![Glyph::default(); slots];
            for &c in &chars {
                let basic = (c as u32) < 0x7f;
                if font.lookup_glyph_index(c) == 0 || (n + 1 == SIZES.len() && !basic) {
                    continue;
                }
                let (m, coverage) = font.rasterize(c, px);
                let uv = if m.width == 0 || m.height == 0 {
                    [0; 4]
                } else {
                    match atlas.add_coverage(m.width, m.height, &coverage) {
                        Some(uv) => uv,
                        None => continue,
                    }
                };
                glyphs[slot(c).unwrap()] = Glyph {
                    uv,
                    size: [m.width as f32, m.height as f32],
                    offset: [m.xmin as f32, -(m.ymin as f32 + m.height as f32)],
                    advance: m.advance_width,
                    present: true,
                };
            }
            sizes.push(glyphs);
        }
        Ok(Font { sizes })
    }

    /// The glyph to draw `c` with at an em of `em` pixels, and the factor
    /// from its pixels to the screen's.
    fn glyph(&self, c: char, em: f32) -> Option<(&Glyph, f32)> {
        let slot = slot(c)?;
        let first = SIZES.iter().position(|&px| px >= em * 0.92).unwrap_or(SIZES.len() - 1);
        // A size that lacks the glyph (the largest has few) yields to the
        // ones below it.
        (0..=first).rev().map(|n| (&self.sizes[n][slot], em / SIZES[n])).find(|(g, _)| g.present)
    }
}

/// Find the system's default sans-serif font: ask fontconfig, then look in
/// the usual places.
pub fn system_font() -> Option<Vec<u8>> {
    let asked = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif:style=Regular"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let usual = [
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/gnu-free/FreeSans.otf",
    ];
    let found =
        asked.iter().map(String::as_str).chain(usual).filter(|p| !p.is_empty()).find_map(|p| std::fs::read(p).ok());
    found
}

/// Use `font` for all text from now on. Its glyphs must stay where they are
/// in the atlas.
pub fn install(font: Font) {
    let _ = FONT.set(font);
}

/// Rows top to bottom, three bits each, leftmost pixel in the high bit.
const fn g(rows: [u8; 5]) -> u32 {
    let mut bits = 0u32;
    let mut r = 0;
    while r < 5 {
        let mut c = 0;
        while c < 3 {
            if rows[r] >> (2 - c) & 1 == 1 {
                bits |= 1 << (r * 3 + c);
            }
            c += 1;
        }
        r += 1;
    }
    bits
}

const fn glyph(ch: u8) -> u32 {
    match ch.to_ascii_uppercase() {
        b'0' => g([0b111, 0b101, 0b101, 0b101, 0b111]),
        b'1' => g([0b010, 0b110, 0b010, 0b010, 0b111]),
        b'2' => g([0b111, 0b001, 0b111, 0b100, 0b111]),
        b'3' => g([0b111, 0b001, 0b111, 0b001, 0b111]),
        b'4' => g([0b101, 0b101, 0b111, 0b001, 0b001]),
        b'5' => g([0b111, 0b100, 0b111, 0b001, 0b111]),
        b'6' => g([0b111, 0b100, 0b111, 0b101, 0b111]),
        b'7' => g([0b111, 0b001, 0b001, 0b001, 0b001]),
        b'8' => g([0b111, 0b101, 0b111, 0b101, 0b111]),
        b'9' => g([0b111, 0b101, 0b111, 0b001, 0b111]),
        b'A' => g([0b111, 0b101, 0b111, 0b101, 0b101]),
        b'B' => g([0b110, 0b101, 0b110, 0b101, 0b110]),
        b'C' => g([0b111, 0b100, 0b100, 0b100, 0b111]),
        b'D' => g([0b110, 0b101, 0b101, 0b101, 0b110]),
        b'E' => g([0b111, 0b100, 0b111, 0b100, 0b111]),
        b'F' => g([0b111, 0b100, 0b111, 0b100, 0b100]),
        b'G' => g([0b111, 0b100, 0b101, 0b101, 0b111]),
        b'H' => g([0b101, 0b101, 0b111, 0b101, 0b101]),
        b'I' => g([0b111, 0b010, 0b010, 0b010, 0b111]),
        b'J' => g([0b001, 0b001, 0b001, 0b101, 0b111]),
        b'K' => g([0b101, 0b101, 0b110, 0b101, 0b101]),
        b'L' => g([0b100, 0b100, 0b100, 0b100, 0b111]),
        b'M' => g([0b101, 0b111, 0b111, 0b101, 0b101]),
        b'N' => g([0b110, 0b101, 0b101, 0b101, 0b101]),
        b'O' => g([0b010, 0b101, 0b101, 0b101, 0b010]),
        b'P' => g([0b111, 0b101, 0b111, 0b100, 0b100]),
        b'Q' => g([0b010, 0b101, 0b101, 0b111, 0b011]),
        b'R' => g([0b110, 0b101, 0b110, 0b101, 0b101]),
        b'S' => g([0b011, 0b100, 0b010, 0b001, 0b110]),
        b'T' => g([0b111, 0b010, 0b010, 0b010, 0b010]),
        b'U' => g([0b101, 0b101, 0b101, 0b101, 0b111]),
        b'V' => g([0b101, 0b101, 0b101, 0b101, 0b010]),
        b'W' => g([0b101, 0b101, 0b111, 0b111, 0b101]),
        b'X' => g([0b101, 0b101, 0b010, 0b101, 0b101]),
        b'Y' => g([0b101, 0b101, 0b010, 0b010, 0b010]),
        b'Z' => g([0b111, 0b001, 0b010, 0b100, 0b111]),
        b'.' => g([0b000, 0b000, 0b000, 0b000, 0b010]),
        b',' => g([0b000, 0b000, 0b000, 0b010, 0b100]),
        b':' => g([0b000, 0b010, 0b000, 0b010, 0b000]),
        b'-' => g([0b000, 0b000, 0b111, 0b000, 0b000]),
        b'+' => g([0b000, 0b010, 0b111, 0b010, 0b000]),
        b'=' => g([0b000, 0b111, 0b000, 0b111, 0b000]),
        b'_' => g([0b000, 0b000, 0b000, 0b000, 0b111]),
        b'%' => g([0b101, 0b001, 0b010, 0b100, 0b101]),
        b'/' => g([0b001, 0b001, 0b010, 0b100, 0b100]),
        b'(' => g([0b010, 0b100, 0b100, 0b100, 0b010]),
        b')' => g([0b010, 0b001, 0b001, 0b001, 0b010]),
        b'>' => g([0b100, 0b010, 0b001, 0b010, 0b100]),
        b'<' => g([0b001, 0b010, 0b100, 0b010, 0b001]),
        b'#' => g([0b101, 0b111, 0b101, 0b111, 0b101]),
        b'!' => g([0b010, 0b010, 0b010, 0b000, 0b010]),
        b'?' => g([0b111, 0b001, 0b010, 0b000, 0b010]),
        b'[' => g([0b110, 0b100, 0b100, 0b100, 0b110]),
        b']' => g([0b011, 0b001, 0b001, 0b001, 0b011]),
        b'{' => g([0b011, 0b010, 0b110, 0b010, 0b011]),
        b'}' => g([0b110, 0b010, 0b011, 0b010, 0b110]),
        b'\'' | b'`' => g([0b010, 0b010, 0b000, 0b000, 0b000]),
        b'"' => g([0b101, 0b101, 0b000, 0b000, 0b000]),
        b'*' => g([0b000, 0b101, 0b010, 0b101, 0b000]),
        b'&' => g([0b010, 0b101, 0b010, 0b101, 0b011]),
        b'~' => g([0b000, 0b001, 0b111, 0b100, 0b000]),
        b';' => g([0b000, 0b010, 0b000, 0b010, 0b100]),
        b'@' => g([0b111, 0b101, 0b111, 0b100, 0b111]),
        b'$' => g([0b011, 0b110, 0b010, 0b011, 0b110]),
        b'^' => g([0b010, 0b101, 0b000, 0b000, 0b000]),
        b'|' => g([0b010, 0b010, 0b010, 0b010, 0b010]),
        b'\\' => g([0b100, 0b100, 0b010, 0b001, 0b001]),
        _ => 0,
    }
}

/// Horizontal advance per character of the bitmap font at `scale`.
pub const fn advance(scale: f32) -> f32 {
    4.0 * scale
}

/// How far a character moves the pen, with the font in use.
fn step(font: Option<&Font>, c: char, scale: f32) -> f32 {
    match font {
        Some(f) => f.glyph(c, EM * scale).map_or(EM * 0.5 * scale, |(g, k)| g.advance * k),
        None => advance(scale),
    }
}

/// The width `s` is drawn with.
pub fn width(scale: f32, s: &str) -> f32 {
    let font = FONT.get();
    let w: f32 = s.chars().map(|c| step(font, c, scale)).sum();
    // The bitmap font's last character has no gap after it.
    if font.is_none() && !s.is_empty() {
        w - scale
    } else {
        w
    }
}

/// The longest beginning of `s` that is no wider than `max_width`.
pub fn fit(scale: f32, s: &str, max_width: f32) -> &str {
    let font = FONT.get();
    let mut w = 0.0;
    for (at, c) in s.char_indices() {
        w += step(font, c, scale);
        if w > max_width {
            return &s[..at];
        }
    }
    s
}

/// Draw `s` with the top of its capitals at `y`, starting at `x`. Returns
/// the x position after the last character.
pub fn draw(batch: &mut Batch, x: f32, y: f32, scale: f32, color: [u8; 4], s: &str) -> f32 {
    let Some(font) = FONT.get() else {
        return draw_bitmap(batch, x, y, scale, color, s);
    };
    let em = EM * scale;
    let (mut pen, baseline) = (x, (y + 5.0 * scale).round());
    for c in s.chars() {
        let Some((g, k)) = font.glyph(c, em) else {
            pen += em * 0.5;
            continue;
        };
        if g.size[0] > 0.0 {
            let half = [g.size[0] * k * 0.5, g.size[1] * k * 0.5];
            // Whole pixels keep unscaled glyphs as sharp as they were made.
            let left = (pen + g.offset[0] * k).round();
            batch.sprite(left + half[0], baseline + g.offset[1] * k + half[1], half, g.uv, color, 0.0, 0);
        }
        pen += g.advance * k;
    }
    pen
}

/// Draw `s` in the fixed-width bitmap font with its top-left corner at
/// (x, y); each font pixel is `scale` screen pixels.
pub fn draw_bitmap(batch: &mut Batch, x: f32, y: f32, scale: f32, color: [u8; 4], s: &str) -> f32 {
    let half = [1.5 * scale, 2.5 * scale];
    let mut cx = x;
    for &b in s.as_bytes() {
        let bits = glyph(b);
        if bits != 0 {
            batch.push(Inst {
                center: [cx + half[0], y + half[1]],
                half,
                color,
                kind: KIND_GLYPH,
                p0: 0.0,
                p1: bits,
                uv: [0; 4],
            });
        }
        cx += advance(scale);
    }
    cx
}

/// Fixed-capacity string for `write!` in the frame; overflow is truncated.
pub struct StackStr<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackStr<N> {
    pub const fn new() -> Self {
        StackStr { buf: [0; N], len: 0 }
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
    pub fn as_str(&self) -> &str {
        // Only whole `&str`s are appended and truncation happens on a char
        // boundary, so the contents are always valid UTF-8.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> std::fmt::Write for StackStr<N> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let mut n = s.len().min(N - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    #[test]
    fn glyph_bits_are_row_major_from_the_top_left() {
        // 'L': left column and the bottom row.
        let l = glyph(b'L');
        for row in 0..5 {
            assert_eq!(l >> (row * 3) & 1, 1, "left column, row {row}");
        }
        assert_eq!(l >> (4 * 3 + 2) & 1, 1, "bottom right");
        assert_eq!(l >> 2 & 1, 0, "top right");
        assert_eq!(glyph(b'l'), l, "lower case maps to upper case");
    }

    #[test]
    fn every_rasterised_character_has_its_own_slot() {
        let mut seen = std::collections::HashSet::new();
        let chars = RANGES.iter().flat_map(|&(lo, hi)| lo..=hi).filter_map(char::from_u32).chain(EXTRA);
        for c in chars {
            assert!(seen.insert(slot(c).unwrap()), "{c:?} shares a slot");
        }
        assert_eq!(slot('\u{4e00}'), None);
    }

    #[test]
    fn without_a_font_text_is_measured_as_the_bitmap_font_draws_it() {
        assert_eq!(width(2.0, "ABC"), 22.0);
        assert_eq!(width(2.0, ""), 0.0);
        assert_eq!(fit(1.0, "ABCDEF", 12.5), "ABC");
        assert_eq!(fit(1.0, "ABC", 100.0), "ABC");
    }

    #[test]
    fn stack_str_truncates_instead_of_failing() {
        let mut s = StackStr::<8>::new();
        write!(s, "{}", 123456).unwrap();
        write!(s, "abcdef").unwrap();
        assert_eq!(s.as_str(), "123456ab");
    }
}
