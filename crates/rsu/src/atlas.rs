//! The CPU side of the renderer's texture atlas: a shelf packer that images
//! are added to at load time, kept so that part of it (the skin) can be
//! thrown away and packed again while the rest (the font) stays put.

use rsu_gfx::ATLAS_SIZE;

const SIZE: usize = ATLAS_SIZE as usize;

pub struct Atlas {
    /// Premultiplied RGBA, `ATLAS_SIZE` pixels wide. Zeroed pages that were
    /// never written cost no memory.
    rgba: Vec<u8>,
    x: usize,
    y: usize,
    shelf_h: usize,
}

/// A packing position to return to.
#[derive(Clone, Copy)]
pub struct Mark {
    y: usize,
}

impl Atlas {
    pub fn new() -> Atlas {
        // Pixel (0, 0) stays transparent: it is what an empty sprite samples.
        Atlas { rgba: vec![0; SIZE * SIZE * 4], x: 1, y: 1, shelf_h: 0 }
    }

    /// Room for a `w` x `h` image with a transparent pixel around it, so
    /// filtering at its edges never reaches a neighbour.
    fn place(&mut self, w: usize, h: usize) -> Option<(usize, usize)> {
        if w == 0 || h == 0 || w + 2 > SIZE {
            return None;
        }
        if self.x + w + 1 > SIZE {
            self.y += self.shelf_h + 1;
            self.x = 1;
            self.shelf_h = 0;
        }
        if self.y + h + 1 > SIZE {
            return None;
        }
        let at = (self.x, self.y);
        self.x += w + 1;
        self.shelf_h = self.shelf_h.max(h);
        Some(at)
    }

    fn uv(x: usize, y: usize, w: usize, h: usize) -> [u16; 4] {
        let f = |v: usize| ((v * 65535 + SIZE / 2) / SIZE) as u16;
        [f(x), f(y), f(x + w), f(y + h)]
    }

    /// Add straight-alpha RGBA pixels. `None` when the atlas is full.
    pub fn add_rgba(&mut self, w: usize, h: usize, pixels: &[u8]) -> Option<[u16; 4]> {
        let (x, y) = self.place(w, h)?;
        for row in 0..h {
            let out = &mut self.rgba[((y + row) * SIZE + x) * 4..][..w * 4];
            for (o, p) in out.chunks_exact_mut(4).zip(pixels[row * w * 4..].chunks_exact(4)) {
                let a = p[3] as u32;
                let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                o.copy_from_slice(&[m(p[0]), m(p[1]), m(p[2]), p[3]]);
            }
        }
        Some(Self::uv(x, y, w, h))
    }

    /// Add a coverage mask (a font glyph) as white.
    pub fn add_coverage(&mut self, w: usize, h: usize, coverage: &[u8]) -> Option<[u16; 4]> {
        let (x, y) = self.place(w, h)?;
        for row in 0..h {
            let out = &mut self.rgba[((y + row) * SIZE + x) * 4..][..w * 4];
            for (o, &c) in out.chunks_exact_mut(4).zip(&coverage[row * w..]) {
                o.copy_from_slice(&[c; 4]);
            }
        }
        Some(Self::uv(x, y, w, h))
    }

    /// Finish the current shelf and remember where the next one starts.
    pub fn mark(&mut self) -> Mark {
        self.y += self.shelf_h + 1;
        self.x = 1;
        self.shelf_h = 0;
        Mark { y: self.y }
    }

    /// Forget everything added after `mark`.
    pub fn rewind(&mut self, mark: Mark) {
        let end = (self.rows() as usize * SIZE * 4).max(mark.y * SIZE * 4);
        self.rgba[mark.y * SIZE * 4..end].fill(0);
        (self.x, self.y, self.shelf_h) = (1, mark.y, 0);
    }

    /// How many rows from the top are in use, and their pixels: what the
    /// renderer has to be given.
    pub fn rows(&self) -> u32 {
        (self.y + self.shelf_h + 1).min(SIZE) as u32
    }

    pub fn pixels(&self) -> &[u8] {
        &self.rgba[..self.rows() as usize * SIZE * 4]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_premultiplied_padded_and_can_be_rewound() {
        let mut atlas = Atlas::new();
        let a = atlas.add_coverage(2, 2, &[255, 0, 0, 128]).unwrap();
        assert_eq!(a, Atlas::uv(1, 1, 2, 2));
        let mark = atlas.mark();
        let b = atlas.add_rgba(1, 1, &[200, 100, 0, 128]).unwrap();
        assert_eq!(b, Atlas::uv(1, 4, 1, 1), "a new shelf, one pixel below the first");
        let px = |atlas: &Atlas, x: usize, y: usize| atlas.rgba[(y * SIZE + x) * 4..][..4].to_vec();
        assert_eq!(px(&atlas, 1, 4), [100, 50, 0, 128]);
        assert_eq!(px(&atlas, 1, 1), [255; 4]);
        assert_eq!(px(&atlas, 0, 0), [0; 4]);
        assert_eq!(atlas.rows(), 6);
        atlas.rewind(mark);
        assert_eq!(px(&atlas, 1, 4), [0; 4]);
        assert_eq!(px(&atlas, 2, 2), [128; 4], "what came before the mark stays");
        assert_eq!(atlas.add_rgba(1, 1, &[0, 0, 0, 255]), Some(b));
        // Too wide for the atlas, and too many to fit.
        assert!(atlas.add_coverage(SIZE, 1, &vec![0; SIZE]).is_none());
        let big = vec![0u8; 1000 * 1000];
        assert_eq!((0..40).filter(|_| atlas.add_coverage(1000, 1000, &big).is_some()).count(), 16);
    }
}
