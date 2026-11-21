//! The rules.
//!
//! [`Game`] is driven by two calls: [`Game::push`] for an input event and
//! [`Game::advance_to`] for the passage of time. Everything that happens
//! because time passed (a window closing, a slider tick, a spinner ending)
//! is resolved at a time computed from the map, in chronological order, and
//! never at the time of the call. So the result is the same however the
//! caller slices time into frames.
//!
//! A time-driven point due at time D is resolved once time has moved *past*
//! D. Inputs stamped exactly D are therefore applied first.

use crate::chart::{Chart, HitObject, NestedKind, ObjectKind, CENTRE, HITTABLE_RANGE};
use crate::{Fx, Time, MS};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitResult {
    Miss,
    Hit50,
    Hit100,
    Hit300,
}

impl HitResult {
    pub fn value(self) -> u32 {
        match self {
            HitResult::Miss => 0,
            HitResult::Hit50 => 50,
            HitResult::Hit100 => 100,
            HitResult::Hit300 => 300,
        }
    }
}

/// Bit values follow the osu! replay format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    M1 = 1,
    M2 = 2,
    K1 = 4,
    K2 = 8,
}

impl Button {
    pub fn from_bits(b: u8) -> Option<Button> {
        match b {
            1 => Some(Button::M1),
            2 => Some(Button::M2),
            4 => Some(Button::K1),
            8 => Some(Button::K2),
            _ => None,
        }
    }

