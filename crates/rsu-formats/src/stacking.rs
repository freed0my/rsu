//! Stacking: objects placed on the same spot close in time are fanned out
//! diagonally so each one can be seen. This moves hit positions, so it is
//! part of the rules, not decoration.
//!
//! Two algorithms exist, as in the game: the current one (file format v6
//! and later) and the original one for older maps.

use crate::curve::{dist, P};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Circle,
    Slider,
    Spinner,
}

/// What stacking needs to know about an object. Times in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub kind: Kind,
    pub start: f64,
    pub end: f64,
    pub pos: P,
    /// Where a slider's ball ends up; equal to `pos` for circles.
    pub end_pos: P,
}

/// Objects closer than this (osu! pixels) count as being on the same spot.
const STACK_DISTANCE: f64 = 3.0;

/// Returns the stack height of every object: positive moves it up-left,
/// negative down-right, each step being 6.4 px times the object scale.
pub fn stack_heights(items: &[Item], preempt_ms: f64, leniency: f64, format_version: u32) -> Vec<i32> {
    let mut h = vec![0i32; items.len()];
    let threshold = preempt_ms * leniency;
    if format_version >= 6 {
        new_stacking(items, threshold, &mut h);
    } else {
        old_stacking(items, threshold, &mut h);
    }
    h
}

fn new_stacking(items: &[Item], threshold: f64, h: &mut [i32]) {
    let near = |a: P, b: P| dist(a, b) < STACK_DISTANCE;
    // Walk backwards; each object not yet in a stack becomes the top of one
    // and pulls the earlier objects under it.
    for i in (1..items.len()).rev() {
        if h[i] != 0 || items[i].kind == Kind::Spinner {
            continue;
        }
        let mut top = i; // the object the next one would stack onto
        if items[i].kind == Kind::Circle {
            for n in (0..i).rev() {
                let on = items[n];
                if on.kind == Kind::Spinner {
                    continue;
                }
                if items[top].start - on.end > threshold {
                    break;
                }
                // Circles sitting on the *end* of a slider are pushed the
                // other way (down-right), so the slider stays where it is.
                if on.kind == Kind::Slider && near(on.end_pos, items[top].pos) {
                    let offset = h[top] - h[n] + 1;
                    for j in n + 1..=i {
                        if near(on.end_pos, items[j].pos) {
                            h[j] -= offset;
                        }
                    }
                    // The slider itself is handled when the outer loop reaches it.
                    break;
                }
                if near(on.pos, items[top].pos) {
                    h[n] = h[top] + 1;
                    top = n;
                }
            }
        } else {
            // A slider: everything ending on its head stacks upwards.
            for n in (0..i).rev() {
                let on = items[n];
                if on.kind == Kind::Spinner {
                    continue;
                }
                if items[top].start - on.start > threshold {
                    break;
                }
                if near(on.end_pos, items[top].pos) {
                    h[n] = h[top] + 1;
                    top = n;
                }
            }
        }
    }
}

