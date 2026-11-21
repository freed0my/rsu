//! Cursor smoke, as osu! stable has it: while the smoke key is held, the
//! cursor leaves a trail of the skin's `cursor-smoke` image that fades away.
//! It is only drawn; it has no part in the game.

use crate::skin::Sprite;
use rsu_gfx::{Batch, FLAG_TRAIL};

/// How long a puff of smoke stays.
const LIFE_NS: u64 = 2_500_000_000;
/// How long a point of the cursor trail stays.
const TRAIL_LIFE_NS: u64 = 170_000_000;
/// The most puffs kept (and drawn): the instance buffer is shared.
const MAX: usize = 700;
/// The strongest a puff is drawn, of 255.
const PEAK: f32 = 110.0;

#[derive(Clone, Copy)]
struct Puff {
    x: f32,
    y: f32,
    t_ns: u64,
}

pub struct Smoke {
    life_ns: u64,
    peak: f32,
    /// Fade out quadratically (smoke) or linearly (trail).
    quadratic: bool,
    puffs: Vec<Puff>,
    /// Where the trail goes on from while the key stays down.
    last: Option<(f32, f32)>,
}

impl Default for Smoke {
    fn default() -> Self {
        Smoke { life_ns: LIFE_NS, peak: PEAK, quadratic: true, puffs: Vec::new(), last: None }
    }
}

impl Smoke {
    /// The cursor trail: the skin's `cursortrail` image along the path the
    /// cursor took, gone again after a moment. Fed with `held` always true.
    pub fn trail() -> Self {
        Smoke { life_ns: TRAIL_LIFE_NS, peak: 255.0, quadratic: false, puffs: Vec::new(), last: None }
    }

    /// One frame: let old puffs go and, while `held`, add the ones between
    /// the last position and (x, y), `spacing` pixels apart.
    pub fn update(&mut self, now_ns: u64, x: f32, y: f32, held: bool, spacing: f32) {
        self.puffs.retain(|p| now_ns.saturating_sub(p.t_ns) < self.life_ns);
        if !held {
            self.last = None;
            return;
        }
        let spacing = spacing.max(1.0);
        match self.last {
            // The first frame with the key down: one puff where the cursor is.
            None => {
                self.puffs.push(Puff { x, y, t_ns: now_ns });
                self.last = Some((x, y));
            }
            Some((lx, ly)) => {
                let distance = ((x - lx).powi(2) + (y - ly).powi(2)).sqrt();
                // A jump of the cursor does not fill the screen with puffs.
                let steps = ((distance / spacing) as usize).min(48);
                for k in 1..=steps {
                    let f = k as f32 / steps as f32;
                    self.puffs.push(Puff { x: lx + (x - lx) * f, y: ly + (y - ly) * f, t_ns: now_ns });
                }
                if steps > 0 {
                    self.last = Some((x, y));
                }
            }
        }
        if self.puffs.len() > MAX {
            let extra = self.puffs.len() - MAX;
            self.puffs.drain(..extra);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.puffs.is_empty()
    }

    /// Draw oldest first, so the newest lies on top. `unit` is the size of
    /// one of the skin's pixels.
    pub fn draw(&self, batch: &mut Batch, sprite: Option<&Sprite>, unit: f32, now_ns: u64, opacity: f32) {
        for p in &self.puffs {
            let age = now_ns.saturating_sub(p.t_ns) as f32 / self.life_ns as f32;
            let left = (1.0 - age).clamp(0.0, 1.0);
            // Keep the existing trail tint; shader premultiplies by alpha.
            let a = (if self.quadratic { left * left } else { left } * self.peak) as u8;
            if a == 0 {
                continue;
            }
            let color = [a, a, a, (a as f32 * opacity.clamp(0.0, 1.0)).round() as u8];
            let flags = if self.quadratic { 0 } else { FLAG_TRAIL };
            match sprite {
                Some(s) => s.draw_turned(batch, p.x, p.y, unit, color, 0.0, flags),
                None => batch.disc(p.x, p.y, 7.0 * unit, color, flags),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_trail_hides_existing_live_stamps() {
        let mut trail = Smoke::trail();
        trail.update(0, 10.0, 10.0, true, 1.0);
        let mut storage = [std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4];
        let mut batch = unsafe { Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        trail.draw(&mut batch, Some(&Sprite::default()), 1.0, 1, 1.0);
        assert_eq!(batch.len(), 0, "a transparent skin image must not turn old stamps into dots");
    }

    #[test]
    fn trail_fading_multiplies_existing_alpha_only() {
        let mut trail = Smoke::trail();
        trail.update(0, 10.0, 10.0, true, 1.0);
        let mut storage = [std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4];
        let mut batch = unsafe { Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        trail.draw(&mut batch, None, 1.0, TRAIL_LIFE_NS / 2, 0.5);
        assert_eq!(batch.len(), 1);
        assert_eq!(unsafe { storage[0].assume_init() }.color, [127, 127, 127, 64]);
    }

    #[test]
    fn a_trail_follows_the_cursor_while_held_and_fades() {
        let mut s = Smoke::default();
        s.update(0, 100.0, 100.0, true, 10.0);
        assert_eq!(s.puffs.len(), 1, "the first frame leaves one puff");
        s.update(1_000_000, 150.0, 100.0, true, 10.0);
        assert_eq!(s.puffs.len(), 6, "50 px at 10 px apart: five more");
        // Nothing moved: nothing added.
        s.update(2_000_000, 150.0, 100.0, true, 10.0);
        assert_eq!(s.puffs.len(), 6);
        // Released: no more, and the trail starts afresh next time.
        s.update(3_000_000, 300.0, 300.0, false, 10.0);
        s.update(4_000_000, 500.0, 500.0, true, 10.0);
        assert_eq!(s.puffs.len(), 7);
        // A long jump is capped.
        s.update(5_000_000, 5000.0, 500.0, true, 10.0);
        assert!(s.puffs.len() <= 7 + 48);
        // Everything is gone after its life.
        s.update(10_000_000_000, 0.0, 0.0, false, 10.0);
        assert!(s.is_empty());
        // The trail is gone much sooner.
        let mut t = Smoke::trail();
        t.update(0, 0.0, 0.0, true, 10.0);
        t.update(1_000_000, 30.0, 0.0, true, 10.0);
        assert!(!t.is_empty());
        t.update(TRAIL_LIFE_NS + 2_000_000, 30.0, 0.0, true, 10.0);
        assert!(t.is_empty());
    }
}