    /// M1/K1 are one logical "left" button and M2/K2 one "right": holding
    /// one and pressing its twin is not a new click.
    fn group(self) -> u8 {
        match self {
            Button::M1 | Button::K1 => Button::M1 as u8 | Button::K1 as u8,
            Button::M2 | Button::K2 => Button::M2 as u8 | Button::K2 as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Move { x: Fx, y: Fx },
    Press(Button),
    Release(Button),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input {
    pub t: Time,
    pub kind: InputKind,
}

/// What the core reports back. `t` is always a time derived from inputs or
/// the map, so it is identical between live play and replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The final verdict on an object.
    Judged {
        object: u32,
        result: HitResult,
        t: Time,
        /// Circles: hit time minus object time. 0 for everything else.
        error: Time,
    },
    /// A click landed on an object that an earlier one still blocks.
    Blocked { object: u32, t: Time },
    /// A slider's head was hit or lost.
    SliderHead { object: u32, hit: bool, t: Time, error: Time },
    /// A slider tick, repeat or tail was reached.
    SliderPoint { object: u32, kind: NestedKind, hit: bool, t: Time },
    /// A spinner completed another full turn.
    Spin { object: u32, t: Time, bonus: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub score: u64,
    pub max_combo: u32,
    pub n300: u32,
    pub n100: u32,
    pub n50: u32,
    pub miss: u32,
    pub state_hash: u64,
}

impl Summary {
    /// 0.0..=1.0; 1.0 when nothing has been judged yet.
    pub fn accuracy(&self) -> f64 {
        let total = self.n300 + self.n100 + self.n50 + self.miss;
        if total == 0 {
            return 1.0;
        }
        (300 * self.n300 + 100 * self.n100 + 50 * self.n50) as f64 / (300 * total) as f64
    }
}

const OPEN: u8 = 0;
const MISS: u8 = 2;
const HIT50: u8 = 3;
const HIT100: u8 = 4;
const HIT300: u8 = 5;

const HEAD_PENDING: u8 = 0;
const HEAD_HIT: u8 = 1;
const HEAD_MISSED: u8 = 2;

/// stable lets a later object be hit while an earlier one is unjudged only
/// if the two are within this of each other.
const NOTELOCK_LENIENCY: Time = 3 * MS;

const SCORE_SLIDER_HEAD: u64 = 30;
const SCORE_SLIDER_TICK: u64 = 10;
const SCORE_SLIDER_REPEAT: u64 = 30;
const SCORE_SLIDER_TAIL: u64 = 30;
const SCORE_SPIN: u64 = 100;
const SCORE_SPIN_BONUS: u64 = 1000;

/// Spinning faster than this does not count for more.
const MAX_SPIN_RPM: u128 = 477;
/// One full turn in the binary angle unit used for spinners.
const TURN: u64 = 1 << 32;

/// Direction of (dx, dy) as a binary angle: the full circle is 2^32, so
/// differences wrap by themselves. Built from exact IEEE operations only;
/// the approximation is off by at most 0.25 degrees, and that error
/// cancels over a full turn.
fn angle(dx: i64, dy: i64) -> u32 {
    if dx == 0 && dy == 0 {
        return 0;
    }
    let (ax, ay) = (dx.unsigned_abs() as f64, dy.unsigned_abs() as f64);
    let q = ax.min(ay) / ax.max(ay);
    // atan(q) / 2pi for q in 0..=1.
    let mut a = q * (0.125 + 0.043_45 * (1.0 - q));
    if ay > ax {
        a = 0.25 - a;
    }
    if dx < 0 {
        a = 0.5 - a;
    }
    if dy < 0 {
        a = 1.0 - a;
    }
    (a * TURN as f64) as u64 as u32
}

/// Per-object progress. Circles only use `slot`.
#[derive(Clone, Copy, Default)]
struct ObjState {
    /// OPEN or the final result.
    slot: u8,
    /// Sliders: what became of the head.
    head: u8,
    /// Sliders: whether the player is currently following the ball.
    tracking: bool,
    /// Spinners: whether `last_angle` is a valid starting point.
    has_angle: bool,
    /// Relax: whether the automatic click at the object's time was tried.
    tried: bool,
    /// Sliders: how many nested points have been resolved.
    next_nested: u32,
    /// Sliders: parts hit so far (head, ticks, repeats, tail).
    hits: u32,
    last_angle: u32,
    last_t: Time,
    /// Spinners: total rotation, in 2^-32 turns.
    rotation: u64,
}

pub struct Game {
    chart: Arc<Chart>,
    mods: u32,
    live_spun_out: bool,
    live_no_50s: bool,
    time: Time,
    cursor: (Fx, Fx),
    buttons: u8,
    state: Vec<ObjState>,
    /// Lowest index that is still open; everything before it is settled.
    first_open: usize,
    score: u64,
    combo: u32,
    max_combo: u32,
    counts: [u32; 4], // miss, 50, 100, 300
    /// Health in [`crate::hp::FULL`] units, as of `hp_time`.
    hp: i64,
    hp_time: Time,
    /// When the health ran out, for a play that can fail.
    failed: Option<Time>,
}

impl Clone for Game {
    fn clone(&self) -> Game {
        Game {
            chart: self.chart.clone(),
            mods: self.mods,
            live_spun_out: self.live_spun_out,
            live_no_50s: self.live_no_50s,
            time: self.time,
            cursor: self.cursor,
            buttons: self.buttons,
            state: self.state.clone(),
            first_open: self.first_open,
            score: self.score,
            combo: self.combo,
            max_combo: self.max_combo,
            counts: self.counts,
            hp: self.hp,
            hp_time: self.hp_time,
            failed: self.failed,
        }
    }

    /// Restoring a snapshot into an existing game reuses its memory, so
    /// seeking does not allocate.
    fn clone_from(&mut self, src: &Game) {
        self.chart.clone_from(&src.chart);
        self.state.clone_from(&src.state);
        self.mods = src.mods;
        self.live_spun_out = src.live_spun_out;
        self.live_no_50s = src.live_no_50s;
        self.time = src.time;
        self.cursor = src.cursor;
        self.buttons = src.buttons;
        self.first_open = src.first_open;
        self.score = src.score;
        self.combo = src.combo;
        self.max_combo = src.max_combo;
        self.counts = src.counts;
        self.hp = src.hp;
        self.hp_time = src.hp_time;
        self.failed = src.failed;
    }
}

impl Game {
    pub fn new(chart: Arc<Chart>) -> Game {
        Game::with_mods(chart, 0)
    }

    /// `mods` are bits from [`crate::mods`]. Speed mods do not change the
    /// rules (everything is in map time); they change the score multiplier.
    pub fn with_mods(chart: Arc<Chart>, mods: u32) -> Game {
        let state = vec![ObjState::default(); chart.objects.len()];
        Game {
            chart,
            mods,
            live_spun_out: false,
            live_no_50s: false,
            time: Time::MIN,
            cursor: (0, 0),
            buttons: 0,
            state,
            first_open: 0,
            score: 0,
            combo: 0,
            max_combo: 0,
            counts: [0; 4],
            hp: crate::hp::FULL,
            hp_time: Time::MIN,
            failed: None,
        }
    }

    /// Opt in only for live play; old recordings keep their original rules.
    pub fn enable_live_spun_out(&mut self) {
        self.live_spun_out = true;
    }

    pub fn enable_live_no_50s(&mut self) { self.live_no_50s = true; }

    pub fn chart(&self) -> &Arc<Chart> {
        &self.chart
    }
    pub fn mods(&self) -> u32 {
        self.mods
    }
    pub fn time(&self) -> Time {
        self.time
    }
    pub fn cursor(&self) -> (Fx, Fx) {
        self.cursor
    }
    pub fn buttons(&self) -> u8 {
        self.buttons
    }
    pub fn score(&self) -> u64 {
        self.score
    }
    pub fn combo(&self) -> u32 {
        self.combo
    }

    /// Health as a fraction of a full bar, as of the last thing that happened.
    pub fn hp(&self) -> f32 {
        self.hp as f32 / crate::hp::FULL as f32
    }

    /// Health at map time `t` (not before the game's own time), counting the
    /// drain since the last change, for drawing a bar that moves smoothly.
    pub fn hp_at(&self, t: Time) -> f32 {
        self.hp_units_at(t) as f32 / crate::hp::FULL as f32
    }

    fn hp_units_at(&self, t: Time) -> i64 {
        if self.failed.is_some() {
            return 0;
        }
        let drained = (self.chart.drain_rate as i128 * self.chart.drain_time(self.hp_time, t.max(self.hp_time)) as i128)
            .min(i64::MAX as i128) as i64;
        (self.hp - drained).max(0)
    }

    /// When the health ran out. Never set with No Fail; the play layer ends
    /// a live play at this time.
    pub fn failed(&self) -> Option<Time> {
        self.failed
    }

    /// Let the drain run up to `t`, and fail or floor the bar if it empties.
    fn settle(&mut self, t: Time) {
        if t <= self.hp_time || self.failed.is_some() {
            return;
        }
        let chart = &*self.chart;
        let rate = chart.drain_rate as i128;
        let drained = |to: Time| (rate * chart.drain_time(self.hp_time, to) as i128).min(i64::MAX as i128) as i64;
        if drained(t) >= self.hp {
            if self.mods & crate::mods::NO_FAIL == 0 {
                // The first microsecond at which nothing is left.
                let (mut lo, mut hi) = (self.hp_time, t);
                while lo + 1 < hi {
                    let mid = lo + (hi - lo) / 2;
                    if drained(mid) >= self.hp {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                self.failed = Some(hi);
            }
            self.hp = 0;
        } else {
            self.hp -= drained(t);
        }
        self.hp_time = t;
    }

    /// Change health at time `t`; going empty fails the play.
    fn hp_add(&mut self, t: Time, delta: i64) {
        self.settle(t);
        if self.failed.is_some() {
            return;
        }
        self.hp = (self.hp + delta).clamp(0, crate::hp::FULL);
        if self.hp == 0 && self.mods & crate::mods::NO_FAIL == 0 {
            self.failed = Some(t);
        }
    }

    pub fn result(&self, object: usize) -> Option<HitResult> {
        match self.state[object].slot {
            MISS => Some(HitResult::Miss),
            HIT50 => Some(HitResult::Hit50),
            HIT100 => Some(HitResult::Hit100),
            HIT300 => Some(HitResult::Hit300),
            _ => None,
        }
    }

    /// Whether the object still awaits its final judgement.
    pub fn is_open(&self, object: usize) -> bool {
        self.state[object].slot == OPEN
    }

    /// Sliders: whether the head can still be clicked.
    pub fn head_pending(&self, object: usize) -> bool {
        self.state[object].head == HEAD_PENDING
    }

    /// Sliders: whether the player is following the ball right now.
    pub fn is_tracking(&self, object: usize) -> bool {
        self.state[object].tracking
    }

    /// Sliders: whether the player would be following the ball if it were at
    /// `ball` now, going by where the cursor and the keys are at this moment.
    /// Nothing changes; this is a forecast for sounds that are placed ahead
    /// of the judgement.
    pub fn would_track(&self, chart: &Chart, object: usize, ball: (Fx, Fx)) -> bool {
        let reach = if self.state[object].tracking { chart.follow_radius } else { chart.radius } as i64;
        let (dx, dy) = ((self.cursor.0 - ball.0) as i64, (self.cursor.1 - ball.1) as i64);
        self.held() && dx * dx + dy * dy <= reach * reach
    }

    /// Whether a key counts as down: with relax it always does.
    fn held(&self) -> bool {
        self.buttons != 0 || self.mods & crate::mods::RELAX != 0
    }

    /// Sliders: whether anything of it has been hit so far, or is being
    /// followed, that is, whether the slider would not end up a miss.
    pub fn slider_scored(&self, object: usize) -> bool {
        self.state[object].hits > 0 || self.state[object].tracking
    }

    /// Sliders: how many of its nested points are already resolved.
    pub fn nested_done(&self, object: usize) -> usize {
        self.state[object].next_nested as usize
    }

    /// Spinners: turns made so far.
    pub fn spinner_turns(&self, object: usize) -> f32 {
        self.state[object].rotation as f32 / TURN as f32
    }

    /// Lowest object index that may still be open; a cheap place to start
    /// when looking for what to draw.
    pub fn first_open(&self) -> usize {
        self.first_open
    }

    pub fn finished(&self) -> bool {
        self.first_open >= self.state.len()
    }

    pub fn summary(&self) -> Summary {
        Summary {
            score: self.score,
            max_combo: self.max_combo,
            n300: self.counts[3],
            n100: self.counts[2],
            n50: self.counts[1],
            miss: self.counts[0],
            state_hash: self.state_hash(),
        }
    }

    /// Let time pass up to `t`. The caller promises that every input with a
    /// timestamp below `t` has already been pushed. Going backwards is a no-op.
    pub fn advance_to(&mut self, t: Time, out: &mut impl FnMut(Event)) {
        self.advance_to_until(t, out, &|| false);
    }

    /// Stop immediately after the event that satisfies `stop`.
    pub fn advance_to_until(&mut self, t: Time, out: &mut impl FnMut(Event), stop: &impl Fn() -> bool) {
        if stop() || t <= self.time {
            return;
        }
        let chart = Arc::clone(&self.chart);
        loop {
            // The earliest time-driven point among objects that have
            // started. Objects are sorted by start time and nothing is due
            // before its object starts, so the scan can stop early.
            let mut next: Option<(Time, usize)> = None;
            for i in self.first_open..chart.objects.len() {
                let o = &chart.objects[i];
                if o.t >= t {
                    break;
                }
                if self.state[i].slot != OPEN {
                    continue;
                }
                let due = self.due(&chart, i);
                if due < t && next.is_none_or(|(best, _)| due < best) {
                    next = Some((due, i));
                }
            }
            let Some((due, i)) = next else { break };
            self.auto_spin_to(&chart, due, out);
            if self.live_spun_out {
                self.settle(due);
            }
            if self.live_spun_out && (self.failed.is_some() || stop()) {
                self.time = self.failed.unwrap_or(due);
                return;
            }
            self.resolve(&chart, i, due, out);
            self.skip_settled();
            if stop() {
                self.time = due;
                return;
            }
        }
        // Clicks settle objects too, without passing through `resolve`.
        self.skip_settled();
        self.auto_spin_to(&chart, t, out);
        if self.live_spun_out && (self.failed.is_some() || stop()) {
            self.time = self.failed.unwrap_or(t);
            return;
        }
        self.settle(t);
        self.time = if self.live_spun_out { self.failed.unwrap_or(t) } else { t };
    }

    fn skip_settled(&mut self) {
        while self.first_open < self.state.len() && self.state[self.first_open].slot != OPEN {
            self.first_open += 1;
        }
    }

    fn auto_spin_to(&mut self, chart: &Chart, t: Time, out: &mut impl FnMut(Event)) {
        if !self.live_spun_out { return; }
        // Process turn crossings from all open spinners in timestamp order.
        // Advancing health to a later crossing first could kill the player
        // before an earlier crossing has had a chance to restore health.
        loop {
            let mut next: Option<(Time, usize, u64)> = None;
            for i in self.first_open..chart.objects.len() {
                let o = &chart.objects[i];
                if o.t > t { break; }
                if o.kind != ObjectKind::Spinner || self.state[i].slot != OPEN { continue; }
                let turn = self.state[i].rotation / TURN + 1;
                let at = o.t.saturating_add(((turn as u128 * 60_000_000).div_ceil(MAX_SPIN_RPM)) as i64);
                if at <= t && at <= o.end && next.is_none_or(|(best, _, _)| at < best) {
                    next = Some((at, i, turn));
                }
            }
            let Some((at, i, turn)) = next else { break };
            let o = &chart.objects[i];
            let bonus = turn * 2 >= o.extra as u64 + 5;
            // McOsu awards two health increments per full turn; its normal
            // and bonus increments are 1.7 and 2.0 on a 200-point bar.
            self.hp_add(at, crate::hp::spinner_turn(bonus));
            if self.failed.is_some() { return; }
            self.state[i].rotation = turn * TURN;
            self.score += SCORE_SPIN + if bonus { SCORE_SPIN_BONUS } else { 0 };
            out(Event::Spin { object: i as u32, t: at, bonus });
        }
        for i in self.first_open..chart.objects.len() {
            let o = &chart.objects[i];
            if o.t > t { break; }
            if o.kind != ObjectKind::Spinner || self.state[i].slot != OPEN { continue; }
            let elapsed = t.min(o.end).saturating_sub(o.t).max(0) as u128;
            self.state[i].rotation = (elapsed * MAX_SPIN_RPM * TURN as u128 / 60_000_000).min(u64::MAX as u128) as u64;
        }
    }

    /// When the object's next time-driven point is due.
    fn due(&self, chart: &Chart, i: usize) -> Time {
        let o = &chart.objects[i];
        // Relax clicks by itself the moment the object's time comes.
        if self.mods & crate::mods::RELAX != 0 && !self.state[i].tried {
            match o.kind {
                ObjectKind::Circle => return o.t,
                ObjectKind::Slider if self.state[i].head == HEAD_PENDING => return o.t,
                _ => {}
            }
        }
        match o.kind {
            ObjectKind::Circle => o.t + if self.live_no_50s { chart.window_100 } else { chart.window_50 },
            ObjectKind::Spinner => o.end,
            ObjectKind::Slider => {
                let (st, s) = (&self.state[i], chart.slider(o));
                let mut due = o.end;
                if st.head == HEAD_PENDING {
                    due = due.min(o.t + if self.live_no_50s { chart.window_100 } else { chart.window_50 });
                }
                if st.next_nested < s.nested_len {
                    due = due.min(chart.nested[(s.nested_start + st.next_nested) as usize].t);
                }
                due
            }
        }
    }

    /// Resolve the point of object `i` that is due at `due`. Each call
    /// makes progress, so the loop in `advance_to` terminates.
    fn resolve(&mut self, chart: &Chart, i: usize, due: Time, out: &mut impl FnMut(Event)) {
        let o = &chart.objects[i];
        if self.mods & crate::mods::RELAX != 0 && !self.state[i].tried && due == o.t {
            let click_kind = match o.kind {
                ObjectKind::Circle => true,
                ObjectKind::Slider => self.state[i].head == HEAD_PENDING,
                ObjectKind::Spinner => false,
            };
            if click_kind {
                self.state[i].tried = true;
                self.click(chart, due, true, out);
                return;
            }
        }
        match o.kind {
            ObjectKind::Circle => self.judge(chart, i, HitResult::Miss, due, 0, true, out),
            ObjectKind::Spinner => {
                let (rotation, need) = (self.state[i].rotation as u128, o.extra as u128 * (TURN as u128 / 2));
                let result = if rotation >= need {
                    HitResult::Hit300
                } else if rotation * 10 > need * 9 {
                    HitResult::Hit100
                } else if rotation * 4 > need * 3 {
                    HitResult::Hit50
                } else {
                    HitResult::Miss
                };
                self.judge(chart, i, result, due, 0, true, out);
            }
            ObjectKind::Slider => {
                let s = *chart.slider(o);
                let st = self.state[i];
                if st.head == HEAD_PENDING && (o.t + if self.live_no_50s { chart.window_100 } else { chart.window_50 }).min(o.end) == due {
                    // Nobody clicked the head in time.
                    self.state[i].head = HEAD_MISSED;
                    self.combo = 0;
                    self.hp_add(due, crate::hp::part_miss(chart.difficulty.hp));
                    out(Event::SliderHead { object: i as u32, hit: false, t: due, error: 0 });
                } else if st.next_nested < s.nested_len
                    && chart.nested[(s.nested_start + st.next_nested) as usize].t == due
                {
                    let n = chart.nested[(s.nested_start + st.next_nested) as usize];
                    self.track(chart, i, (n.x, n.y));
                    let hit = self.state[i].tracking;
                    self.state[i].next_nested += 1;
                    if hit {
                        self.state[i].hits += 1;
                        self.score += match n.kind {
                            NestedKind::Tick => SCORE_SLIDER_TICK,
                            NestedKind::Repeat => SCORE_SLIDER_REPEAT,
                            NestedKind::Tail => SCORE_SLIDER_TAIL,
                        };
                        self.add_combo();
                        self.hp_add(due, crate::hp::part_hit(n.kind.into()));
                    } else {
                        self.hp_add(due, crate::hp::part_miss(chart.difficulty.hp));
                        if n.kind != NestedKind::Tail {
                            // Letting go early costs the 300 but not the combo.
                            self.combo = 0;
                        }
                    }
                    out(Event::SliderPoint { object: i as u32, kind: n.kind, hit, t: due });
                } else {
                    // The slider is over: grade it by how much of it was hit.
                    let (hits, total) = (self.state[i].hits, 1 + s.nested_len);
                    let result = if hits == total {
                        HitResult::Hit300
                    } else if hits * 2 >= total {
                        HitResult::Hit100
                    } else if hits > 0 {
                        HitResult::Hit50
                    } else {
                        HitResult::Miss
                    };
                    self.state[i].tracking = false;
                    self.judge(chart, i, result, due, 0, false, out);
                }
            }
        }
    }

    /// Apply one input. Timestamps must not go backwards; one that does is
    /// moved up to the current time. Returns the input as actually applied,
    /// which is what a replay has to store.
    pub fn push(&mut self, input: Input, out: &mut impl FnMut(Event)) -> Input {
        self.push_until(input, out, &|| false).expect("uninterrupted input")
    }

    /// Returns None if a prior timed event stopped the input from being applied.
    pub fn push_until(&mut self, input: Input, out: &mut impl FnMut(Event), stop: &impl Fn() -> bool) -> Option<Input> {
        let t = input.t.max(self.time);
        self.advance_to_until(t, out, stop);
        if stop() || (self.live_spun_out && self.failed.is_some()) { return None; }
        let chart = Arc::clone(&self.chart);
        match input.kind {
            InputKind::Move { x, y } => {
                self.cursor = (x, y);
                if self.mods & crate::mods::RELAX != 0 {
                    // Arriving late on an object whose time has come.
                    self.click(&chart, t, true, out);
                    self.skip_settled();
                }
            }
            InputKind::Release(b) => self.buttons &= !(b as u8),
            InputKind::Press(b) => {
                let fresh = self.buttons & b.group() == 0;
                self.buttons |= b as u8;
                if fresh {
                    self.click(&chart, t, false, out);
                    self.skip_settled();
                }
            }
        }
        if stop() { return Some(Input { t, kind: input.kind }); }
        // The cursor or a button changed: re-evaluate everything that is
        // held rather than clicked.
        for i in self.first_open..chart.objects.len() {
            let o = &chart.objects[i];
            if o.t > t {
                break;
            }
            if self.state[i].slot != OPEN || t > o.end {
                continue;
            }
            match o.kind {
                ObjectKind::Slider => self.track(&chart, i, chart.ball_at(o, t)),
                ObjectKind::Spinner if !self.live_spun_out => self.spin(i, o, t, out),
                ObjectKind::Spinner => {},
                ObjectKind::Circle => {}
            }
        }
        Some(Input { t, kind: input.kind })
    }

    /// Following a slider: it starts with a button down and the cursor on
    /// the ball, and then holds within the larger follow circle.
    fn track(&mut self, chart: &Chart, i: usize, ball: (Fx, Fx)) {
        let held = self.held();
        let st = &mut self.state[i];
        let reach = if st.tracking { chart.follow_radius } else { chart.radius } as i64;
        let (dx, dy) = ((self.cursor.0 - ball.0) as i64, (self.cursor.1 - ball.1) as i64);
        st.tracking = held && dx * dx + dy * dy <= reach * reach;
    }

    fn spin(&mut self, i: usize, o: &HitObject, t: Time, out: &mut impl FnMut(Event)) {
        let down = self.held();
        let now = angle((self.cursor.0 - CENTRE.0) as i64, (self.cursor.1 - CENTRE.1) as i64);
        let st = &mut self.state[i];
        if down && st.has_angle {
            let turned = (now.wrapping_sub(st.last_angle) as i32).unsigned_abs() as u64;
            let dt = (t - st.last_t).max(0) as u128;
            let limit = (dt * MAX_SPIN_RPM * TURN as u128 / 60_000_000).min(u64::MAX as u128) as u64;
            let before = st.rotation / TURN;
            st.rotation += turned.min(limit);
            let after = st.rotation / TURN;
            for turn in before + 1..=after {
                // Bonus turns start a turn and a half past the requirement.
                let bonus = turn * 2 >= o.extra as u64 + 5;
                self.score += SCORE_SPIN + if bonus { SCORE_SPIN_BONUS } else { 0 };
                out(Event::Spin { object: i as u32, t, bonus });
            }
        }
        let st = &mut self.state[i];
        st.has_angle = down;
        st.last_angle = now;
        st.last_t = t;
    }

    fn click(&mut self, chart: &Chart, t: Time, relax: bool, out: &mut impl FnMut(Event)) {
        let objects = &chart.objects;
        let r2 = chart.radius as i64 * chart.radius as i64;
        // The click goes to the earliest clickable object under the cursor
        // that is close enough in time, and to nothing else.
        let mut target = None;
        for i in self.first_open..objects.len() {
            let o = &objects[i];
            if o.t - HITTABLE_RANGE > t || (relax && o.t > t) {
                break;
            }
            let st = &self.state[i];
            let clickable = match o.kind {
                ObjectKind::Circle => st.slot == OPEN,
                // A slider is clicked once, on its head.
                ObjectKind::Slider => st.slot == OPEN && st.head == HEAD_PENDING,
                ObjectKind::Spinner => false,
            };
            if !clickable {
                continue;
            }
            let (dx, dy) = ((self.cursor.0 - o.x) as i64, (self.cursor.1 - o.y) as i64);
            if dx * dx + dy * dy <= r2 {
                target = Some(i);
                break;
            }
        }
        let Some(i) = target else { return };
        let o = &objects[i];

        // Note lock: an earlier open object blocks this one unless the two
        // are (almost) simultaneous.
        let blocked =
            (self.first_open..i).any(|j| self.state[j].slot == OPEN && objects[j].end + NOTELOCK_LENIENCY < o.t);
        if blocked {
            out(Event::Blocked { object: i as u32, t });
            return;
        }

        let error = t - o.t;
        let off = error.abs();
        if o.kind == ObjectKind::Slider {
            // A slider head has no accuracy of its own: in the window or not.
            let hit = off <= if self.live_no_50s { chart.window_100 } else { chart.window_50 };
            if hit {
                self.state[i].head = HEAD_HIT;
                self.state[i].hits += 1;
                self.score += SCORE_SLIDER_HEAD;
                self.add_combo();
                self.hp_add(t, crate::hp::part_hit(crate::hp::Part::Head));
            } else {
                self.state[i].head = HEAD_MISSED;
                self.combo = 0;
                self.hp_add(t, crate::hp::part_miss(chart.difficulty.hp));
            }
            out(Event::SliderHead { object: i as u32, hit, t, error });
            return;
        }
        let result = if off <= chart.window_300 {
            HitResult::Hit300
        } else if off <= chart.window_100 {
            HitResult::Hit100
        } else if off <= chart.window_50 {
            HitResult::Hit50
        } else {
            // Only reachable early: late objects have already expired.
            HitResult::Miss
        };
        self.judge(chart, i, result, t, error, true, out);
    }

    fn add_combo(&mut self) {
        self.combo += 1;
        self.max_combo = self.max_combo.max(self.combo);
    }

    /// Final judgement of an object. `counts_combo`: circles and spinners
    /// are one combo each; a slider's combo came from its parts.
    #[allow(clippy::too_many_arguments)]
    fn judge(
        &mut self,
        chart: &Chart,
        i: usize,
        result: HitResult,
        t: Time,
        error: Time,
        counts_combo: bool,
        out: &mut impl FnMut(Event),
    ) {
        let result = if self.live_no_50s && result == HitResult::Hit50 { HitResult::Miss } else { result };
        let slot = match result {
            HitResult::Miss => MISS,
            HitResult::Hit50 => HIT50,
            HitResult::Hit100 => HIT100,
            HitResult::Hit300 => HIT300,
        };
        self.state[i].slot = slot;
        self.counts[(slot - MISS) as usize] += 1;
        self.hp_add(t, crate::hp::result_delta(chart.difficulty.hp, result));
        if result == HitResult::Miss {
            if counts_combo {
                self.combo = 0;
            }
        } else {
            // Score V1: value + value/25 * (combo before the hit - 1) * multiplier,
            // where the multiplier is difficulty times mods, truncated.
            let value = result.value() as u64;
            let combo_factor = self.combo.saturating_sub(1) as u64;
            let mod_multiplier = crate::mods::score_ppm(self.mods);
            let mod_multiplier = if self.live_spun_out { mod_multiplier * 90 / 100 } else { mod_multiplier };
            let multiplier = chart.score_multiplier as u64 * mod_multiplier;
            self.score += value + combo_factor * (value / 25) * multiplier / 1_000_000;
            if counts_combo {
                self.add_combo();
            }
        }
        out(Event::Judged { object: i as u32, result, t, error });
    }

    /// FNV-1a over everything that defines the state. Two games with equal
    /// hashes went through the same judgements in the same order.
    pub fn state_hash(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        eat(&self.mods.to_le_bytes());
        eat(&self.time.to_le_bytes());
        eat(&self.cursor.0.to_le_bytes());
        eat(&self.cursor.1.to_le_bytes());
        eat(&[self.buttons]);
        eat(&self.score.to_le_bytes());
        eat(&self.combo.to_le_bytes());
        eat(&self.max_combo.to_le_bytes());
        // The bar as of now, not as of the last change: slicing time
        // differently moves where the drain was last settled.
        eat(&self.hp_units_at(self.time).to_le_bytes());
        eat(&self.failed.unwrap_or(Time::MIN).to_le_bytes());
        for c in self.counts {
            eat(&c.to_le_bytes());
        }
        for s in &self.state {
            eat(&[s.slot, s.head, s.tracking as u8, s.has_angle as u8]);
            eat(&s.next_nested.to_le_bytes());
            eat(&s.hits.to_le_bytes());
            eat(&s.rotation.to_le_bytes());
            eat(&s.last_angle.to_le_bytes());
            eat(&s.last_t.to_le_bytes());
        }
        h
    }

    /// Run a whole input stream and return the outcome. `end` is the time
    /// the original session was advanced to after its last input.
    pub fn simulate(chart: Arc<Chart>, mods: u32, inputs: &[Input], end: Time) -> Summary {
        let mut game = Game::with_mods(chart, mods);
        let mut sink = |_| {};
        for &input in inputs {
            game.push(input, &mut sink);
        }
        game.advance_to(end, &mut sink);
        game.summary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{ChartBuilder, Difficulty};
    use crate::{to_fx, FX_ONE};

    /// OD 8: windows 31.5 / 75.5 / 119.5 ms. CS 4: radius ~36.5 px,
    /// follow radius ~87.6 px.
    fn builder() -> ChartBuilder {
        ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 })
    }

    fn circles(list: &[(i64, f32, f32)]) -> Arc<Chart> {
        let mut b = builder();
        for &(ms, x, y) in list {
            b.circle(ms * MS, to_fx(x), to_fx(y));
        }
        Arc::new(b.build())
    }

    fn px(x: f32, y: f32) -> (Fx, Fx) {
        (to_fx(x), to_fx(y))
    }

    /// A straight 200 px slider from (100,100) to (300,100) starting at
    /// 1 s, one second per pass, with a tick in the middle of each pass
    /// and the tail check 36 ms before the end.
    fn slider_chart(spans: u32) -> Arc<Chart> {
        let mut b = builder();
        let mut nested = Vec::new();
        for s in 0..spans as Time {
            nested.push((1_500_000 + s * 1_000_000, NestedKind::Tick));
            if s + 1 < spans as Time {
                nested.push((2_000_000 + s * 1_000_000, NestedKind::Repeat));
            }
        }
        nested.push((1_000_000 + spans as Time * 1_000_000 - 36_000, NestedKind::Tail));
        b.slider(1_000_000, &[px(100.0, 100.0), px(300.0, 100.0)], spans, 1_000_000, &nested);
        Arc::new(b.build())
    }

    struct Run {
        game: Game,
        events: Vec<Event>,
    }

    impl Run {
        fn new(chart: Arc<Chart>) -> Run {
            Run { game: Game::new(chart), events: Vec::new() }
        }
        fn at(&mut self, us: Time, kind: InputKind) -> &mut Self {
            let events = &mut self.events;
            self.game.push(Input { t: us, kind }, &mut |e| events.push(e));
            self
        }
        fn mv(&mut self, us: Time, x: f32, y: f32) -> &mut Self {
            self.at(us, InputKind::Move { x: to_fx(x), y: to_fx(y) })
        }
        fn press(&mut self, us: Time, b: Button) -> &mut Self {
            self.at(us, InputKind::Press(b))
        }
        fn release(&mut self, us: Time, b: Button) -> &mut Self {
            self.at(us, InputKind::Release(b))
        }
        fn tap(&mut self, us: Time, b: Button) -> &mut Self {
            self.press(us, b).release(us + 20 * MS, b)
        }
        fn until(&mut self, us: Time) -> &mut Self {
            let events = &mut self.events;
            self.game.advance_to(us, &mut |e| events.push(e));
            self
        }
        fn results(&self) -> Vec<(u32, HitResult, Time)> {
            self.events
                .iter()
                .filter_map(|e| match *e {
                    Event::Judged { object, result, error, .. } => Some((object, result, error)),
                    _ => None,
                })
                .collect()
        }
        /// Hit/miss of each slider part, in order.
        fn points(&self) -> Vec<(NestedKind, bool)> {
            self.events
                .iter()
                .filter_map(|e| match *e {
                    Event::SliderPoint { kind, hit, .. } => Some((kind, hit)),
                    _ => None,
                })
                .collect()
        }
    }

    #[test]
    fn no_50s_circle_and_slider_head_boundaries() {
        for offset in [-119_500, -75_501, -75_500, 75_500, 75_501, 119_500] {
            let mut r = Run::new(circles(&[(1000, 100.0, 100.0)]));
            r.game.enable_live_no_50s();
            r.mv(0, 100.0, 100.0).press(1_000_000 + offset, Button::K1).until(1_300_000);
            assert_eq!(r.results()[0].1, if offset.abs() <= 75_500 { HitResult::Hit100 } else { HitResult::Miss }, "offset {offset}");
            let mut r = Run::new(slider_chart(1));
            r.game.enable_live_no_50s();
            r.mv(0, 100.0, 100.0).press(1_000_000 + offset, Button::K1).until(1_300_000);
            let head = r.events.iter().find_map(|e| if let Event::SliderHead { hit, .. } = e { Some(*hit) } else { None }).unwrap();
            assert_eq!(head, offset.abs() <= 75_500, "offset {offset}");
        }
    }

    #[test]
    fn no_50s_transforms_final_slider_and_spinner_not_parts() {
        let mut r = Run::new(slider_chart(1));
        r.game.enable_live_no_50s();
        r.mv(900_000, 100.0, 100.0).press(1_000_000, Button::K1).release(1_100_000, Button::K1).until(3_000_000);
        assert_eq!(r.points(), vec![(NestedKind::Tick, false), (NestedKind::Tail, false)]);
        assert_eq!(r.results(), vec![(0, HitResult::Miss, 0)]);
        let mut r = Run::new(spinner_chart());
        r.game.enable_live_no_50s();
        r.mv(900_000, 336.0, 192.0).press(1_000_000, Button::K1);
        whirl(&mut r, 1_000_000, 3_000_000, 150.0);
        r.until(3_100_000);
        assert_eq!(r.results(), vec![(0, HitResult::Miss, 0)]);
        assert_eq!(r.game.summary().n50, 0);
    }

    #[test]
    fn window_edges_are_inclusive_to_the_microsecond() {
        for (offset, expect) in [
            (0, HitResult::Hit300),
            (31_500, HitResult::Hit300),
            (-31_500, HitResult::Hit300),
            (31_501, HitResult::Hit100),
            (75_500, HitResult::Hit100),
            (75_501, HitResult::Hit50),
            (-119_500, HitResult::Hit50),
            (119_500, HitResult::Hit50),
            (-119_501, HitResult::Miss),
            (-400_000, HitResult::Miss),
        ] {
            let mut r = Run::new(circles(&[(1000, 100.0, 100.0)]));
            r.mv(0, 100.0, 100.0).tap(1_000_000 + offset, Button::K1);
            assert_eq!(r.results(), vec![(0, expect, offset)], "offset {offset}");
        }
    }

    #[test]
    fn clicks_outside_time_or_space_do_nothing() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0)]));
        // More than 400 ms early: the object is not clickable yet.
        r.mv(0, 100.0, 100.0).tap(599_999, Button::K1);
        // In time, but the cursor is outside the circle.
        r.mv(900_000, 100.0 + 37.0, 100.0).tap(1_000_000, Button::K2);
        assert!(r.events.is_empty());
        // Just inside the radius does count.
        r.mv(1_050_000, 100.0 + 36.0, 100.0).tap(1_060_000, Button::K1);
        assert_eq!(r.results(), vec![(0, HitResult::Hit100, 60_000)]);
    }

    #[test]
    fn an_unhit_object_misses_exactly_when_its_window_closes() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0)]));
        r.until(1_119_500);
        assert!(r.events.is_empty(), "still hittable at the edge");
        r.until(1_119_501);
        assert_eq!(r.events, vec![Event::Judged { object: 0, result: HitResult::Miss, t: 1_119_500, error: 0 }]);
        assert!(r.game.finished());
    }

    #[test]
    fn a_late_click_cannot_hit_an_expired_object() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0)]));
        r.mv(0, 100.0, 100.0).tap(1_119_501, Button::K1);
        assert_eq!(r.results(), vec![(0, HitResult::Miss, 0)]);
    }

    #[test]
    fn note_lock_blocks_the_later_object_until_the_earlier_is_judged() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0), (1200, 300.0, 100.0)]));
        // Clicking the second while the first is open: blocked, no judgement.
        r.mv(0, 300.0, 100.0).press(1_100_000, Button::K1);
        assert_eq!(r.events, vec![Event::Blocked { object: 1, t: 1_100_000 }]);
        r.release(1_110_000, Button::K1);
        // Once the first has expired the second can be hit.
        r.tap(1_200_000, Button::K2);
        assert_eq!(r.results(), vec![(0, HitResult::Miss, 0), (1, HitResult::Hit300, 0)]);

        // Objects within 3 ms of each other do not lock.
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0), (1003, 300.0, 100.0)]));
        r.mv(0, 300.0, 100.0).tap(1_000_000, Button::K1);
        assert_eq!(r.results(), vec![(1, HitResult::Hit300, -3_000)]);
    }

    #[test]
    fn a_click_only_ever_goes_to_the_earliest_object_under_the_cursor() {
        // Two stacked objects: one click must not hit both.
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0), (1100, 100.0, 100.0)]));
        r.mv(0, 100.0, 100.0).tap(1_050_000, Button::K1);
        assert_eq!(r.results(), vec![(0, HitResult::Hit100, 50_000)]);
    }

    #[test]
    fn twin_buttons_are_one_logical_button() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0), (1010, 100.0, 100.0)]));
        r.mv(0, 100.0, 100.0);
        // K1 down, then M1 down while K1 is held: one click, not two.
        r.press(1_000_000, Button::K1).press(1_005_000, Button::M1);
        assert_eq!(r.results().len(), 1);
        // K2 is a different button and does click.
        r.press(1_010_000, Button::K2);
        assert_eq!(r.results().len(), 2);
    }

    #[test]
    fn score_v1_combo_formula() {
        let c = circles(&[
            (1000, 100.0, 100.0),
            (2000, 100.0, 100.0),
            (3000, 100.0, 100.0),
            (4000, 100.0, 100.0),
            (5000, 100.0, 100.0),
        ]);
        let m = c.score_multiplier as u64;
        let mut r = Run::new(c);
        r.mv(0, 100.0, 100.0);
        for i in 0..4 {
            r.tap(1_000_000 + i * 1_000_000, Button::K1);
        }
        // Combo before each hit: 0, 1, 2, 3 -> factor 0, 0, 1, 2.
        assert_eq!(r.game.score(), 4 * 300 + (1 + 2) * 12 * m);
        // A 100 at combo 4: 100 + 3 * (100/25) * m.
        r.tap(5_000_000 + 50_000, Button::K1);
        assert_eq!(r.game.score(), 4 * 300 + 3 * 12 * m + 100 + 3 * 4 * m);
        let s = r.game.summary();
        assert_eq!((s.n300, s.n100, s.n50, s.miss, s.max_combo), (4, 1, 0, 0, 5));
        assert!((s.accuracy() - (4.0 * 300.0 + 100.0) / 1500.0).abs() < 1e-12);
    }

    #[test]
    fn relax_hits_on_time_without_keys() {
        use crate::chart::{ChartBuilder, Difficulty};
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.circle(1_000_000, to_fx(100.0), to_fx(100.0));
        b.circle(1_500_000, to_fx(300.0), to_fx(200.0));
        b.circle(2_000_000, to_fx(400.0), to_fx(300.0));
        b.spinner(2_500_000, 4_000_000);
        let chart = Arc::new(b.build());
        let mv = |t, x: f32, y: f32| Input { t, kind: InputKind::Move { x: to_fx(x), y: to_fx(y) } };
        // Third circle is never visited; the others are aimed at early/late.
        let inputs = [mv(900_000, 100.0, 100.0), mv(1_510_000, 300.0, 200.0), mv(2_400_000, 0.0, 0.0)];
        let mut g = Game::with_mods(chart.clone(), crate::mods::RELAX);
        let mut errors = vec![];
        for i in inputs {
            g.push(i, &mut |e| if let Event::Judged { error, .. } = e { errors.push(error) });
        }
        g.advance_to(5_000_000, &mut |e| if let Event::Judged { error, .. } = e { errors.push(error) });
        let s = g.summary();
        assert_eq!((s.n300, s.miss), (2, 2), "two aimed circles hit, the third and the spinner missed");
        assert_eq!(errors[..2], [0, 10_000], "on time, and when the cursor got there");
        // Without relax nothing is hit.
        let s = Game::simulate(chart, 0, &inputs, 5_000_000);
        assert_eq!(s.miss, 4);
    }

    #[test]
    fn speed_mods_scale_the_combo_part_of_the_score() {
        let c = circles(&[(1000, 100.0, 100.0), (2000, 100.0, 100.0), (3000, 100.0, 100.0), (4000, 100.0, 100.0)]);
        let m = c.score_multiplier as u64;
        let play = |mods: u32| {
            let mut g = Game::with_mods(c.clone(), mods);
            let mut sink = |_| {};
            g.push(Input { t: 0, kind: InputKind::Move { x: to_fx(100.0), y: to_fx(100.0) } }, &mut sink);
            for i in 0..4 {
                let t = 1_000_000 + i * 1_000_000;
                g.push(Input { t, kind: InputKind::Press(Button::K1) }, &mut sink);
                g.push(Input { t: t + 10_000, kind: InputKind::Release(Button::K1) }, &mut sink);
            }
            g.summary()
        };
        // Combo factors 0, 0, 1, 2; each bonus is 12 * multiplier * percent / 100, truncated.
        let bonus = |percent: u64| 12 * m * percent / 100 + 2 * 12 * m * percent / 100;
        assert_eq!(play(0).score, 1200 + bonus(100));
        assert_eq!(play(crate::mods::DOUBLE_TIME).score, 1200 + bonus(112));
        assert_eq!(play(crate::mods::HALF_TIME).score, 1200 + bonus(30));
        assert_ne!(play(0).state_hash, play(crate::mods::DOUBLE_TIME).state_hash);
        assert_eq!(crate::mods::parse("nc").map(crate::mods::speed), Ok(1.5));
        assert!(crate::mods::parse("dt,ht").is_err() && crate::mods::parse("fl").is_err());
    }

    #[test]
    fn a_miss_resets_combo() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0), (2000, 100.0, 100.0), (3000, 100.0, 100.0)]));
        r.mv(0, 100.0, 100.0).tap(1_000_000, Button::K1).tap(3_000_000, Button::K1);
        assert_eq!(r.game.combo(), 1);
        assert_eq!(r.game.summary().max_combo, 1);
        assert_eq!(r.game.summary().miss, 1);
    }

    /// A map of `n` circles, one every `gap` ms from 1 s.
    fn stream(n: i64, gap: i64) -> Arc<Chart> {
        circles(&(0..n).map(|i| (1000 + i * gap, 100.0, 100.0)).collect::<Vec<_>>())
    }

    #[test]
    fn perfect_play_keeps_the_bar_above_the_target_and_missing_everything_fails() {
        let chart = stream(200, 300);
        assert!(chart.drain_rate > 0);
        // Every circle on time: never fails, and the bar stays well above empty.
        let mut r = Run::new(chart.clone());
        r.mv(0, 100.0, 100.0);
        let mut lowest = 1.0f32;
        for i in 0..200 {
            r.tap((1000 + i * 300) * MS, Button::K1);
            lowest = lowest.min(r.game.hp_at((1000 + i * 300) * MS - 1));
        }
        assert!(r.game.failed().is_none());
        assert!(lowest > 0.75 && lowest < 1.0, "lowest {lowest}");
        // Nothing hit: the bar empties and the time is recorded; with No Fail
        // the bar is empty but the play goes on.
        let mut miss = Run::new(chart.clone());
        miss.until(70_000_000);
        let at = miss.game.failed().expect("failed");
        assert!(at > 1_000 * MS && at < 20_000 * MS, "failed at {at}");
        assert_eq!(miss.game.hp(), 0.0);
        let mut nf = Run { game: Game::with_mods(chart, crate::mods::NO_FAIL), events: Vec::new() };
        nf.until(70_000_000);
        assert!(nf.game.failed().is_none());
        assert_eq!(nf.game.hp(), 0.0);
    }

    #[test]
    fn a_break_does_not_drain() {
        let mut b = builder();
        b.breaks(&[(2_000, 30_000)]);
        for ms in [1000, 1500, 31_000, 31_500] {
            b.circle(ms * MS, to_fx(100.0), to_fx(100.0));
        }
        let chart = b.build();
        assert_eq!(chart.drain_time(1_500 * MS, 31_000 * MS), 1_500 * MS);
        assert_eq!(chart.drain_time(0, 100_000 * MS), 2_500 * MS);
    }

    #[test]
    fn out_of_order_timestamps_are_clamped_and_reported() {
        let mut r = Run::new(circles(&[(1000, 100.0, 100.0)]));
        r.mv(1_000_000, 100.0, 100.0);
        let applied = r.game.push(Input { t: 999_000, kind: InputKind::Press(Button::K1) }, &mut |_| {});
        assert_eq!(applied.t, 1_000_000);
    }

    // ---- sliders -----------------------------------------------------

    /// Hold from the head and ride the ball along a one-pass slider.
    fn ride(r: &mut Run, from: Time, to: Time) {
        let mut t = from;
        while t <= to {
            let x = 100.0 + 200.0 * (t - 1_000_000) as f32 / 1_000_000.0;
            r.mv(t, x.clamp(100.0, 300.0), 100.0);
            t += 10_000;
        }
    }

    #[test]
    fn a_fully_followed_slider_is_a_300_and_counts_every_part_as_combo() {
        let c = slider_chart(1);
        let m = c.score_multiplier as u64;
        let mut r = Run::new(c);
        r.mv(900_000, 100.0, 100.0).press(1_000_000, Button::K1);
        ride(&mut r, 1_000_000, 2_000_000);
        r.release(2_000_001, Button::K1).until(3_000_000);
        assert_eq!(r.points(), vec![(NestedKind::Tick, true), (NestedKind::Tail, true)]);
        assert_eq!(r.results(), vec![(0, HitResult::Hit300, 0)]);
        // Head, tick, tail: three combo. The 300 itself adds none.
        assert_eq!(r.game.summary().max_combo, 3);
        // 30 + 10 + 30 raw, then 300 scored at combo 3: factor 2.
        assert_eq!(r.game.score(), 30 + 10 + 30 + 300 + 2 * 12 * m);
        assert!(r.game.finished());
    }

    #[test]
    fn the_head_only_needs_the_50_window_and_has_no_accuracy() {
        let mut r = Run::new(slider_chart(1));
        r.mv(900_000, 100.0, 100.0).press(1_000_000 + 119_500, Button::K1);
        assert_eq!(r.events, vec![Event::SliderHead { object: 0, hit: true, t: 1_119_500, error: 119_500 }]);
    }

    #[test]
    fn letting_go_early_breaks_combo_and_lowers_the_grade() {
        let mut r = Run::new(slider_chart(1));
        r.mv(900_000, 100.0, 100.0).press(1_000_000, Button::K1).release(1_100_000, Button::K1);
        r.until(1_500_001);
        assert_eq!(r.game.combo(), 0, "the missed tick is a slider break");
        r.until(3_000_000);
        assert_eq!(r.points(), vec![(NestedKind::Tick, false), (NestedKind::Tail, false)]);
        // One part of three: more than nothing, less than half.
        assert_eq!(r.results(), vec![(0, HitResult::Hit50, 0)]);
    }

    #[test]
    fn missing_only_the_tail_costs_the_300_but_not_the_combo() {
        let mut r = Run::new(slider_chart(1));
        r.mv(900_000, 100.0, 100.0).press(1_000_000, Button::K1);
        ride(&mut r, 1_000_000, 1_600_000);
        r.release(1_700_000, Button::K1).until(3_000_000);
        assert_eq!(r.points(), vec![(NestedKind::Tick, true), (NestedKind::Tail, false)]);
        assert_eq!(r.results(), vec![(0, HitResult::Hit100, 0)]);
        assert_eq!(r.game.combo(), 2, "head and tick; the lost tail does not reset it");
    }

    #[test]
    fn a_slider_can_be_followed_without_clicking_its_head() {
        let mut r = Run::new(slider_chart(1));
        // Key already down before the slider is clickable, cursor waiting on the head.
        r.mv(100_000, 100.0, 100.0).press(200_000, Button::K1);
        ride(&mut r, 1_000_000, 2_000_000);
        r.until(3_000_000);
        let head = r.events.iter().find(|e| matches!(e, Event::SliderHead { .. })).copied();
        assert_eq!(head, Some(Event::SliderHead { object: 0, hit: false, t: 1_119_500, error: 0 }));
        assert_eq!(r.points(), vec![(NestedKind::Tick, true), (NestedKind::Tail, true)]);
        assert_eq!(r.results(), vec![(0, HitResult::Hit100, 0)], "two of three parts");
    }

    #[test]
    fn an_early_click_loses_the_head_but_the_slider_plays_on() {
        let mut r = Run::new(slider_chart(1));
        r.mv(500_000, 100.0, 100.0).press(700_000, Button::K1); // 300 ms early
        assert_eq!(r.events, vec![Event::SliderHead { object: 0, hit: false, t: 700_000, error: -300_000 }]);
        ride(&mut r, 1_000_000, 2_000_000);
        r.until(3_000_000);
        assert_eq!(r.results(), vec![(0, HitResult::Hit100, 0)]);
    }

    #[test]
    fn nothing_hit_is_a_miss() {
        let mut r = Run::new(slider_chart(1));
        r.until(3_000_000);
        assert_eq!(r.results(), vec![(0, HitResult::Miss, 0)]);
        assert_eq!(r.game.summary().miss, 1);
    }

    #[test]
    fn following_starts_on_the_ball_and_holds_in_the_follow_circle() {
        let c = slider_chart(1);
        // Ball at 1.25 s is at x = 150.
        let mut r = Run::new(c.clone());
        r.mv(900_000, 150.0 + 60.0, 100.0).press(1_250_000, Button::K1);
        assert!(!r.game.is_tracking(0), "60 px away: outside the circle, cannot start");
        r.mv(1_250_000, 150.0 + 30.0, 100.0);
        assert!(r.game.is_tracking(0), "on the ball: following starts");
        r.mv(1_250_000, 150.0 + 80.0, 100.0);
        assert!(r.game.is_tracking(0), "80 px is inside the 87 px follow circle");
        r.mv(1_250_000, 150.0 + 90.0, 100.0);
        assert!(!r.game.is_tracking(0), "90 px is outside it");
        r.mv(1_250_000, 150.0 + 80.0, 100.0);
        assert!(!r.game.is_tracking(0), "and once lost, it has to be caught on the ball again");
    }

    #[test]
    fn repeats_are_judged_at_the_far_end_and_the_ball_comes_back() {
        let mut r = Run::new(slider_chart(2));
        r.mv(900_000, 100.0, 100.0).press(1_000_000, Button::K1);
        // Ride out and back.
        let mut t = 1_000_000;
        while t <= 3_000_000 {
            let phase = (t - 1_000_000) as f32 / 1_000_000.0;
            let along = if phase <= 1.0 { phase } else { 2.0 - phase };
            r.mv(t, 100.0 + 200.0 * along, 100.0);
            t += 10_000;
        }
        r.until(4_000_000);
        assert_eq!(
            r.points(),
            vec![
                (NestedKind::Tick, true),
                (NestedKind::Repeat, true),
                (NestedKind::Tick, true),
                (NestedKind::Tail, true)
            ]
        );
        assert_eq!(r.results(), vec![(0, HitResult::Hit300, 0)]);
        assert_eq!(r.game.summary().max_combo, 5);
    }

    #[test]
    fn a_hit_slider_head_no_longer_swallows_clicks() {
        // A circle sits under the slider's head while the slider is active.
        let mut b = builder();
        b.slider(1_000_000, &[px(100.0, 100.0), px(300.0, 100.0)], 1, 1_000_000, &[]);
        b.circle(1_001_000, to_fx(100.0), to_fx(100.0)); // within the note lock leniency
        let mut r = Run::new(Arc::new(b.build()));
        r.mv(900_000, 100.0, 100.0).press(1_000_000, Button::K1).press(1_001_000, Button::K2);
        assert!(matches!(r.events[0], Event::SliderHead { hit: true, .. }));
        assert_eq!(r.results(), vec![(1, HitResult::Hit300, 0)]);
    }

    #[test]
    fn a_very_short_slider_resolves_head_and_grade_together() {
        // 20 ms long: the head's window would outlast the slider.
        let mut b = builder();
        b.slider(1_000_000, &[px(100.0, 100.0), px(110.0, 100.0)], 1, 20_000, &[(1_010_000, NestedKind::Tail)]);
        let mut r = Run::new(Arc::new(b.build()));
        r.until(2_000_000);
        assert_eq!(
            r.events,
            vec![
                Event::SliderPoint { object: 0, kind: NestedKind::Tail, hit: false, t: 1_010_000 },
                Event::SliderHead { object: 0, hit: false, t: 1_020_000, error: 0 },
                Event::Judged { object: 0, result: HitResult::Miss, t: 1_020_000, error: 0 },
            ]
        );
    }

    // ---- spinners ----------------------------------------------------

    /// Circle the centre at `rpm` from `from` to `to`, sampling every 5 ms.
    fn whirl(r: &mut Run, from: Time, to: Time, rpm: f64) {
        let mut t = from;
        while t <= to {
            let a = (t - from) as f64 / 60e6 * rpm * std::f64::consts::TAU;
            r.mv(t, 256.0 + 80.0 * a.cos() as f32, 192.0 + 80.0 * a.sin() as f32);
            t += 5_000;
        }
    }

    fn spinner_chart() -> Arc<Chart> {
        let mut b = builder();
        b.spinner(1_000_000, 3_000_000); // 2 s at OD 8: 6.5/s -> 13 half turns
        Arc::new(b.build())
    }

    #[test]
    fn spun_out_turn_restores_health_before_later_drain() {
        let mut chart = spinner_chart();
        Arc::get_mut(&mut chart).unwrap().drain_rate = crate::hp::FULL / 2_000_000;
        let mut r = Run::new(chart);
        r.game.enable_live_spun_out();
        r.game.hp = crate::hp::FULL / 10;
        r.game.hp_time = 1_000_000;
        r.until(1_130_000);
        assert!(r.game.hp() > 0.05, "turn must restore health: {}", r.game.hp());
    }

    #[test]
    fn spun_out_steps_preserve_order_health_and_failure() {
        let mut b = builder();
        b.spinner(1_000_000, 3_000_000);
        b.spinner(1_050_000, 2_500_000);
        let mut chart = Arc::new(b.build());
        Arc::get_mut(&mut chart).unwrap().drain_rate = crate::hp::FULL / 1_000_000;
        let run = |step: i64, initial: i64| {
            let mut r = Run::new(chart.clone());
            r.game.enable_live_spun_out();
            r.game.hp = initial;
            r.game.hp_time = 1_000_000;
            let mut t = 1_000_000;
            while t < 2_000_000 && r.game.failed().is_none() {
                t = (t + step).min(2_000_000);
                r.until(t);
            }
            r
        };
        for initial in [crate::hp::FULL / 2, crate::hp::FULL / 10] {
            let large = run(1_000_000, initial);
            let small = run(5_000, initial);
            assert_eq!(large.game.failed(), small.game.failed());
            assert_eq!(large.events, small.events);
            assert_eq!(large.game.hp, small.game.hp);
            let times: Vec<_> = large.events.iter().filter_map(|e| match e {
                Event::Spin { t, .. } => Some(*t), _ => None,
            }).collect();
            assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
            if initial == crate::hp::FULL / 10 {
                assert!(large.game.failed().is_some());
                assert!(times.is_empty(), "no turns may be credited after failure");
            }
        }
    }

    #[test]
    fn spun_out_rejects_input_after_drain_failure() {
        let mut chart = circles(&[(1_000, 100.0, 100.0), (1_500, 100.0, 100.0)]);
        Arc::get_mut(&mut chart).unwrap().drain_rate = crate::hp::FULL / 1_000_000;
        let mut game = Game::with_mods(chart, crate::mods::SPUN_OUT);
        game.enable_live_spun_out();
        game.hp = crate::hp::FULL / 10;
        game.hp_time = 1_000_000;
        game.time = 1_000_000;
        game.cursor = px(100.0, 100.0);
        let mut events = Vec::new();
        let applied = game.push_until(Input { t: 1_500_000, kind: InputKind::Press(Button::K1) }, &mut |e| events.push(e), &|| false);
        assert!(game.failed().is_some());
        assert_eq!(applied, None);
        assert!(events.iter().all(|e| !matches!(e, Event::Judged { result: HitResult::Hit300, .. })));
        assert_eq!(game.buttons(), 0);
        assert_eq!(game.time(), game.failed().unwrap());
    }

    #[test]
    fn spun_out_does_not_judge_after_drain_failure() {
        let mut b = builder();
        b.spinner(1_000_000, 3_000_000);
        b.circle(1_000_000, to_fx(100.0), to_fx(100.0));
        let mut chart = Arc::new(b.build());
        Arc::get_mut(&mut chart).unwrap().drain_rate = crate::hp::FULL / 1_000_000;
        let mut r = Run::new(chart);
        r.game.enable_live_spun_out();
        r.game.hp = crate::hp::FULL / 100;
        r.game.hp_time = 1_000_000;
        r.until(2_000_000);
        assert!(r.game.failed().is_some());
        assert!(r.events.is_empty(), "events after failure: {:?}", r.events);
    }

    #[test]
    fn spinning_enough_clears_the_spinner() {
        let c = spinner_chart();
        assert_eq!(c.objects[0].extra, 13);
        let mut r = Run::new(c);
        r.mv(900_000, 336.0, 192.0).press(1_000_000, Button::K1);
        whirl(&mut r, 1_000_000, 3_000_000, 300.0); // 10 turns
        r.until(3_100_000);
        assert_eq!(r.results(), vec![(0, HitResult::Hit300, 0)]);
        let turns = r.events.iter().filter(|e| matches!(e, Event::Spin { .. })).count();
        assert!((9..=10).contains(&turns), "{turns} full turns reported");
        // 13 half turns needed; bonus from the turn that reaches 18 halves.
        let bonus = r.events.iter().filter(|e| matches!(e, Event::Spin { bonus: true, .. })).count();
        assert_eq!(bonus, turns - 8);
        assert_eq!(r.game.combo(), 1);
    }

    #[test]
    fn partial_spinning_grades_down_and_none_is_a_miss() {
        // 6.5 turns needed. 180 rpm for 2 s is 6 turns: 92% -> 100.
        let mut r = Run::new(spinner_chart());
        r.mv(900_000, 336.0, 192.0).press(1_000_000, Button::K1);
        whirl(&mut r, 1_000_000, 3_000_000, 180.0);
        r.until(3_100_000);
        assert_eq!(r.results(), vec![(0, HitResult::Hit100, 0)]);
        // 150 rpm is 5 turns: 77% -> 50.
        let mut r = Run::new(spinner_chart());
        r.mv(900_000, 336.0, 192.0).press(1_000_000, Button::K1);
        whirl(&mut r, 1_000_000, 3_000_000, 150.0);
        r.until(3_100_000);
        assert_eq!(r.results(), vec![(0, HitResult::Hit50, 0)]);
        let mut r = Run::new(spinner_chart());
        r.until(3_100_000);
        assert_eq!(r.results(), vec![(0, HitResult::Miss, 0)]);
    }

    #[test]
    fn spinning_needs_a_button_and_is_capped() {
        // No button: nothing counts.
        let mut r = Run::new(spinner_chart());
        whirl(&mut r, 1_000_000, 3_000_000, 300.0);
        assert_eq!(r.game.spinner_turns(0), 0.0);
        // 2000 rpm for 2 s would be 66 turns; the cap allows 477 rpm = 15.9.
        let mut r = Run::new(spinner_chart());
        r.mv(900_000, 336.0, 192.0).press(1_000_000, Button::K1);
        whirl(&mut r, 1_000_000, 3_000_000, 2000.0);
        let turns = r.game.spinner_turns(0);
        assert!(turns > 15.0 && turns <= 15.91, "{turns}");
    }

    #[test]
    fn the_angle_approximation_is_monotonic_and_closes_the_circle() {
        let mut last = 0u32;
        let mut total = 0u64;
        for i in 1..=3600 {
            let a = i as f64 / 3600.0 * std::f64::consts::TAU;
            let now = angle((a.cos() * 1e6) as i64, (a.sin() * 1e6) as i64);
            let step = now.wrapping_sub(last) as i32;
            assert!(step > 0, "angle went backwards at step {i}");
            total += step as u64;
            last = now;
        }
        assert!(total.abs_diff(TURN) < TURN / 10_000, "a full circle measured {total}");
        assert_eq!(angle(1, 0), 0);
        assert_eq!(angle(0, 1), 1 << 30);
        assert_eq!(angle(-1, 0), 1 << 31);
    }

    // ---- determinism -------------------------------------------------

    /// Small deterministic generator for the property test.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            lo + (self.next() % (hi - lo) as u64) as i64
        }
    }

    /// The property everything else leans on: how the caller slices time
    /// into `advance_to` calls must not change anything.
    #[test]
    fn slicing_time_differently_gives_identical_results() {
        let mut seen = [false; 8];
        for seed in 0..200u64 {
            let mut rng = Rng(seed * 7919 + 1);
            // A dense map of everything: objects 30..400 ms apart, clustered
            // in space so stacks, note locks and overlaps all occur.
            let mut b = builder();
            let mut t = 1000 * MS;
            for _ in 0..60 {
                t += rng.range(0, 400) * MS;
                let (x, y) = (rng.range(100, 220) as f32, rng.range(100, 220) as f32);
                match rng.range(0, 10) {
                    0 => {
                        b.spinner(t, t + rng.range(100, 900) * MS);
                    }
                    1..=4 => {
                        let spans = rng.range(1, 4) as u32;
                        let span = rng.range(60, 500) * MS;
                        let end = t + spans as Time * span;
                        let mut nested: Vec<_> =
                            (1..spans as Time).map(|s| (t + s * span, NestedKind::Repeat)).collect();
                        for _ in 0..rng.range(0, 4) {
                            nested.push((rng.range(t, end), NestedKind::Tick));
                        }
                        nested.push(((t + (end - t) / 2).max(end - 36 * MS), NestedKind::Tail));
                        let to = (x + rng.range(-80, 80) as f32, y + rng.range(-80, 80) as f32);
                        b.slider(t, &[px(x, y), px(to.0, y), px(to.0, to.1)], spans, span, &nested);
                    }
                    _ => {
                        b.circle(t, to_fx(x), to_fx(y));
                    }
                }
            }
            let c = Arc::new(b.build());
            // A player who is sloppy in every way: wandering cursor, presses
            // around object times with up to +-200 ms of error, holds of
            // random length (which is what follows sliders and spinners).
            let mut inputs = Vec::new();
            for o in &c.objects {
                let around = o.t + rng.range(-200_000, 200_000);
                let jitter = |rng: &mut Rng| (rng.range(-40, 40) * FX_ONE as i64) as i32;
                inputs.push(Input {
                    t: around - rng.range(0, 30_000),
                    kind: InputKind::Move { x: o.x + jitter(&mut rng), y: o.y + jitter(&mut rng) },
                });
                let button = [Button::K1, Button::K2, Button::M1, Button::M2][rng.range(0, 4) as usize];
                if rng.range(0, 10) > 0 {
                    let held = rng.range(1_000, (o.end - o.t).max(90_000) + 100_000);
                    inputs.push(Input { t: around, kind: InputKind::Press(button) });
                    inputs.push(Input { t: around + held, kind: InputKind::Release(button) });
                    // Wander while holding, roughly along sliders and around spinners.
                    let mut mt = around;
                    while mt < around + held {
                        mt += rng.range(2_000, 40_000);
                        let (bx, by) = if o.kind == ObjectKind::Slider { c.ball_at(o, mt) } else { (o.x, o.y) };
                        let wide = if o.kind == ObjectKind::Spinner { 4 } else { 1 };
                        inputs.push(Input {
                            t: mt,
                            kind: InputKind::Move { x: bx + jitter(&mut rng) * wide, y: by + jitter(&mut rng) * wide },
                        });
                    }
                }
            }
            inputs.sort_by_key(|i| i.t);
            let end = c.objects.iter().map(|o| o.end).max().unwrap() + 1_000_000;

            // Reference: inputs only, one advance at the very end.
            let mut ref_events = Vec::new();
            let mut g = Game::new(c.clone());
            for &i in &inputs {
                g.push(i, &mut |e| ref_events.push(e));
            }
            g.advance_to(end, &mut |e| ref_events.push(e));
            let reference = g.summary();
            assert!(g.finished(), "seed {seed}: objects left open");
            assert_eq!(Game::simulate(c.clone(), 0, &inputs, end), reference);

            // The same inputs with random extra advances in between, the
            // way a frame loop would call it.
            let mut events = Vec::new();
            let mut g = Game::new(c.clone());
            let mut now = 0;
            for &i in &inputs {
                while now < i.t && rng.range(0, 3) > 0 {
                    now = (now + rng.range(1, 30_000)).min(i.t);
                    g.advance_to(now, &mut |e| events.push(e));
                }
                g.push(i, &mut |e| events.push(e));
                now = now.max(i.t);
            }
            while now < end {
                now = (now + rng.range(1, 50_000)).min(end);
                g.advance_to(now, &mut |e| events.push(e));
            }
            assert_eq!(events, ref_events, "seed {seed}");
            assert_eq!(g.summary(), reference, "seed {seed}");

            // Events must come out in time order.
            let time_of = |e: &Event| match *e {
                Event::Judged { t, .. } | Event::Blocked { t, .. } | Event::SliderHead { t, .. } => t,
                Event::SliderPoint { t, .. } | Event::Spin { t, .. } => t,
            };
            assert!(ref_events.windows(2).all(|w| time_of(&w[0]) <= time_of(&w[1])), "seed {seed}");

            // The test only means something if every kind of outcome occurs.
            for e in &ref_events {
                let k = match *e {
                    Event::Judged { result: HitResult::Miss, .. } => 0,
                    Event::Judged { .. } => 1,
                    Event::Blocked { .. } => 2,
                    Event::SliderHead { hit, .. } => 3 + hit as usize,
                    Event::SliderPoint { hit, .. } => 5 + hit as usize,
                    Event::Spin { .. } => 7,
                };
                seen[k] = true;
            }
        }
        assert_eq!(seen, [true; 8], "some outcome never happened: {seen:?}");
    }

    #[test]
    fn a_clone_continues_identically() {
        // Seeking will restore snapshots; a snapshot is just a clone.
        let mut b = builder();
        for i in 0..20 {
            b.circle((1000 + i * 600) * MS, to_fx(100.0), to_fx(100.0));
            b.slider(
                (1300 + i * 600) * MS,
                &[px(200.0, 200.0), px(300.0, 200.0)],
                1,
                200 * MS,
                &[((1400 + i * 600) * MS, NestedKind::Tick)],
            );
        }
        let c = Arc::new(b.build());
        let inputs = crate::auto::autoplay(&c, 200);
        let mut a = Game::new(c.clone());
        for &i in &inputs[..inputs.len() / 2] {
            a.push(i, &mut |_| {});
        }
        let mut b = a.clone();
        for &i in &inputs[inputs.len() / 2..] {
            a.push(i, &mut |_| {});
            b.push(i, &mut |_| {});
        }
        assert_eq!(a.summary(), b.summary());
        assert_eq!(a.summary().miss, 0);
    }
}