fn old_stacking(items: &[Item], threshold: f64, h: &mut [i32]) {
    for i in 0..items.len() {
        if h[i] != 0 && items[i].kind != Kind::Slider {
            continue;
        }
        let cur = items[i];
        let mut start_time = cur.end;
        let mut slider_stack = 0;
        for j in i + 1..items.len() {
            if items[j].start - threshold > start_time {
                break;
            }
            if dist(items[j].pos, cur.pos) < STACK_DISTANCE {
                h[i] += 1;
                start_time = items[j].start;
            } else if dist(items[j].pos, cur.end_pos) < STACK_DISTANCE {
                // Under a slider's end: bump down and right instead.
                slider_stack += 1;
                h[j] -= slider_stack;
                start_time = items[j].start;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(ms: f64, x: f64, y: f64) -> Item {
        Item { kind: Kind::Circle, start: ms, end: ms, pos: [x, y], end_pos: [x, y] }
    }

    fn slider(ms: f64, end: f64, from: P, to: P) -> Item {
        Item { kind: Kind::Slider, start: ms, end, pos: from, end_pos: to }
    }

    // AR 9 (600 ms) with the default leniency 0.7: objects up to 420 ms apart stack.
    const PREEMPT: f64 = 600.0;

    #[test]
    fn a_stack_of_circles_fans_upwards_from_the_last() {
        let items = [circle(1000.0, 100.0, 100.0), circle(1200.0, 100.0, 100.0), circle(1400.0, 101.0, 100.0)];
        assert_eq!(stack_heights(&items, PREEMPT, 0.7, 14), vec![2, 1, 0]);
    }

    #[test]
    fn distance_and_time_limits_apply() {
        // 3 px apart is not "the same spot"; 2.9 px is.
        let far = [circle(1000.0, 100.0, 100.0), circle(1200.0, 103.0, 100.0)];
        assert_eq!(stack_heights(&far, PREEMPT, 0.7, 14), vec![0, 0]);
        let close = [circle(1000.0, 100.0, 100.0), circle(1200.0, 102.9, 100.0)];
        assert_eq!(stack_heights(&close, PREEMPT, 0.7, 14), vec![1, 0]);
        // Same spot but 421 ms apart: too long ago.
        let late = [circle(1000.0, 100.0, 100.0), circle(1421.0, 100.0, 100.0)];
        assert_eq!(stack_heights(&late, PREEMPT, 0.7, 14), vec![0, 0]);
        // Zero leniency disables stacking for anything not simultaneous.
        assert_eq!(stack_heights(&close, PREEMPT, 0.0, 14), vec![0, 0]);
    }

    #[test]
    fn circles_under_a_slider_end_go_down_instead() {
        let items = [
            slider(1000.0, 1300.0, [50.0, 50.0], [200.0, 50.0]),
            circle(1400.0, 200.0, 50.0),
            circle(1500.0, 200.0, 50.0),
        ];
        assert_eq!(stack_heights(&items, PREEMPT, 0.7, 14), vec![0, -1, -2]);
    }

    #[test]
    fn objects_ending_on_a_slider_head_stack_up() {
        let items = [
            circle(1000.0, 100.0, 100.0),
            circle(1100.0, 100.0, 100.0),
            slider(1200.0, 1500.0, [100.0, 100.0], [300.0, 100.0]),
        ];
        assert_eq!(stack_heights(&items, PREEMPT, 0.7, 14), vec![2, 1, 0]);
    }

    #[test]
    fn interleaved_stacks_stay_separate() {
        let items = [
            circle(1000.0, 100.0, 100.0),
            circle(1100.0, 300.0, 100.0),
            circle(1200.0, 100.0, 100.0),
            circle(1300.0, 300.0, 100.0),
        ];
        assert_eq!(stack_heights(&items, PREEMPT, 0.7, 14), vec![1, 1, 0, 0]);
    }

    #[test]
    fn spinners_neither_stack_nor_interrupt_a_stack() {
        let spinner =
            Item { kind: Kind::Spinner, start: 1100.0, end: 1150.0, pos: [256.0, 192.0], end_pos: [256.0, 192.0] };
        let items = [circle(1000.0, 256.0, 192.0), spinner, circle(1200.0, 256.0, 192.0)];
        assert_eq!(stack_heights(&items, PREEMPT, 0.7, 14), vec![1, 0, 0]);
    }

    #[test]
    fn old_maps_count_from_the_first_object() {
        // The original algorithm raises the *earlier* object once per follower.
        let items = [circle(1000.0, 100.0, 100.0), circle(1200.0, 100.0, 100.0), circle(1400.0, 100.0, 100.0)];
        assert_eq!(stack_heights(&items, PREEMPT, 0.7, 5), vec![2, 1, 0]);
        let under = [slider(1000.0, 1300.0, [50.0, 50.0], [200.0, 50.0]), circle(1400.0, 200.0, 50.0)];
        assert_eq!(stack_heights(&under, PREEMPT, 0.7, 5), vec![0, -1]);
    }
}
