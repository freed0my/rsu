//! Playing a beatmap: feeding input to the core, drawing its state, and
//! recording the session as a replay.
//!
//! The split matters. The core decides everything from timestamped events;
//! this module only translates (window pixels to osu! pixels, the audio
//! clock to map time) and draws. The cursor the player sees is latched by
//! the renderer and is not part of the game state at all.

use crate::skin::Skin;
use crate::text::{self, StackStr};
use std::fmt::Write;
use std::sync::Arc;
use rsu_audio::clock::ClockModel;
use rsu_audio::SAMPLE_RATE;
use rsu_core::game::{Button, Event, Game, HitResult, Input, InputKind, Summary};
use rsu_core::{from_fx, to_fx, Chart, Fx, NestedKind, ObjectKind, Time, MS};
use rsu_formats::hitsound::Sections;
use rsu_formats::osr::Frame;
use rsu_formats::osu::PracticeDifficulty;
use rsu_gfx::Batch;
use rsu_telemetry::Running;

/// Maps the 512x384 osu! playfield onto the window, the way the original
/// lays it out: a 640x480 field fitted to the window, playfield centred and
/// nudged 8 px down.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub scale: f32,
    pub ox: f32,
    pub oy: f32,
}

impl Field {
    pub fn new(w: f32, h: f32) -> Field {
        let scale = (w / 640.0).min(h / 480.0);
        Field { scale, ox: (w - 512.0 * scale) * 0.5, oy: (h - 384.0 * scale) * 0.5 + 8.0 * scale }
    }
    pub fn to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        (self.ox + x * self.scale, self.oy + y * self.scale)
    }
    pub fn to_osu(&self, x: f32, y: f32) -> (f32, f32) {
        ((x - self.ox) / self.scale, (y - self.oy) / self.scale)
    }
}

/// How far behind "now" time-driven judgements run. An input event carries
/// the kernel's timestamp but reaches us a little later; the core must not
/// be advanced past a moment an event can still arrive for.
const SETTLE: Time = 3 * MS;

/// How far ahead of the music the sounds of a slider's ticks, repeats and
/// end are decided and handed to the audio thread, which starts each on its
/// exact frame. Long enough for a frame and an audio cycle to pass before
/// the moment; short enough that where the cursor is now still tells where
/// it will be. Real time; at a speed mod it is more of the map.
const SPECULATE: Time = 6 * MS;

/// Something that just happened and is still being shown.
#[derive(Clone, Copy)]
struct Effect {
    /// Where, in osu! pixels.
    x: f32,
    y: f32,
    /// A final judgement, or `None` for a part of a slider being hit.
    result: Option<HitResult>,
    t: Time,
}

/// Everything the core's events feed, kept apart from the game so the event
/// sink can borrow it while the game is being driven.
struct Feedback {
    effects: [Option<Effect>; 64],
    next_effect: usize,
    /// Recent hit errors in ms, oldest first.
    errors: [f32; 32],
    errors_len: usize,
    error_stats: Running,
    /// Sounds to play for what happened since the last frame.
    cues: [Cue; 32],
    cues_len: usize,
    /// Slider moments whose sound was decided ahead (object, time, whether
    /// it was started), so that the judgement does not sound them again.
    planned: [(u32, Time, bool); 64],
    planned_next: usize,
}

/// Samples to play for one slider part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundIds {
    /// Ids in the mixer's sample table.
    pub ids: [u16; 4],
    pub count: u8,
    /// Each sample's own level in percent.
    pub levels: [u8; 4],
    /// 0..=100.
    pub volume: u8,
    /// -100 (left) ..= 100 (right).
    pub pan: i8,
}

impl Default for SoundIds {
    fn default() -> SoundIds {
        SoundIds { ids: [0; 4], count: 0, levels: [100; 4], volume: 100, pan: 0 }
    }
}

impl SoundIds {
    pub fn one(id: u16, volume: u8) -> SoundIds {
        SoundIds { ids: [id, 0, 0, 0], count: 1, volume, ..SoundIds::default() }
    }
}

/// The sounds of one object, with samples already looked up.
#[derive(Clone, Debug, Default)]
pub struct ObjectCues {
    /// Circles and spinners: one. Sliders: head, each repeat, tail.
    pub edges: Vec<SoundIds>,
    /// Sliders: the time and sound of each tick.
    pub ticks: Vec<(Time, SoundIds)>,
}

/// Everything a map sounds like, with samples already looked up. Empty
/// (the default) is a silent map.
#[derive(Clone, Debug, Default)]
pub struct MapSounds {
    pub objects: Vec<ObjectCues>,
    /// Sample set and volume over time, for the sounds below.
    pub sections: Sections,
    /// The loop of a held slider, per sample set (normal, soft, drum).
    pub slide: [Option<u16>; 3],
    /// The loop of a spinner being spun.
    pub spin: Option<u16>,
    /// A spinner turned beyond what it needed.
    pub bonus: Option<u16>,
    pub combo_break: Option<u16>,
}

/// A looping sound that should be heard right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopState {
    pub sample: u16,
    /// 0..=100.
    pub volume: u8,
    pub pan: i8,
    /// Playback speed; pitch follows.
    pub rate: f32,
}

/// Where a sound sits between the speakers for an object at osu! x
/// coordinate `x`, as McOsu places it (`OsuGameRules::osuCoords2Pan`).
fn pan_for(x: Fx) -> i8 {
    ((from_fx(x) / 512.0 - 0.5) * 0.8 * 100.0).clamp(-100.0, 100.0) as i8
}

/// A sound the game wants played now.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cue {
    pub sound: SoundIds,
    /// Whether a click caused it directly (as opposed to time passing), so
    /// its delay can be measured against that key press.
    pub from_click: bool,
    /// Start on this timeline frame of the music instead of at once.
    pub at: Option<f64>,
}

impl Feedback {
    /// Ask for a sound at osu! x coordinate `x`. A sample that is already
    /// starting in this frame is not started a second time, and a section
    /// at volume zero is silent (both as in McOsu).
    fn cue(&mut self, sound: Option<&SoundIds>, from_click: bool, x: Fx) {
        self.cue_at(sound, from_click, x, None);
    }

    fn cue_at(&mut self, sound: Option<&SoundIds>, from_click: bool, x: Fx, at: Option<f64>) {
        let Some(&asked) = sound.filter(|s| s.volume > 0) else {
            return;
        };
        let mut sound = SoundIds { count: 0, pan: pan_for(x), ..asked };
        for n in 0..asked.count as usize {
            let id = asked.ids[n];
            let starting =
                self.cues[..self.cues_len].iter().any(|c| c.sound.ids[..c.sound.count as usize].contains(&id));
            if !starting {
                sound.ids[sound.count as usize] = id;
                sound.levels[sound.count as usize] = asked.levels[n];
                sound.count += 1;
            }
        }
        if sound.count > 0 && self.cues_len < self.cues.len() {
            self.cues[self.cues_len] = Cue { sound, from_click, at };
            self.cues_len += 1;
        }
    }

    /// Whether the sound of this slider moment was already started ahead.
    fn played_ahead(&self, object: u32, t: Time) -> bool {
        self.planned.iter().any(|&(o, at, played)| played && o == object && at == t)
    }

    fn planned(&self, object: u32, t: Time) -> bool {
        self.planned.iter().any(|&(o, at, _)| o == object && at == t)
    }

    fn plan(&mut self, object: u32, t: Time, played: bool) {
        self.planned[self.planned_next] = (object, t, played);
        self.planned_next = (self.planned_next + 1) % self.planned.len();
    }

    /// Decide, ahead of the judgement, the sounds of the slider ticks,
    /// repeats and ends that fall before `upto`, and hand them out with the
    /// music frame they belong to. The judgement later finds them done. When
    /// the forecast was wrong the judgement still has the last word on
    /// score; a sound that should have been silent is the worst that
    /// happens, and a sound that was forecast silent is made late, as before.
    fn speculate(&mut self, game: &Game, chart: &Chart, sounds: &MapSounds, upto: Time, lead: f64, offset: Time) {
        let frames = |t: Time| lead + (t + offset) as f64 * SAMPLE_RATE as f64 / 1e6;
        for i in game.first_open()..chart.objects.len() {
            let o = &chart.objects[i];
            if o.t > upto {
                break;
            }
            if o.kind != ObjectKind::Slider || !game.is_open(i) {
                continue;
            }
            let cues = sounds.objects.get(i);
            let s = chart.slider(o);
            let nested = chart.slider_nested(s);
            for n in nested.iter().skip(game.nested_done(i)).take_while(|n| n.t <= upto) {
                if n.kind == NestedKind::Tail || self.planned(i as u32, n.t) {
                    continue;
                }
                let hit = game.would_track(chart, i, (n.x, n.y));
                self.plan(i as u32, n.t, hit);
                if hit {
                    let sound = point_sound(chart, cues, o, n.kind, n.t);
                    self.cue_at(sound, false, n.x, Some(frames(n.t)));
                }
            }
            if o.end <= upto && !self.planned(i as u32, o.end) {
                let scored = game.slider_scored(i);
                self.plan(i as u32, o.end, scored);
                if scored {
                    let (x, _) = chart.slider_end_position(o);
                    self.cue_at(cues.and_then(|c| c.edges.last()), false, x, Some(frames(o.end)));
                }
            }
        }
    }

    fn effect(&mut self, pos: (Fx, Fx), result: Option<HitResult>, t: Time) {
        self.effects[self.next_effect] = Some(Effect { x: from_fx(pos.0), y: from_fx(pos.1), result, t });
        self.next_effect = (self.next_effect + 1) % self.effects.len();
    }

    /// Timing errors exist where a click was judged against a time: circles
    /// and slider heads.
    fn timing(&mut self, error: Time) {
        let ms = error as f32 / 1000.0;
        self.error_stats.push(ms as f64);
        if self.errors_len == self.errors.len() {
            self.errors.copy_within(1.., 0);
            self.errors_len -= 1;
        }
        self.errors[self.errors_len] = ms;
        self.errors_len += 1;
    }

    fn on_event(&mut self, chart: &Chart, sounds: &MapSounds, e: Event) {
        match e {
            Event::Judged { object, result, t, error } => {
                let o = &chart.objects[object as usize];
                let cues = sounds.objects.get(object as usize);
                let pos = match o.kind {
                    ObjectKind::Slider => chart.slider_end_position(o),
                    _ => (o.x, o.y),
                };
                self.effect(pos, Some(result), t);
                if result == HitResult::Miss {
                    return;
                }
                match o.kind {
                    ObjectKind::Circle => {
                        self.cue(cues.and_then(|c| c.edges.first()), true, o.x);
                        self.timing(error);
                    }
                    ObjectKind::Spinner => self.cue(cues.and_then(|c| c.edges.first()), false, o.x),
                    // The end of a slider sounds when the slider ends, unless the
                    // whole slider was missed (McOsu: `OsuSlider::onHit` with the end
                    // result); the tail check 36 ms earlier makes no sound.
                    ObjectKind::Slider if self.played_ahead(object, o.end) => {}
                    ObjectKind::Slider => self.cue(cues.and_then(|c| c.edges.last()), false, pos.0),
                }
            }
            Event::SliderHead { object, hit: true, t, error } => {
                let o = &chart.objects[object as usize];
                self.effect((o.x, o.y), None, t);
                self.cue(sounds.objects.get(object as usize).and_then(|c| c.edges.first()), true, o.x);
                self.timing(error);
            }
            Event::SliderPoint { object, kind, hit: true, t } => {
                let o = &chart.objects[object as usize];
                let ball = chart.ball_at(o, t);
                self.effect(ball, None, t);
                let cues = sounds.objects.get(object as usize);
                // Sounds forecast ahead of this moment have already started.
                let sound = match kind {
                    // Sounds with the slider's judgement, at its real end.
                    NestedKind::Tail => None,
                    _ if self.played_ahead(object, t) => None,
                    _ => point_sound(chart, cues, o, kind, t),
                };
                self.cue(sound, false, ball.0);
            }
            // A turn beyond what the spinner needed.
            Event::Spin { bonus: true, t, .. } => {
                let volume = sounds.sections.at(t).1;
                self.cue(sounds.bonus.map(|id| SoundIds::one(id, volume)).as_ref(), false, to_fx(256.0));
            }
            _ => {}
        }
    }
}

/// The sound of a slider's tick or repeat at `t`.
fn point_sound<'a>(
    chart: &Chart,
    cues: Option<&'a ObjectCues>,
    o: &rsu_core::HitObject,
    kind: NestedKind,
    t: Time,
) -> Option<&'a SoundIds> {
    match kind {
        // The tick nearest in time is the one that was reached.
        NestedKind::Tick => cues.and_then(|c| c.ticks.iter().min_by_key(|(at, _)| (at - t).abs())).map(|(_, s)| s),
        // The k-th repeat is edge k.
        NestedKind::Repeat => {
            let pass = ((t - o.t) / chart.slider(o).span_duration.max(1)) as usize;
            cues.and_then(|c| c.edges.get(pass))
        }
        NestedKind::Tail => cues.and_then(|c| c.edges.last()),
    }
}

/// The game state at one moment of a replay, to seek from.
struct Keyframe {
    t: Time,
    game: Game,
    /// Index of the first input not yet applied.
    next: usize,
}

/// Visual-only positions from the original stable frames. The core still
/// receives the unchanged discrete `Osr::inputs()` stream.
pub struct VisualTrack {
    frames: Vec<(Time, f32, f32)>,
}

impl VisualTrack {
    pub fn from_stable_frames(frames: &[Frame]) -> Self {
        let mut positions: Vec<(Time, f32, f32)> = Vec::with_capacity(frames.len());
        for frame in frames {
            // stable's initial (256, -500) is a placeholder, not a pen
            // position. A later out-of-bounds position is still recorded.
            if positions.is_empty() && frame.x == 256.0 && frame.y == -500.0 {
                continue;
            }
            let t = frame.t.saturating_mul(MS).max(positions.last().map_or(Time::MIN, |p| p.0));
            // The raw `.osr` and judged inputs stay untouched; only bound
            // visual coordinates so hostile but finite f32 values cannot
            // overflow interpolation or screen-space GPU geometry.
            let (x, y) = (frame.x.clamp(-4096.0, 4096.0), frame.y.clamp(-4096.0, 4096.0));
            if positions.last().is_some_and(|p| p.0 == t) {
                // The last frame at a timestamp is what is displayed there.
                *positions.last_mut().unwrap() = (t, x, y);
            } else {
                positions.push((t, x, y));
            }
        }
        Self { frames: positions }
    }

    /// Map-time sample. A dropout (>100 ms) holds until the next actual frame.
    /// This is visual only: no pen movement or buttons are recovered.
    pub fn position(&self, t: Time) -> Option<(f32, f32)> {
        let next = self.frames.partition_point(|f| f.0 <= t);
        let &(a, x, y) = self.frames.get(next.checked_sub(1)?)?;
        let Some(&(b, bx, by)) = self.frames.get(next) else { return Some((x, y)); };
        let gap = b.saturating_sub(a);
        if gap > 100 * MS {
            return Some((x, y));
        }
        let fraction = (t.saturating_sub(a) as f64 / gap as f64) as f32;
        Some((x + (bx - x) * fraction, y + (by - y) * fraction))
    }

    /// Fixed map-time samples, independent of render FPS and seek history.
    pub fn trail(&self, t: Time, mut draw: impl FnMut((f32, f32), u8)) {
        trail_samples(t, |at| self.position(at), &mut draw);
    }
}

fn trail_samples(t: Time, mut position: impl FnMut(Time) -> Option<(f32, f32)>, draw: &mut impl FnMut((f32, f32), u8)) {
    for sample in (0..=34).rev() {
        let age_ms = sample * 5;
        if let Some(pos) = position(t.saturating_sub(age_ms * MS)) {
            let alpha = (255 * (170 - age_ms) / 170) as u8;
            if alpha > 0 { draw(pos, alpha); }
        }
    }
}

/// A replay being played back instead of live input.
pub struct Playback {
    pub inputs: Vec<Input>,
    /// Native replay movement index for bounded-cost trail lookup through
    /// long stretches of buttons with no cursor movement.
    native_moves: Vec<(Time, f32, f32)>,
    pub next: usize,
    pub end_time: Time,
    /// The result the recording claims; `None` for generated autoplay.
    pub expected: Option<Summary>,
    /// Present only for imported `.osr`; never fed to the judge.
    pub visual: Option<VisualTrack>,
    /// Snapshots every `interval` of map time, the first one at the start.
    keyframes: Vec<Keyframe>,
    interval: Time,
    pub seeks: u32,
    /// How long the most recent seek took, in nanoseconds of real time.
    pub last_seek_ns: u64,
}

impl Playback {
    /// Simulates the whole replay once to place the keyframes. The spacing
    /// adapts to the map so that all snapshots together stay within a fixed
    /// memory budget.
    pub fn new(
        chart: &Arc<Chart>,
        mods: u32,
        inputs: Vec<Input>,
        end_time: Time,
        expected: Option<Summary>,
    ) -> Playback {
        const BUDGET_BYTES: usize = 64 << 20;
        let per_keyframe = chart.objects.len() * 48 + 256;
        let max_keyframes = (BUDGET_BYTES / per_keyframe).clamp(4, 600) as Time;
        let start = inputs.first().map_or(0, |i| i.t).min(0);
        let interval = ((end_time - start) / max_keyframes).max(1_000_000);

        let mut game = Game::with_mods(chart.clone(), mods);
        let mut keyframes = vec![Keyframe { t: Time::MIN, game: game.clone(), next: 0 }];
        let mut sink = |_| {};
        let mut at = start + interval;
        for (n, &input) in inputs.iter().enumerate() {
            while input.t > at {
                game.advance_to(at, &mut sink);
                keyframes.push(Keyframe { t: at, game: game.clone(), next: n });
                at += interval;
            }
            game.push(input, &mut sink);
        }
        let native_moves = inputs.iter().filter_map(|i| match i.kind {
            InputKind::Move { x, y } => Some((i.t, from_fx(x).clamp(-4096.0, 4096.0), from_fx(y).clamp(-4096.0, 4096.0))),
            _ => None,
        }).collect();
        Playback { inputs, native_moves, next: 0, end_time, expected, visual: None, keyframes, interval, seeks: 0, last_seek_ns: 0 }
    }

    pub fn with_stable_visual(mut self, frames: &[Frame]) -> Self {
        self.visual = Some(VisualTrack::from_stable_frames(frames));
        self.native_moves = Vec::new();
        self
    }

    /// The replay cursor's short-lived trail, sampled from recorded map
    /// time. Native `.tkr` uses its discrete moves; stable `.osr` uses only
    /// the separate smooth visual track. Neither affects judged inputs.
    pub fn trail(&self, t: Time, mut draw: impl FnMut((f32, f32), u8)) {
        if let Some(visual) = &self.visual {
            visual.trail(t, draw);
        } else {
            trail_samples(t, |at| {
                let end = self.native_moves.partition_point(|p| p.0 <= at);
                self.native_moves.get(end.checked_sub(1)?).map(|p| (p.1, p.2))
            }, &mut draw);
        }
    }

    /// Reconstruct a skin's legacy trail from map time, not render history.
    /// `joined` corresponds to skins with a cursor middle. `spacing` is in
    /// playfield units and is derived from the trail sprite's screen width.
    pub fn skin_trail(&self, t: Time, joined: bool, spacing: f32, mut draw: impl FnMut((f32, f32), u8)) {
        let position = |at| {
            if let Some(visual) = &self.visual {
                visual.position(at)
            } else {
                let end = self.native_moves.partition_point(|p| p.0 <= at);
                self.native_moves.get(end.checked_sub(1)?).map(|p| (p.1, p.2))
            }
        };
        if !joined {
            // The reference legacy viewer emits disjoint parts at 60 Hz and
            // fades them over 150 ms. Anchor the phase to map time for seeks.
            let frame = t.saturating_mul(60).div_euclid(1_000_000);
            for n in (frame.saturating_sub(9)..=frame).take(10) {
                let at = n.saturating_mul(1_000_000).div_euclid(60);
                let age = t.saturating_sub(at);
                if age >= 150 * MS { continue; }
                if let Some(p) = position(at) {
                    draw(p, (255 * (150 * MS - age) / (150 * MS)) as u8);
                }
            }
            return;
        }
        // Joined trails use a longer fade and space stamps by image width.
        // Bound work even for corrupt positions and tiny sprites.
        let spacing = if spacing.is_finite() { spacing.max(1.0) } else { 1.0 };
        let mut previous: Option<((f32, f32), Time)> = None;
        let mut stamps = [((0.0, 0.0), 0u8); 700];
        let mut total = 0usize;
        for i in (0..=100).rev() {
            let at = t.saturating_sub(i * 5 * MS);
            let Some(p) = position(at) else { continue; };
            if let Some((last, last_at)) = previous {
                let dx: f32 = p.0 - last.0;
                let dy: f32 = p.1 - last.1;
                let count = ((dx.hypot(dy) / spacing) as usize).min(48);
                for k in 1..=count {
                    let f = k as f32 / count as f32;
                    let stamp_time = last_at + ((at - last_at) as f32 * f) as Time;
                    let alpha = (255 * (500 * MS - t.saturating_sub(stamp_time)).max(0) / (500 * MS)) as u8;
                    if alpha > 0 {
                        stamps[total % stamps.len()] = ((last.0 + dx * f, last.1 + dy * f), alpha);
                        total += 1;
                    }
                }
                if count > 0 { previous = Some((p, at)); }
            } else {
                previous = Some((p, at));
            }
        }
        for i in total.saturating_sub(stamps.len())..total {
            let (p, alpha) = stamps[i % stamps.len()];
            draw(p, alpha);
        }
    }

    pub fn keyframe_count(&self) -> usize {
        self.keyframes.len()
    }
}

pub struct Play {
    pub chart: Arc<Chart>,
    pub game: Game,
    pub title: String,
    pub map_md5: [u8; 16],
    pub mods: u32,
    /// The player's own playback rate in percent (0: what the mods give).
    pub rate_pct: u16,
    pub practice: PracticeDifficulty,
    /// What to play when; may be empty (silent).
    pub(crate) sounds: MapSounds,
    /// The looping sounds wanted as of the last update: a held slider's,
    /// a spinner's.
    loops: [Option<LoopState>; 2],
    /// First object that may still make a looping sound.
    loop_from: usize,
    /// Whether no combo has been broken yet (the first break always sounds).
    first_break: bool,
    /// What the beatmap itself is shifted by (osu!'s local and online
    /// offsets), in map time: the map plays this much later.
    pub map_offset: Time,
    /// Stream position (frames) of map time 0.
    lead_in_frames: f64,
    /// Universal offset, in real time: how much later than the audio
    /// device admits the sound actually reaches the player. The whole game
    /// (what is drawn, what is judged, when sounds are due) runs this much
    /// behind the device's clock, so picture and judgement stay together
    /// and both meet the music.
    pub offset: Time,
    /// Map time at which the offset was last changed by hand, to show it.
    offset_changed: Option<Time>,
    pub playback: Option<Playback>,
    /// The inputs exactly as the core applied them: this *is* the replay.
    pub recorded: Vec<Input>,
    pub record_overflow: bool,
    /// Inputs whose timestamp had to be raised to keep time monotonic.
    pub late_inputs: u64,
    /// This frame's inputs, gathered from all sources before sorting.
    pending: Vec<Input>,
    feedback: Feedback,
    /// First object that may still need drawing.
    draw_from: usize,
    /// Map time of the frame being drawn.
    pub now: Time,
    pub done: bool,
    /// A live play whose health ran out; watching a replay never fails.
    pub failed: bool,
    pub retry_requested: bool,
    /// The health the bar shows, which follows the real one (McOsu's
    /// smoothing), and when it was last moved.
    hp_shown: f32,
    hp_moved: Option<std::time::Instant>,
    /// Playback speed and pause state, for display only.
    pub view_speed: f32,
    pub view_paused: bool,
}

/// How long before the next object a skippable stretch starts to be offered.
const SKIP_AFTER: Time = 5_000 * MS;
/// How long before the object a skip lands.
const SKIP_LEAD: Time = 2_500 * MS;

impl Play {
    pub fn new(
        chart: Arc<Chart>,
        title: String,
        map_md5: [u8; 16],
        lead_in: Time,
        offset: Time,
        mods: u32,
        sounds: MapSounds,
        playback: Option<Playback>,
    ) -> Play {
        let mut game = Game::with_mods(chart.clone(), mods);
        if playback.is_none() && mods & rsu_core::mods::SPUN_OUT != 0 { game.enable_live_spun_out(); }
        Play {
            game,
            chart,
            title,
            map_md5,
            mods,
            rate_pct: 0,
            practice: PracticeDifficulty::default(),
            sounds,
            loops: [None; 2],
            loop_from: 0,
            first_break: true,
            map_offset: 0,
            lead_in_frames: lead_in as f64 * SAMPLE_RATE as f64 / 1e6,
            offset,
            offset_changed: None,
            playback,
            // Room for ten minutes of a 1000 Hz device plus keys, allocated
            // up front so recording never allocates during play.
            recorded: Vec::with_capacity(1 << 20),
            record_overflow: false,
            late_inputs: 0,
            pending: Vec::with_capacity(1024),
            feedback: Feedback {
                effects: [None; 64],
                next_effect: 0,
                errors: [0.0; 32],
                errors_len: 0,
                error_stats: Running::default(),
                cues: [Cue::default(); 32],
                cues_len: 0,
                planned: [(u32::MAX, 0, false); 64],
                planned_next: 0,
            },
            draw_from: 0,
            now: Time::MIN,
            done: false,
            failed: false,
            retry_requested: false,
            hp_shown: 1.0,
            hp_moved: None,
            view_speed: 1.0,
            view_paused: false,
        }
    }

    /// Timeline position (frames) of a map time: what the audio transport
    /// has to be told to get there.
    pub fn timeline_frames(&self, map_time: Time) -> f64 {
        self.lead_in_frames + (map_time + self.offset_in_map_time() + self.map_offset) as f64 * SAMPLE_RATE as f64 / 1e6
    }

    /// How fast the map runs: the mods' speed, or the player's own rate.
    pub fn speed(&self) -> f64 {
        rsu_core::mods::speed_with(self.mods, self.rate_pct)
    }

    /// The offset is a real delay; with a speed mod that much real time is
    /// more (or less) of the map.
    fn offset_in_map_time(&self) -> Time {
        (self.offset as f64 * self.speed()).round() as Time
    }

    /// Move the offset by whole milliseconds, while playing.
    pub fn nudge_offset(&mut self, ms: i64) {
        self.offset = (self.offset + ms * MS).clamp(-500 * MS, 500 * MS);
        self.offset_changed = Some(self.now);
    }

    /// Where the skip button would take the player, as a map time, if this
    /// is a stretch with nothing to play (McOsu `OsuBeatmap::skipEmptySection`):
    /// the next object is more than 5 s away, and (after the first one) a
    /// second has passed since the last. It lands 2.5 s before that object.
    pub fn skip_target(&self) -> Option<Time> {
        let objects = &self.chart.objects;
        let now = self.now;
        let next = objects.partition_point(|o| o.t <= now);
        let first = objects.get(next)?;
        let after_previous = match next.checked_sub(1) {
            Some(p) => now > objects[p].end + 1_000 * MS,
            None => true,
        };
        if first.t - now <= SKIP_AFTER || !after_previous {
            return None;
        }
        // Slowed play is not compensated within the usual speeds, and a
        // very slow one is capped.
        let speed = self.speed();
        let factor = if (0.74..=1.51).contains(&speed) || speed > 1.0 { 1.0 } else { speed.max(0.2) };
        Some(first.t - (SKIP_LEAD as f64 * factor) as Time)
    }

    /// Where the skip button is, in window pixels: the bottom right corner.
    pub fn skip_rect(skin: &Skin, w: f32, h: f32) -> crate::ui::Rect {
        let (rw, rh) = match skin.play_skip.as_ref().filter(|s| s.w > 0.0) {
            Some(s) => (s.w * h / 768.0, s.h * h / 768.0),
            None => (h * 0.2, h * 0.07),
        };
        crate::ui::Rect::new(w - rw, h - rh, rw, rh)
    }

    /// The last moment worth seeking to.
    pub fn end_time(&self) -> Time {
        match &self.playback {
            Some(pb) => pb.end_time,
            None => self.chart.objects.iter().map(|o| o.end).max().unwrap_or(0),
        }
    }

    /// Lead-in before the track: what the map asks for, but always enough
    /// for the first object's approach to be seen in full.
    pub fn lead_in_for(chart: &Chart, map_lead_in_ms: i64) -> Time {
        let first = chart.objects.first().map_or(0, |o| o.t);
        (map_lead_in_ms * MS).max(1_000_000 + chart.preempt - first).max(500_000)
    }

    /// Map time of a CLOCK_MONOTONIC instant, through the audio clock.
    pub fn map_time(&self, clock: &ClockModel, t_ns: u64) -> Time {
        let frames = clock.pos_at(t_ns) - self.lead_in_frames;
        (frames * 1e6 / SAMPLE_RATE as f64).round() as Time - self.offset_in_map_time() - self.map_offset
    }

    /// Queue a live input for this frame. Positions are window pixels.
    pub fn queue_move(&mut self, clock: &ClockModel, t_ns: u64, field: &Field, x: f32, y: f32) {
        let (ox, oy) = field.to_osu(x, y);
        let kind = InputKind::Move { x: to_fx(ox), y: to_fx(oy) };
        self.queue(Input { t: self.map_time(clock, t_ns), kind });
    }

    pub fn queue_button(&mut self, clock: &ClockModel, t_ns: u64, button: Button, pressed: bool) {
        let kind = if pressed { InputKind::Press(button) } else { InputKind::Release(button) };
        self.queue(Input { t: self.map_time(clock, t_ns), kind });
    }

    fn queue(&mut self, input: Input) {
        if self.playback.is_none() && self.pending.len() < self.pending.capacity() {
            self.pending.push(input);
        }
    }

    /// The sounds for what happened in the last `update`.
    pub fn cues(&self) -> &[Cue] {
        &self.feedback.cues[..self.feedback.cues_len]
    }

    /// Run the core up to the present.
    pub fn update(&mut self, clock: &ClockModel, now_ns: u64) {
        if self.failed || self.retry_requested { return; }
        let now = self.map_time(clock, now_ns);
        self.now = now;
        let restored = self.follow_clock_jump(now);
        let (lead, offset) = (self.lead_in_frames, self.offset_in_map_time() + self.map_offset);
        let speed = self.speed();
        let feedback = &mut self.feedback;
        feedback.cues_len = 0;
        let (chart, sounds) = (&*self.chart, &self.sounds);
        // A fresh closure at each use, so that `feedback` is free in between.
        let mod_break = std::cell::Cell::new(false);
        let live_mods = if self.playback.is_none() { self.mods } else { 0 };
        macro_rules! sink {
            () => {
                |e| {
                    if live_mods & rsu_core::mods::SUDDEN_DEATH != 0 {
                        mod_break.set(mod_break.get() || match e {
                            Event::Judged { result, .. } => result == HitResult::Miss || (live_mods & rsu_core::mods::PERFECT != 0 && result != HitResult::Hit300),
                            Event::SliderHead { hit: false, .. } => true,
                            Event::SliderPoint { hit: false, kind, .. } => kind != NestedKind::Tail,
                            _ => false,
                        });
                    }
                    feedback.on_event(chart, sounds, e)
                }
            };
        }
        let combo_before = self.game.combo();

        if let Some(pb) = &mut self.playback {
            let target = now.min(pb.end_time);
            let started = std::time::Instant::now();
            // A replay knows its whole future, so there is nothing to wait for.
            while pb.next < pb.inputs.len() && pb.inputs[pb.next].t <= now {
                self.game.push(pb.inputs[pb.next], &mut sink!());
                pb.next += 1;
            }
            self.game.advance_to(target, &mut sink!());
            self.done = pb.next == pb.inputs.len() && now >= pb.end_time;
            if restored {
                pb.last_seek_ns += started.elapsed().as_nanos() as u64;
                // Catching up is not something that just happened: no sounds.
                self.feedback.cues_len = 0;
            }
        } else {
            // Sources are merged by timestamp: a key read from evdev and a
            // pointer event from the window system are stamped differently
            // and must still be applied in the order they happened.
            self.pending.sort_by_key(|i| i.t);
            for &input in &self.pending {
                let Some(applied) = self.game.push_until(input, &mut sink!(), &|| mod_break.get()) else { break };
                self.late_inputs += u64::from(applied.t != input.t);
                if self.recorded.len() < self.recorded.capacity() {
                    self.recorded.push(applied);
                } else {
                    self.record_overflow = true;
                }
                if mod_break.get() { break; }
            }
            self.pending.clear();
            // The margin is real time; at 1.5x that is more map time.
            let settle = (SETTLE as f64 * speed) as Time;
            // The sounds that depend on the cursor being where it should be
            // are decided now for the moments just ahead and placed on the
            // music's own clock, instead of waiting for the judgement.
            let upto = now + (SPECULATE as f64 * speed) as Time;
            if !mod_break.get() {
                self.game.advance_to_until(now - settle, &mut sink!(), &|| mod_break.get());
                if !mod_break.get() {
                    feedback.speculate(&self.game, chart, sounds, upto, lead, offset);
                }
            }
            let last = self.chart.objects.last().map_or(0, |o| o.end.max(o.t));
            self.done = self.game.finished() && now > last + 1_500_000;
            self.retry_requested |= mod_break.get() && live_mods & rsu_core::mods::PERFECT != 0;
            self.failed |= self.game.failed().is_some() || (mod_break.get() && !self.retry_requested);
        }

        // A broken combo sounds if it was worth something: always the first
        // time, afterwards only above 20 (McOsu's `playMissSound`).
        if !restored && self.game.combo() < combo_before && (self.first_break || combo_before > 20) {
            self.first_break = false;
            let sound = self.sounds.combo_break.map(|id| SoundIds::one(id, 100));
            self.feedback.cue(sound.as_ref(), false, to_fx(256.0));
        }
        self.update_loops();
    }

    /// The looping sounds of this moment: the slide of a slider that is
    /// being followed and the hum of a spinner that is being turned, in the
    /// sample set and at the volume of the current timing section.
    fn update_loops(&mut self) {
        self.loops = [None; 2];
        let (chart, now) = (&self.chart, self.game.time());
        let objects = &chart.objects;
        while self.loop_from > 0 && objects[self.loop_from - 1].end >= now {
            self.loop_from -= 1; // a seek backwards
        }
        while self.loop_from < objects.len() && objects[self.loop_from].end < now {
            self.loop_from += 1;
        }
        if self.view_paused {
            return;
        }
        let (set, volume) = self.sounds.sections.at(now);
        for (i, o) in objects.iter().enumerate().skip(self.loop_from).take_while(|(_, o)| o.t <= now) {
            if o.end < now {
                continue;
            }
            match o.kind {
                ObjectKind::Slider if self.game.is_tracking(i) => {
                    let sample = self.sounds.slide[(set.clamp(1, 3) - 1) as usize];
                    let pan = pan_for(chart.ball_at(o, now).0);
                    self.loops[0] = sample.map(|sample| LoopState { sample, volume, pan, rate: 1.0 });
                }
                ObjectKind::Spinner if self.game.is_open(i) && self.game.spinner_turns(i) > 0.0 => {
                    // The hum rises with progress: 20 kHz to 120 kHz playback
                    // of a 44.1 kHz sample, as McOsu sets it.
                    let done = self.game.spinner_turns(i) / (o.extra as f32 / 2.0).max(0.5);
                    let rate = (20_000.0 + done.clamp(0.0, 2.5) * 40_000.0) / 44_100.0;
                    self.loops[1] = self.sounds.spin.map(|sample| LoopState { sample, volume, pan: 0, rate });
                }
                _ => {}
            }
        }
    }

    pub fn loops(&self) -> &[Option<LoopState>; 2] {
        &self.loops
    }

    /// Playback follows the clock wherever it goes. If the clock is behind
    /// the game (a seek backwards) or far ahead of it (a seek forwards),
    /// continue from the nearest snapshot. Returns whether it did.
    fn follow_clock_jump(&mut self, now: Time) -> bool {
        let Some(pb) = &mut self.playback else {
            return false;
        };
        let target = now.min(pb.end_time);
        let at = self.game.time();
        if !(now + 20 * MS < at || target.saturating_sub(at) > 2 * pb.interval) {
            return false;
        }
        let started = std::time::Instant::now();
        let k = pb.keyframes.partition_point(|k| k.t <= target).saturating_sub(1);
        let keyframe = &pb.keyframes[k];
        self.game.clone_from(&keyframe.game);
        pb.next = keyframe.next;
        self.feedback.effects = [None; 64];
        self.feedback.errors_len = 0;
        self.feedback.error_stats = Running::default();
        self.draw_from = 0;
        pb.seeks += 1;
        // Completed in `update`, once the inputs up to `now` are replayed.
        pb.last_seek_ns = started.elapsed().as_nanos() as u64;
        true
    }

    /// Where the judged, stepwise cursor is, in window pixels.
    pub fn game_cursor(&self, field: &Field) -> (f32, f32) {
        let (x, y) = self.game.cursor();
        field.to_screen(from_fx(x), from_fx(y))
    }

    /// The displayed replay cursor can differ from the position the core
    /// judges. Before the first real stable frame the sentinel is hidden.
    pub fn display_cursor(&self, field: &Field) -> (f32, f32) {
        match self.playback.as_ref().and_then(|pb| pb.visual.as_ref()) {
            Some(track) => track.position(self.now).map_or((-10_000.0, -10_000.0), |(x, y)| field.to_screen(x, y)),
            None => self.game_cursor(field),
        }
    }

    pub fn hit_error(&self) -> &Running {
        &self.feedback.error_stats
    }

    pub fn draw(&mut self, batch: &mut Batch, skin: &Skin, w: f32, h: f32) {
        // McOsu: the shown health closes 1/4 of the gap upwards and 1/6
        // downwards per 60th of a second.
        let now = std::time::Instant::now();
        let frames = self.hp_moved.map_or(1.0, |t| now.duration_since(t).as_secs_f32() * 60.0);
        self.hp_moved = Some(now);
        let real = self.game.hp_at(self.now).clamp(0.0, 1.0);
        let k = if real > self.hp_shown { 0.25 } else { 1.0 / 6.0 };
        self.hp_shown = (self.hp_shown + (real - self.hp_shown) * (k * frames).min(1.0)).clamp(0.0, 1.0);
        if !self.view_paused && self.skip_target().is_some() {
            let r = Self::skip_rect(skin, w, h);
            match skin.play_skip.as_ref().filter(|s| s.w > 0.0) {
                Some(s) => {
                    let (cx, cy) = r.centre();
                    s.draw(batch, cx, cy, h / 768.0, [255, 255, 255, 255]);
                }
                None => crate::ui::button(batch, r, [70, 70, 80], "Skip", crate::ui::em_for(r.h * 0.5), false),
            }
        }
        let field = Field::new(w, h);
        let chart = &self.chart;
        let objects = &chart.objects;
        let now = self.now;
        let r = from_fx(chart.radius) * field.scale;
        let line = (r * 0.09).max(2.0);
        // Skin images are made for a circle of radius 64.
        let unit = r / 64.0;
        let screen = |p: (Fx, Fx)| field.to_screen(from_fx(p.0), from_fx(p.1));

        // Objects stay relevant until they are over and their feedback has
        // faded. An object that outlasts later ones (a long slider) keeps
        // the window open, which only costs a few skipped iterations.
        const LINGER: Time = 600 * MS;
        while self.draw_from < objects.len() {
            let o = &objects[self.draw_from];
            if o.end + chart.window_50 + LINGER >= now {
                break;
            }
            self.draw_from += 1;
        }
        let mut end = self.draw_from;
        while end < objects.len() && objects[end].t - chart.preempt <= now {
            end += 1;
        }

        self.draw_follow_points(batch, skin, &field, r);
        let hidden = self.mods & rsu_core::mods::HIDDEN != 0;

        // McOsu draws complete objects in descending end-time order. Keep
        // chart indices unchanged: they also identify judgement state.
        let mut draw_order: Vec<_> = (self.draw_from..end).collect();
        draw_order.sort_unstable_by_key(|&i| (std::cmp::Reverse(objects[i].end), std::cmp::Reverse(i)));
        for i in draw_order {
            let o = &objects[i];
            let plain = ((now - (o.t - chart.preempt)) as f32 / chart.fade_in as f32).clamp(0.0, 1.0);
            if o.kind == ObjectKind::Slider {
                let mut body = if hidden { hidden_body_alpha(o, chart.preempt, chart.fade_in, now) } else { plain };
                if !self.game.is_open(i) {
                    // Finished slider bodies linger for 293 ms, except under Hidden.
                    let t = ((now - o.end) as f32 / (293 * MS) as f32).clamp(0.0, 1.0);
                    body = if hidden { 0.0 } else { (1.0 - t) * (1.0 - t) };
                }
                if body > 0.0 {
                    self.draw_slider_body(batch, skin, i, &field, r, body, skin.combo_colour(o.combo_index));
                }
            }
            if !self.game.is_open(i) {
                continue;
            }
            let alpha = if hidden { hidden_alpha(o.t, chart.preempt, now) } else { plain };
            let a = (alpha * 255.0) as u8;
            let c = skin.combo_colour(o.combo_index);
            let (cx, cy) = screen((o.x, o.y));

            match o.kind {
                ObjectKind::Spinner => {
                    self.draw_spinner(batch, skin, i, w, h, &field, line, plain);
                    continue;
                }
                ObjectKind::Slider => {
                    let body = if hidden { hidden_body_alpha(o, chart.preempt, chart.fade_in, now) } else { plain };
                    self.draw_slider(batch, skin, i, &field, r, line, body, plain, c)
                }
                ObjectKind::Circle => {}
            }
            // The clickable part: a circle, or a slider's head while it can
            // still be clicked.
            if o.kind == ObjectKind::Slider && !self.game.head_pending(i) {
                continue;
            }
            let white = [255, 255, 255, a];
            match &skin.hitcircle {
                Some(sprite) => sprite.draw(batch, cx, cy, unit, [c[0], c[1], c[2], a]),
                None => batch.disc(cx, cy, r, [c[0], c[1], c[2], (alpha * 220.0) as u8], 0),
            }
            let overlay = |batch: &mut Batch| match &skin.hitcircle_overlay {
                Some(sprite) => sprite.draw(batch, cx, cy, unit, white),
                // Without a circle image of its own a skin gets our rim.
                None if skin.hitcircle.is_none() => batch.ring(cx, cy, r, line * 1.6, [245, 245, 250, a], 0),
                None => {}
            };
            if !skin.overlay_above_number {
                overlay(batch);
            }
            let mut num = StackStr::<8>::new();
            let _ = write!(num, "{}", o.combo_number);
            match &skin.numbers {
                Some(digits) => {
                    // osu! draws the numbers at four fifths of their size.
                    let u = unit * 0.8;
                    digits.draw(batch, cx - digits.width(num.as_str(), u) * 0.5, cy, u, white, num.as_str());
                }
                None => {
                    let s = r * 0.14;
                    let tw = text::width(s, num.as_str());
                    text::draw(batch, cx - tw * 0.5, cy - 2.5 * s, s, white, num.as_str());
                }
            }
            if skin.overlay_above_number {
                overlay(batch);
            }
            // Hidden leaves the approach circle only on the first object
            // (McOsu's default).
            if now < o.t && (!hidden || i == 0) {
                let k = 1.0 + 3.0 * (o.t - now) as f32 / chart.preempt as f32;
                let colour = [c[0], c[1], c[2], (plain * 230.0) as u8];
                match &skin.approach {
                    Some(sprite) => sprite.draw(batch, cx, cy, unit * k, colour),
                    None => batch.ring(cx, cy, r * k, line, colour, 0),
                }
            }
        }

        // Feedback for recent judgements.
        for e in self.feedback.effects.iter().flatten() {
            let age = now - e.t;
            if !(0..500 * MS).contains(&age) {
                continue;
            }
            let (cx, cy) = field.to_screen(e.x, e.y);
            let fade = ((1.0 - age as f32 / (500 * MS) as f32) * 255.0) as u8;
            let (label, colour, image) = match e.result {
                Some(HitResult::Hit100) => ("100", [110, 230, 120], skin.hit[2]),
                Some(HitResult::Hit50) => ("50", [255, 200, 80], skin.hit[1]),
                Some(HitResult::Miss) => ("X", [255, 70, 70], skin.hit[0]),
                Some(HitResult::Hit300) => ("", [0, 0, 0], skin.hit[3]),
                None => ("", [0, 0, 0], None),
            };
            // Hidden shows no burst of the hit object, only the judgement.
            if !hidden && e.result != Some(HitResult::Miss) && age < 160 * MS {
                let g = age as f32 / (160 * MS) as f32;
                let gone = ((1.0 - g) * 200.0) as u8;
                match skin.hitcircle_overlay.or(skin.hitcircle) {
                    // A skinned circle bursts as itself: it grows and fades.
                    Some(sprite) if e.result.is_some() => {
                        sprite.draw(batch, cx, cy, unit * (1.0 + 0.35 * g), [255, 255, 255, gone]);
                    }
                    Some(_) => {}
                    None => {
                        // A part of a slider gets a smaller ripple than a whole object.
                        let size = if e.result.is_some() { 1.0 } else { 0.6 };
                        batch.ring(cx, cy, r * size * (1.0 + 0.35 * g), line * 1.6, [255, 255, 255, gone], 0);
                    }
                }
            }
            if let Some(sprite) = image {
                // Judgement images pop in slightly larger and settle.
                let pop = 1.0 + 0.2 * (1.0 - age as f32 / (100 * MS) as f32).max(0.0);
                sprite.draw(batch, cx, cy, unit * pop, [255, 255, 255, fade]);
            } else if !label.is_empty() {
                let s = r * 0.16;
                let tw = text::width(s, label);
                text::draw(batch, cx - tw * 0.5, cy - 2.5 * s, s, [colour[0], colour[1], colour[2], fade], label);
            }
        }

        self.draw_hud(batch, skin, w, h, &field);
    }

    /// The dots that lead from each object to the next one of the same
    /// combo, as McOsu's `drawFollowPoints` does them: a dot every 32 px of
    /// the 640x480 frame from 1.5 steps after the start to one step before
    /// the end, each fading in linearly over 800 ms ending at the moment the
    /// ball would pass it, and out over 400 ms after. The image is scaled to
    /// 16 px of it per eighth of the circle's diameter; frames of an animated one run at
    /// 1/n s each from the dot's fade-in.
    fn draw_follow_points(&self, batch: &mut Batch, skin: &Skin, field: &Field, r: f32) {
        const APPROACH: Time = 800 * MS;
        const PREV_FADE: Time = 400 * MS;
        let frames = &skin.follow_point;
        if frames.is_empty() {
            return;
        }
        let (chart, now) = (&self.chart, self.now);
        let objects = &chart.objects;
        // First pair whose later object has not been passed longer than the
        // fade-out ago.
        let first = objects.partition_point(|o| o.t + PREV_FADE < now).max(1) - 1;
        let step = ((32.0 * field.scale) as i32).max(1);
        let frame_len = (1_000_000 / frames.len() as Time).max(1);
        for i in first..objects.len().saturating_sub(1) {
            let (a, b) = (&objects[i], &objects[i + 1]);
            if a.kind == ObjectKind::Spinner || b.kind == ObjectKind::Spinner || b.combo_number == 1 {
                if b.t >= now + APPROACH {
                    break;
                }
                continue;
            }
            let start_t = a.end + MS;
            let time_diff = b.t - start_t;
            let (ax, ay) = match a.kind {
                ObjectKind::Slider => chart.slider_end_position(a),
                _ => (a.x, a.y),
            };
            let (sx, sy) = field.to_screen(from_fx(ax), from_fx(ay));
            let (ex, ey) = field.to_screen(from_fx(b.x), from_fx(b.y));
            let (dx, dy) = (ex - sx, ey - sy);
            // Rounded to two places against flicker, as McOsu does.
            let dist = (dx.hypot(dy) * 100.0).round() / 100.0;
            let angle = dy.atan2(dx);
            let mut j = (step as f32 * 1.5) as i32;
            while (j as f32) < dist - step as f32 {
                let ratio = j as f32 / dist;
                j += step;
                let fade_out = start_t + (ratio * time_diff as f32) as Time;
                let fade_in = fade_out - APPROACH;
                let alpha = if now >= fade_in && now < fade_out {
                    (now - fade_in) as f32 / APPROACH as f32
                } else if now >= fade_out && now < fade_out + PREV_FADE {
                    1.0 - (now - fade_out) as f32 / PREV_FADE as f32
                } else {
                    continue;
                };
                let n = frames.len();
                let frame = (((now - fade_in).max(0) / frame_len) as usize) % n;
                let sprite = &frames[frame];
                if sprite.w <= 0.0 {
                    continue;
                }
                // McOsu scales it by an eighth of the circle's diameter per
                // 16 px of the image, which is the usual `r / 64`.
                let unit = r / 64.0;
                sprite.draw_turned(batch, sx + dx * ratio, sy + dy * ratio, unit, [255, 255, 255, (alpha * 255.0) as u8], angle, 0);
            }
            if b.t >= now + APPROACH {
                break;
            }
        }
    }

    /// The body of slider `i`, drawn together with the rest of the object.
    fn draw_slider_body(&self, batch: &mut Batch, skin: &Skin, i: usize, field: &Field, r: f32, alpha: f32, c: [u8; 3]) {
        let chart = &self.chart;
        let s = chart.slider(&chart.objects[i]);
        let screen = |p: (Fx, Fx)| field.to_screen(from_fx(p.0), from_fx(p.1));
        let fade = (alpha * 255.0) as u8;
        // The track is the combo colour, unless the skin fixes it.
        let track = skin.slider_track.unwrap_or(c);
        let path = chart.slider_path(s);
        let progress = slider_snake_progress(chart.objects[i].t, chart.preempt, self.now);
        let distance = (s.length as f64 * progress as f64) as i64;
        let tip = screen(chart.position_at(s, distance));
        let id = batch.begin_body();
        // Both passes must use exactly the same clipped geometry.
        for depth_only in [true, false] {
            let mut from = screen((path[0].x, path[0].y));
            for p in path.iter().skip(1).take_while(|p| p.d < distance) {
                let to = screen((p.x, p.y));
                if (to.0 - from.0).hypot(to.1 - from.1) >= 2.0 {
                    batch.body(id, from, to, r, skin.slider_border, track, fade, depth_only);
                    from = to;
                }
            }
            batch.body(id, from, tip, r, skin.slider_border, track, fade, depth_only);
        }
    }

    /// Ticks, repeat markers and the ball of slider `i` (the body is `draw_slider_body`).
    #[allow(clippy::too_many_arguments)]
    fn draw_slider(
        &self,
        batch: &mut Batch,
        skin: &Skin,
        i: usize,
        field: &Field,
        r: f32,
        line: f32,
        alpha: f32,
        _arrows: f32,
        c: [u8; 3],
    ) {
        let chart = &self.chart;
        let o = &chart.objects[i];
        let s = chart.slider(o);
        let now = self.now;
        let screen = |p: (Fx, Fx)| field.to_screen(from_fx(p.0), from_fx(p.1));
        let unit = r / 64.0;
        let path = chart.slider_path(s);
        // Ticks fade with the body; the reverse arrows keep the plain
        // fade-in, also under hidden (as McOsu).
        let a = (alpha * 255.0) as u8;
        let snake = slider_snake_progress(o.t, chart.preempt, now);
        let arrow_start = o.t - chart.preempt + chart.preempt / 3;
        let arrow_a = ((((now - arrow_start) as f32 / (150 * MS) as f32).clamp(0.0, 1.0)) * 255.0) as u8;
        let done = self.game.nested_done(i);
        let nested = chart.slider_nested(s);
        // Only what belongs to the pass being played (or about to be).
        let span = ((now - o.t).max(0) / s.span_duration).min(s.spans as Time - 1);
        let span_end = o.t + (span + 1) * s.span_duration;
        // As in McOsu a tick is one image at its place on the path, however
        // many passes cross it, and stays until its last pass is over. Its
        // place along a pass, measured from the head, is the key.
        let mut seen: Vec<Time> = Vec::new();
        for n in &nested[done.min(nested.len())..] {
            let (x, y) = screen((n.x, n.y));
            match n.kind {
                NestedKind::Tick => {
                    let off = (n.t - o.t) % s.span_duration;
                    let along = if ((n.t - o.t) / s.span_duration) % 2 == 0 { off } else { s.span_duration - off };
                    if along as f32 / s.span_duration as f32 > snake {
                        continue;
                    }
                    if seen.iter().any(|&t| (t - along).abs() <= MS) {
                        continue;
                    }
                    seen.push(along);
                    match &skin.score_point {
                        Some(sprite) => sprite.draw(batch, x, y, unit, [255, 255, 255, a]),
                        None => batch.disc(x, y, r * 0.13, [245, 245, 250, a], 0),
                    }
                }
                NestedKind::Repeat if arrow_a > 0 && n.t <= span_end + s.span_duration
                    && (now > o.t || n.t <= span_end) => match &skin.reverse_arrow {
                    Some(sprite) => {
                        // The arrow image points right; turn it back along
                        // the path from whichever end it sits on.
                        let at_start = (n.x, n.y) == (path[0].x, path[0].y);
                        let (here, towards) = match (at_start, path.len()) {
                            (_, 1) => (path[0], path[0]),
                            (true, _) => (path[0], path[1]),
                            (false, len) => (path[len - 1], path[len - 2]),
                        };
                        let angle = ((towards.y - here.y) as f32).atan2((towards.x - here.x) as f32);
                        sprite.draw_turned(batch, x, y, unit, [255, 255, 255, arrow_a], angle, 0);
                    }
                    None => {
                        batch.ring(x, y, r * 0.5, line * 1.6, [245, 245, 250, arrow_a], 0);
                        batch.disc(x, y, r * 0.2, [245, 245, 250, arrow_a], 0);
                    }
                },
                _ => {}
            }
        }

        if now >= o.t {
            let (bx, by) = screen(chart.ball_at(o, now));
            let tracking = self.game.is_tracking(i);
            match &skin.slider_ball {
                Some(sprite) => sprite.draw(batch, bx, by, unit, [255, 255, 255, 255]),
                None => {
                    batch.disc(bx, by, r * 0.92, [c[0], c[1], c[2], 235], 0);
                    batch.ring(bx, by, r * 0.92, line * 1.6, [255, 255, 255, 255], 0);
                }
            }
            if tracking {
                // McOsu: fades in over 60 ms and grows from 0.5 over 180 ms
                // (quad out), then pulses by 10 % for 200 ms at every tick,
                // repeat and the end.
                let since = (now - o.t).max(0) as f32 / 1e6;
                let dur = (o.end - o.t) as f32 / 1e6;
                let fade = (since / 0.06f32.clamp(0.0, dur)).clamp(0.0, 1.0);
                let grow = (since / 0.18f32.clamp(0.0, dur)).clamp(0.0, 1.0);
                let grow = 0.5 + 0.5 * (-grow * (grow - 2.0));
                let pulse = nested[..done.min(nested.len())]
                    .iter()
                    .rev()
                    .find(|n| n.t <= now)
                    .map(|n| (now - n.t) as f32 / 0.2e6)
                    .filter(|p| *p < 1.0)
                    .map_or(0.0, |p| if p < 0.1 { 1.0 } else { (1.0 - p) / 0.9 });
                let k = grow * (1.0 + 0.1 * pulse);
                let fa = (255.0 * fade) as u8;
                match &skin.follow_circle {
                    Some(sprite) if sprite.w > 0.0 => {
                        // 2.4 diameters times 0.85 per 259 px of the image.
                        let unit = (r * 2.0 * 2.4 * 0.85 / 259.0) * k;
                        sprite.draw(batch, bx, by, unit, [255, 255, 255, fa]);
                    }
                    _ => {
                        let follow = from_fx(chart.follow_radius) * field.scale * k;
                        batch.ring(bx, by, follow, line * 1.2, [255, 220, 120, (200.0 * fade) as u8], 0);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_spinner(
        &self,
        batch: &mut Batch,
        skin: &Skin,
        i: usize,
        w: f32,
        h: f32,
        field: &Field,
        line: f32,
        alpha: f32,
    ) {
        let o = &self.chart.objects[i];
        let now = self.now;
        let (cx, cy) = (w * 0.5, field.to_screen(256.0, 192.0).1);
        let radius = 150.0 * field.scale;
        let active = now >= o.t;
        let a = (alpha * if active { 255.0 } else { 110.0 }) as u8;
        let turns = self.game.spinner_turns(i);
        let angle = turns * std::f32::consts::TAU;
        let left = if o.end > o.t { (o.end - now).max(0) as f32 / (o.end - o.t) as f32 } else { 0.0 };
        // Spinner images are sized for a screen 768 pixels tall.
        let unit = h / 768.0;
        if skin.spinner.is_empty() {
            batch.ring(cx, cy, radius, line * 2.0, [200, 205, 225, a], 0);
            batch.disc(cx, cy, line * 2.0, [200, 205, 225, a], 0);
            // A marker that turns with the player.
            batch.disc(
                cx + radius * 0.82 * angle.cos(),
                cy + radius * 0.82 * angle.sin(),
                line * 3.0,
                [255, 220, 120, a],
                0,
            );
        }
        for (sprite, follow) in &skin.spinner {
            sprite.draw_turned(batch, cx, cy, unit, [255, 255, 255, a], angle * follow, 0);
        }
        // Time left: a ring closing in on the centre (hidden hides it).
        if active && o.end > o.t && self.mods & rsu_core::mods::HIDDEN == 0 {
            match &skin.spinner_approach {
                Some(sprite) => sprite.draw(batch, cx, cy, unit * (0.1 + 1.8 * left), [255, 255, 255, a]),
                None => batch.ring(cx, cy, radius * left.max(0.03), line, [120, 190, 255, 200], 0),
            }
        }

        let needed = o.extra as f32 / 2.0;
        let cleared = turns >= needed;
        let mut text = StackStr::<32>::new();
        if cleared {
            let _ = write!(text, "CLEAR");
        } else {
            let _ = write!(text, "{:.1} / {:.1}", turns, needed);
        }
        let s = (field.scale * 3.0).max(2.0);
        let tw = text::width(s, text.as_str());
        let colour = if cleared { [120, 230, 150, a] } else { [240, 240, 245, a] };
        text::draw(batch, cx - tw * 0.5, cy + radius * 0.35, s, colour, text.as_str());
    }

    /// McOsu's `drawScorebarBg` and `drawHPBar`. Without the skin's images a
    /// plain bar is drawn.
    fn draw_health_bar(&self, batch: &mut Batch, skin: &Skin, w: f32, h: f32) {
        let hp = self.hp_shown;
        let bar = &skin.scorebar;
        let Some(colour) = bar.colour.filter(|s| s.visible()) else {
            let (bx, by, bw, bh) = (16.0, 14.0, w * 0.3, (h / 60.0).max(6.0));
            batch.rect(bx - 2.0, by - 2.0, bw + 4.0, bh + 4.0, [0, 0, 0, 140]);
            let fill = if hp < 0.25 { [235, 70, 70, 255] } else { [240, 240, 245, 255] };
            batch.rect(bx, by, bw * hp, bh, fill);
            return;
        };
        // One osu! pixel of the 640x480 layout, and the size the bar's
        // images are drawn at (0.625 of a pixel per image pixel).
        let m = (w / 640.0).min(h / 480.0);
        let unit = 0.625 * m;
        if let Some(bg) = bar.bg.filter(|s| s.visible()) {
            bg.draw_part(batch, 0.0, 0.0, unit, [255; 4], 1.0);
        }
        // McOsu counts a file as present even when it is empty.
        let marker = bar.marker;
        let (ox, oy) = if marker.is_some() { (7.5 * m, 7.8 * m) } else { (3.0 * m, 10.0 * m) };
        // The newer bar tints itself: grey, then dark red, as health sinks.
        let tint = match marker {
            Some(_) if hp < 0.2 => [(255.0 * (0.2 - hp) / 0.2) as u8, 0, 0, 255],
            Some(_) if hp < 0.5 => {
                let v = (255.0 * (1.0 - (0.5 - hp) / 0.5)) as u8;
                [v, v, v, 255]
            }
            _ => [255; 4],
        };
        colour.draw_part(batch, ox, oy, unit, tint, hp);
        let x = ox + hp * colour.w * unit;
        let ki = match marker {
            Some(_) if hp < 0.2 => None,
            Some(marker) => Some(marker),
            None => bar.ki[if hp < 0.2 { 2 } else if hp < 0.5 { 1 } else { 0 }].filter(|s| s.visible()),
        };
        if let Some(ki) = ki {
            let y = if marker.is_some() { (8.125 + 2.5) * m } else { 10.0 * m };
            let scale = if marker.is_some() { 0.625 } else { 72.0 / 116.0 } * m * 0.8;
            ki.draw(batch, x, y, scale, tint);
        }
    }

    fn draw_hud(&self, batch: &mut Batch, skin: &Skin, w: f32, h: f32, field: &Field) {
        let s = (h / 270.0).max(2.0).round();
        let white = [240, 240, 245, 255];
        let mut line = StackStr::<64>::new();
        let summary = self.game.summary();
        // The skin's score font where it has one; its images are sized for
        // a screen 768 pixels tall. `k` is the size relative to the score.
        let unit = h / 768.0;
        let right = |batch: &mut Batch, y: f32, k: f32, text: &str| match &skin.score {
            Some(digits) => {
                let u = unit * k;
                digits.draw(batch, w - 16.0 - digits.width(text, u), y + digits.height(u) * 0.5, u, [255; 4], text);
                y + digits.height(u)
            }
            None => {
                text::draw(batch, w - 16.0 - text::width(s * k, text), y, s * k, white, text);
                y + 7.0 * s * k
            }
        };
        self.draw_health_bar(batch, skin, w, h);
        let _ = write!(line, "{:08}", summary.score);
        let below = right(batch, 14.0, 1.0, line.as_str());
        line.clear();
        let _ = write!(line, "{:.2}%", summary.accuracy() * 100.0);
        right(batch, below, 0.75, line.as_str());
        let (accuracy_x, accuracy_h) = match &skin.score {
            Some(digits) => (w - 16.0 - digits.width(line.as_str(), unit * 0.75), digits.height(unit * 0.75)),
            None => (w - 16.0 - text::width(s * 0.75, line.as_str()), 7.0 * s * 0.75),
        };
        let radius = 10.5 * field.scale;
        let (x, y) = (accuracy_x - radius - 20.0, (below + accuracy_h * 0.5).max(below + radius));
        let end = self.chart.objects.last().map_or(0, |o| o.end.max(o.t));
        batch.sector(x, y, radius, map_progress(self.now, end), [242, 242, 242, 170]);
        match skin.circular_metre {
            Some(sprite) if sprite.w > 0.0 => sprite.draw(batch, x, y, radius * 2.0 / sprite.w * 1.3, [255; 4]),
            None => batch.ring(x, y, radius * 1.15, (field.scale * 1.2).max(1.0), [242, 242, 242, 200], 0),
            _ => {}
        }
        line.clear();
        let _ = write!(line, "{}X", self.game.combo());
        // The combo sits in the bottom left corner; what else is shown
        // there goes above it, however tall the skin makes it.
        let (combo_end, combo_h) = match &skin.score {
            Some(digits) => {
                let u = unit * 1.3;
                digits.draw(batch, 16.0, h - 16.0 - digits.height(u) * 0.5, u, [255; 4], line.as_str());
                (16.0 + digits.width(line.as_str(), u), digits.height(u))
            }
            None => (text::draw(batch, 16.0, h - 16.0 - 5.0 * s * 1.5, s * 1.5, white, line.as_str()), 5.0 * s * 1.5),
        };
        if self.mods != 0 && self.playback.is_none() {
            let mut name = StackStr::<24>::new();
            let _ = write!(name, "{}", rsu_core::mods::name(self.mods));
            text::draw(batch, combo_end + 6.0 * s, h - 16.0 - 5.0 * s * 1.5, s * 1.5, white, name.as_str());
        }
        if self.practice.is_active() {
            let mut practice = StackStr::<96>::new();
            let _ = write!(practice, "PRACTICE - NOT SAVED");
            for (name, value) in [("CS", self.practice.cs), ("AR", self.practice.ar), ("OD", self.practice.od), ("HP", self.practice.hp)] {
                if let Some(value) = value {
                    let _ = write!(practice, " {name} {value:.1}");
                }
            }
            if self.practice.no_spinners {
                let _ = write!(practice, " NO SPINNERS");
            }
            if self.practice.fading_cursor {
                let _ = write!(practice, " FADING CURSOR");
            }
            if self.practice.flip_left_right {
                let _ = write!(practice, " FLIP LEFT/RIGHT");
            }
            if self.practice.flip_up_down {
                let _ = write!(practice, " FLIP UP/DOWN");
            }
            if self.practice.ar_lock {
                let _ = write!(practice, " AR LOCK");
            }
            if self.practice.od_lock {
                let _ = write!(practice, " OD LOCK");
            }
            text::draw(batch, 16.0, h - 46.0 - combo_h, s * 0.75, [255, 200, 80, 255], practice.as_str());
        }
        if self.playback.is_some() {
            let clock = |t: Time| (t.max(0) / 60_000_000, t.max(0) / 1_000_000 % 60);
            let ((m, sec), (em, es)) = (clock(self.now), clock(self.end_time()));
            line.clear();
            let _ = write!(line, "REPLAY  {m}:{sec:02} / {em}:{es:02}  {:.2}X", self.view_speed);
            if self.mods != 0 {
                let _ = write!(line, "  {}", rsu_core::mods::name(self.mods));
            }
            if self.view_paused {
                let _ = write!(line, "  PAUSED");
            }
            let y = h - 30.0 - combo_h - 5.0 * s * 0.75;
            text::draw(batch, 16.0, y, s * 0.75, [255, 200, 80, 255], line.as_str());
            if self.playback.as_ref().is_some_and(|pb| pb.visual.is_some()) {
                text::draw(batch, 16.0, y - 15.0 * s * 0.6, s * 0.6, [150, 150, 160, 255],
                    "STABLE: CURSOR SMOOTHING IS VISUAL ONLY; JUDGEMENT USES RECORDED INPUT");
            }
            text::draw(
                batch,
                16.0,
                y - 7.0 * s * 0.6,
                s * 0.6,
                [150, 150, 160, 255],
                "LEFT/RIGHT: SEEK 5 S   UP/DOWN: SPEED   SPACE: PAUSE   HOME: RESTART",
            );
        }

        // The offset, for a moment after it was changed.
        if self.offset_changed.is_some_and(|t| (self.now - t).abs() < 2_500 * MS) {
            line.clear();
            let _ = write!(line, "OFFSET {:+} MS", self.offset / MS);
            let tw = text::width(s, line.as_str());
            text::draw(batch, (w - tw) * 0.5, h - 60.0 - 5.0 * s, s, [255, 200, 80, 255], line.as_str());
        }


        // Hit error bar: the three windows as zones, recent hits as ticks.
        let (cx, y) = (w * 0.5, h - 22.0);
        let px_per_us = field.scale * 384.0 * 0.45 / self.chart.window_50 as f32 * 0.5;
        for (window, colour) in [
            (self.chart.window_50, [200, 160, 60, 110]),
            (self.chart.window_100, [90, 200, 110, 130]),
            (self.chart.window_300, [90, 170, 255, 160]),
        ] {
            let half = window as f32 * px_per_us;
            batch.rect(cx - half, y - 2.0, half * 2.0, 4.0, colour);
        }
        batch.rect(cx - 1.0, y - 9.0, 2.0, 18.0, white);
        let n = self.feedback.errors_len;
        for (i, e) in self.feedback.errors[..n].iter().enumerate() {
            let a = (50 + 205 * (i + 1) / n) as u8;
            batch.rect(cx + e * 1000.0 * px_per_us - 1.0, y - 7.0, 2.0, 14.0, [255, 255, 255, a]);
        }
    }
}

fn map_progress(now: Time, end: Time) -> f32 {
    (now as f32 / end.max(1) as f32).clamp(0.0, 1.0)
}

// McOsu snakes linearly over the first third of the approach time.
fn slider_snake_progress(start: Time, preempt: Time, now: Time) -> f32 {
    (((now - (start - preempt)) as f64 * 3.0 / preempt.max(1) as f64) as f32).clamp(0.0, 1.0)
}

/// A circle (or slider head) under hidden, as McOsu's defaults have it:
/// in from 100 % to 60 % of the approach time before the object, out from
/// 60 % to 30 %.
fn hidden_alpha(t: Time, preempt: Time, now: Time) -> f32 {
    let at = |part: f32| t - (preempt as f32 * part) as Time;
    let ramp = |from: Time, to: Time| ((now - from) as f32 / (to - from).max(1) as f32).clamp(0.0, 1.0);
    if now < at(0.6) {
        ramp(at(1.0), at(0.6))
    } else {
        1.0 - ramp(at(0.6), at(0.3))
    }
}

/// A slider body under hidden: the plain fade-in, then out (quadratic)
/// from the end of the fade-in to the slider's end.
fn hidden_body_alpha(o: &rsu_core::HitObject, preempt: Time, fade_in: Time, now: Time) -> f32 {
    let start = o.t.min(o.t - preempt + fade_in);
    if now < start {
        return ((now - (o.t - preempt)) as f32 / fade_in.max(1) as f32).clamp(0.0, 1.0);
    }
    let left = ((o.end - now) as f32 / (o.end - start).max(1) as f32).clamp(0.0, 1.0);
    left * left
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsu_core::auto::autoplay;
    use rsu_core::chart::{ChartBuilder, Difficulty};

    #[test]
    fn snaking_body_reaches_half_distance_not_half_vertices() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        let p = |x, y| (to_fx(x), to_fx(y));
        b.slider(2000 * MS, &[p(100.0, 100.0), p(120.0, 100.0), p(200.0, 100.0)], 1, 300 * MS, &[]);
        let mut play = Play::new(Arc::new(b.build()), String::new(), [0; 16], 1_500_000, 0, 0, MapSounds::default(), None);
        play.now = 2000 * MS - play.chart.preempt + play.chart.preempt / 6;
        let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 128];
        let mut batch = unsafe { Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        let field = Field::new(640.0, 480.0);
        play.draw_slider_body(&mut batch, &Skin::default(), 0, &field, 20.0, 1.0, [255; 3]);
        let len = batch.len();
        let instances = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), len) };
        let endpoint = field.to_screen(150.0, 100.0).0;
        let last = instances.iter().filter(|v| v.uv[3] == 0).last().unwrap();
        let actual = last.center[0] + last.half[0] - 20.0;
        assert!((actual - endpoint).abs() < 0.1, "endpoint {actual}, expected {endpoint}");
    }

    #[test]
    fn snaking_ticks_and_repeat_arrows_wait_for_the_body() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        let p = |x, y| (to_fx(x), to_fx(y));
        b.slider(2000 * MS, &[p(100.0, 100.0), p(200.0, 100.0)], 3, 400 * MS,
            &[(2300 * MS, NestedKind::Tick), (2400 * MS, NestedKind::Repeat), (2800 * MS, NestedKind::Repeat)]);
        let mut play = Play::new(Arc::new(b.build()), String::new(), [0; 16], 1_500_000, 0, 0, MapSounds::default(), None);
        let field = Field::new(640.0, 480.0);
        let start = 2000 * MS - play.chart.preempt;
        for (now, tick, arrow_alpha) in [
            (start + play.chart.preempt / 6, false, 0),
            (start + play.chart.preempt / 3 + 75 * MS, true, 127),
            (start + play.chart.preempt / 3 + 150 * MS, true, 255),
        ] {
            play.now = now;
            let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 128];
            let mut batch = unsafe { Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
            play.draw_slider(&mut batch, &Skin::default(), 0, &field, 20.0, 2.0, 1.0, 1.0, [255; 3]);
            let items = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
            let tick_pos = field.to_screen(175.0, 100.0);
            assert_eq!(items.iter().any(|v| v.kind == rsu_gfx::KIND_DISC && v.center == [tick_pos.0, tick_pos.1]), tick);
            let arrow = items.iter().find(|v| v.kind == rsu_gfx::KIND_RING);
            assert_eq!(arrow.map_or(0, |v| v.color[3]), arrow_alpha);
        }
    }

    #[test]
    fn snaking_degenerate_path_draws_finite_head_in_both_passes() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.slider(2000 * MS, &[(to_fx(100.0), to_fx(100.0)); 2], 2, 400 * MS, &[(2400 * MS, NestedKind::Repeat)]);
        let mut play = Play::new(Arc::new(b.build()), String::new(), [0; 16], 1_500_000, 0, 0, MapSounds::default(), None);
        let field = Field::new(640.0, 480.0);
        for now in [0, 2000 * MS - play.chart.preempt, 2000 * MS, 2500 * MS] {
            play.now = now;
            let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 32];
            let mut batch = unsafe { Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
            play.draw_slider_body(&mut batch, &Skin::default(), 0, &field, 20.0, 1.0, [255; 3]);
            let items = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
            assert_eq!(items.len(), 2);
            assert!(items.iter().all(|v| v.center.iter().chain(v.half.iter()).all(|x| x.is_finite())));
            assert_eq!(items[0].center, items[1].center);
            assert_eq!(items[0].half, items[1].half);
        }
    }

    #[test]
    fn earlier_slider_body_covers_the_following_circle() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        let p = |x, y| (to_fx(x), to_fx(y));
        b.slider(2000 * MS, &[p(100.0, 100.0), p(200.0, 100.0)], 1, 300 * MS, &[]);
        b.circle(2400 * MS, to_fx(200.0), to_fx(100.0));
        let mut play = Play::new(Arc::new(b.build()), String::new(), [0; 16], 1_500_000, 0, 0, MapSounds::default(), None);
        play.now = 1900 * MS;
        let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 16384];
        let mut batch = unsafe { Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        play.draw(&mut batch, &Skin::default(), 640.0, 480.0);
        let len = batch.len();
        let instances = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), len) };
        let (x, y) = Field::new(640.0, 480.0).to_screen(200.0, 100.0);
        let radius = from_fx(play.chart.radius);
        let circle = instances.iter().position(|v| v.kind == rsu_gfx::KIND_DISC && v.center == [x, y] && v.half == [radius, radius]).unwrap();
        let body = instances.iter().rposition(|v| v.kind & 255 == rsu_gfx::KIND_BODY).unwrap();
        assert!(circle < body, "the earlier slider body must be drawn over the later circle");
    }

    #[test]
    fn progress_counts_from_music_start_and_clamps() {
        assert_eq!(map_progress(-100, 1000), 0.0);
        assert_eq!(map_progress(250, 1000), 0.25);
        assert_eq!(map_progress(1500, 1000), 1.0);
        assert_eq!(map_progress(0, 0), 0.0);
    }

    /// Circles, sliders with ticks and a repeat, and a spinner.
    fn chart() -> Arc<Chart> {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        let p = |x: f32, y: f32| (to_fx(x), to_fx(y));
        for i in 0..30i64 {
            let t = (2000 + i * 400) * MS;
            let (x, y) = (60.0 + (i * 67 % 400) as f32, 50.0 + (i * 41 % 280) as f32);
            if i % 3 == 1 {
                b.slider(
                    t,
                    &[p(x, y), p(x + 60.0, y), p(x + 60.0, y + 40.0)],
                    2,
                    150 * MS,
                    &[
                        (t + 75 * MS, NestedKind::Tick),
                        (t + 150 * MS, NestedKind::Repeat),
                        (t + 264 * MS, NestedKind::Tail),
                    ],
                );
            } else {
                b.circle(t, to_fx(x), to_fx(y));
            }
        }
        b.spinner(14_200 * MS, 15_500 * MS);
        Arc::new(b.build())
    }

    #[test]
    fn the_skip_is_offered_only_in_long_empty_stretches() {
        // First object at 2 s; then a gap from 14.2 s (spinner end 15.5 s).
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.circle(10_000 * MS, to_fx(100.0), to_fx(100.0));
        b.circle(11_000 * MS, to_fx(100.0), to_fx(100.0));
        b.circle(30_000 * MS, to_fx(100.0), to_fx(100.0));
        let mut p = Play::new(Arc::new(b.build()), String::new(), [0; 16], 1_500_000, 0, 0, MapSounds::default(), None);
        let at = |p: &mut Play, ms: i64| {
            p.now = ms * MS;
            p.skip_target()
        };
        assert_eq!(at(&mut p, 0), Some(7_500 * MS), "intro: 2.5 s before the first object");
        assert_eq!(at(&mut p, 5_000), None, "less than 5 s to go");
        assert_eq!(at(&mut p, 11_500), None, "a second must pass after the last object");
        assert_eq!(at(&mut p, 12_100), Some(27_500 * MS));
        assert_eq!(at(&mut p, 26_000), None);
        assert_eq!(at(&mut p, 31_000), None, "nothing is skipped at the end");
    }

    /// Distinct sample ids per object, so a test can tell which sound played:
    /// edge k of object i is id i * 10 + k, its tick is i * 10 + 9.
    fn sounds(chart: &Chart) -> MapSounds {
        let one = |id: usize| SoundIds::one(id as u16, 100);
        let objects = chart
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let (edges, ticks) = match o.kind {
                    ObjectKind::Slider => {
                        let s = chart.slider(o);
                        let ticks = chart.slider_nested(s).iter().filter(|n| n.kind == NestedKind::Tick);
                        (s.spans as usize + 1, ticks.map(|n| (n.t, one(i * 10 + 9))).collect())
                    }
                    _ => (1, Vec::new()),
                };
                ObjectCues { edges: (0..edges).map(|k| one(i * 10 + k)).collect(), ticks }
            })
            .collect();
        // Looping and extra sounds get ids no object uses.
        MapSounds {
            objects,
            slide: [Some(901), Some(902), Some(903)],
            spin: Some(904),
            bonus: Some(905),
            combo_break: Some(906),
            sections: Sections { points: vec![(0, 1, 100), (5_000_000, 2, 60)] },
        }
    }

    /// A clock that maps CLOCK_MONOTONIC straight onto the stream.
    fn clock() -> ClockModel {
        ClockModel { t0_ns: 0, pos0: 0.0, rate: SAMPLE_RATE as f64 / 1e9, valid: true, ..ClockModel::INVALID }
    }

    fn feedback() -> Feedback {
        Feedback {
            effects: [None; 64],
            next_effect: 0,
            errors: [0.0; 32],
            errors_len: 0,
            error_stats: Running::default(),
            cues: [Cue::default(); 32],
            cues_len: 0,
            planned: [(u32::MAX, 0, false); 64],
            planned_next: 0,
        }
    }

    #[test]
    fn practice_hud_shows_not_saved_before_long_modifier_list() {
        for (w, h) in [(800.0, 600.0), (1920.0, 1080.0)] {
            let mut play = Play::new(chart(), String::new(), [0; 16], 0, 0, 0, MapSounds::default(), None);
            play.practice = PracticeDifficulty { cs: Some(12.5), ar: Some(12.5), od: Some(12.5), hp: Some(12.5), ar_lock: true, od_lock: true, flip_left_right: true, flip_up_down: true, fading_cursor: true, no_spinners: true, no_50s: false };
            let mut actual = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 8192];
            let mut batch = unsafe { Batch::over(actual.as_mut_ptr().cast(), actual.len()) };
            play.draw_hud(&mut batch, &Skin::default(), w, h, &Field::new(w, h));
            let actual = unsafe { std::slice::from_raw_parts(actual.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
            let yellow: Vec<_> = actual.iter().filter(|v| v.color == [255, 200, 80, 255]).collect();
            let mut expected = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 2048];
            let mut reference = unsafe { Batch::over(expected.as_mut_ptr().cast(), expected.len()) };
            let s = (h / 270.0_f32).max(2.0).round();
            text::draw(&mut reference, 16.0, yellow[0].center[1] - yellow[0].half[1], s * 0.75, [255, 200, 80, 255], "PRACTICE - NOT SAVED");
            let expected = unsafe { std::slice::from_raw_parts(expected.as_ptr().cast::<rsu_gfx::Inst>(), reference.len()) };
            assert!(yellow.len() >= expected.len());
            for (actual, expected) in yellow.iter().zip(expected) {
                assert_eq!(actual.center[0], expected.center[0], "warning must precede potentially truncated details");
                assert_eq!(actual.center[1], expected.center[1]);
            }
        }
    }

    #[test]
    fn spun_out_is_frame_independent_and_does_not_hit_circles() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.spinner(1_000_000, 3_000_000);
        b.circle(4_000_000, to_fx(100.0), to_fx(100.0));
        let c = Arc::new(b.build());
        let make = || Play::new(c.clone(), String::new(), [0; 16], 0, 0, 4096, MapSounds::default(), None);
        let mut a = make();
        let mut b = make();
        let mut ae = Vec::new();
        let mut be = Vec::new();
        a.game.advance_to(4_500_000, &mut |e| ae.push(e));
        for t in (0..=4500).step_by(17) { b.game.advance_to(t * MS, &mut |e| be.push(e)); }
        b.game.advance_to(4_500_000, &mut |e| be.push(e));
        assert_eq!(a.game.summary(), b.game.summary());
        assert_eq!(ae, be);
        assert_eq!(a.game.result(0), Some(HitResult::Hit300));
        assert_eq!(a.game.result(1), Some(HitResult::Miss));
    }

    #[test]
    fn spun_out_spins_without_input_only_during_live_play() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.spinner(1_000_000, 3_000_000);
        let c = Arc::new(b.build());
        let mut live = Play::new(c.clone(), String::new(), [0; 16], 0, 0, 4096, MapSounds::default(), None);
        live.game.advance_to(3_100_000, &mut |_| {});
        assert_eq!(live.game.result(0), Some(HitResult::Hit300));
        let pb = Playback::new(&c, 4096, vec![], 3_100_000, None);
        let mut replay = Play::new(c, String::new(), [0; 16], 0, 0, 4096, MapSounds::default(), Some(pb));
        replay.game.advance_to(3_100_000, &mut |_| {});
        assert_eq!(replay.game.result(0), Some(HitResult::Miss));
    }

    #[test]
    fn sudden_death_failure_survives_another_update() {
        let mut p = Play::new(chart(), String::new(), [0; 16], 0, 0, 32, MapSounds::default(), None);
        p.update(&clock(), 2_200_000_000);
        assert!(p.failed);
        p.update(&clock(), 2_200_000_000);
        assert!(p.failed, "a paused frame must not erase a failure");
    }

    #[test]
    fn sudden_death_stops_before_later_object_in_same_frame() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.circle(1_000_000, to_fx(100.0), to_fx(100.0));
        b.circle(2_000_000, to_fx(100.0), to_fx(100.0));
        let mut p = Play::new(Arc::new(b.build()), String::new(), [0; 16], 0, 0, 32, MapSounds::default(), None);
        p.update(&clock(), 3_000_000_000);
        assert!(p.failed);
        assert_eq!(p.game.result(1), None, "later objects must not be judged after failure");
    }

    #[test]
    fn sudden_death_does_not_apply_inputs_after_timed_miss() {
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        b.circle(1_000_000, to_fx(100.0), to_fx(100.0));
        b.circle(2_000_000, to_fx(100.0), to_fx(100.0));
        let mut p = Play::new(Arc::new(b.build()), String::new(), [0; 16], 0, 0, 32, MapSounds::default(), None);
        p.pending.push(Input { t: 2_000_000, kind: InputKind::Press(Button::M1) });
        p.update(&clock(), 3_000_000_000);
        assert!(p.failed);
        assert_eq!(p.game.result(1), None);
        assert!(p.recorded.is_empty());
        let summary = p.game.summary();
        p.update(&clock(), 4_000_000_000);
        assert_eq!(p.game.summary(), summary);
    }

    #[test]
    fn sudden_death_fails_live_on_miss_without_changing_replay_playback() {
        let c = chart();
        let mut live = Play::new(c.clone(), String::new(), [0; 16], 0, 0, 32, MapSounds::default(), None);
        live.update(&clock(), 2_200_000_000);
        assert!(live.failed, "SD must fail on the first miss");
        let pb = Playback::new(&c, 32, vec![], 3_000_000, None);
        let mut replay = Play::new(c, String::new(), [0; 16], 0, 0, 32, MapSounds::default(), Some(pb));
        replay.update(&clock(), 2_200_000_000);
        assert!(!replay.failed, "old recordings must not gain SD failure");
    }

    #[test]
    fn slider_head_break_triggers_sd_failure_or_pf_retry() {
        for mods in [32, 32 | 16384] {
            let c = chart();
            let mut p = Play::new(c, String::new(), [0; 16], 0, 0, mods, MapSounds::default(), None);
            p.pending.push(Input { t: 2_000 * MS, kind: InputKind::Move { x: to_fx(60.0), y: to_fx(50.0) } });
            p.pending.push(Input { t: 2_000 * MS, kind: InputKind::Press(Button::M1) });
            p.pending.push(Input { t: 2_010 * MS, kind: InputKind::Release(Button::M1) });
            p.update(&clock(), 2_600_000_000);
            assert_eq!(p.game.result(0), Some(HitResult::Hit300));
            assert_eq!(p.failed, mods == 32);
            assert_eq!(p.retry_requested, mods != 32);
        }
    }

    #[test]
    fn no_50s_miss_triggers_sudden_death_and_perfect() {
        for mods in [32, 32 | 16384] {
            let c = chart();
            let at = 2_000 * MS - c.window_100 - 1;
            let mut p = Play::new(c, String::new(), [0; 16], 0, 0, mods, MapSounds::default(), None);
            p.game.enable_live_no_50s();
            p.pending.push(Input { t: at, kind: InputKind::Move { x: to_fx(60.0), y: to_fx(50.0) } });
            p.pending.push(Input { t: at, kind: InputKind::Press(Button::M1) });
            p.update(&clock(), 2_000_000_000);
            assert_eq!(p.game.result(0), Some(HitResult::Miss));
            assert_eq!(p.failed, mods == 32);
            assert_eq!(p.retry_requested, mods != 32);
        }
    }

    #[test]
    fn perfect_restarts_on_a_hundred_but_sudden_death_does_not() {
        for (mods, retry) in [(32, false), (32 | 16384, true)] {
            let c = chart();
            let mut p = Play::new(c, String::new(), [0; 16], 0, 0, mods, MapSounds::default(), None);
            p.pending.push(Input { t: 2_060 * MS, kind: InputKind::Move { x: to_fx(60.0), y: to_fx(50.0) } });
            p.pending.push(Input { t: 2_060 * MS, kind: InputKind::Press(Button::M1) });
            p.update(&clock(), 2_100_000_000);
            assert_eq!(p.game.result(0), Some(HitResult::Hit100));
            assert_eq!(p.retry_requested, retry);
            assert!(!p.failed);
        }
    }

    #[test]
    fn a_tick_is_placed_on_the_music_clock_before_it_is_judged_and_not_sounded_twice() {
        let (chart, map_sounds) = (chart(), None::<MapSounds>);
        let map_sounds = map_sounds.unwrap_or_else(|| sounds(&chart));
        // Object 1: a slider at 2.4 s, tick at +75 ms, its ball starts at (x, y).
        let o = chart.objects[1];
        let tick = chart.slider_nested(chart.slider(&o)).iter().find(|n| n.kind == NestedKind::Tick).copied().unwrap();
        let mut game = Game::new(chart.clone());
        let mut none = |_| {};
        let on = |t: Time, kind| Input { t, kind };
        // Following the ball from before its head.
        game.push(on(o.t - 50 * MS, InputKind::Move { x: o.x, y: o.y }), &mut none);
        game.push(on(o.t - 10 * MS, InputKind::Press(Button::K1)), &mut none);
        game.advance_to(o.t + 40 * MS, &mut none);
        game.push(on(o.t + 40 * MS, InputKind::Move { x: tick.x, y: tick.y }), &mut none);

        let mut fb = feedback();
        // The horizon does not reach the tick yet: nothing.
        fb.speculate(&game, &chart, &map_sounds, tick.t - MS, 0.0, 0);
        assert_eq!(fb.cues_len, 0);
        // The horizon reaches it: decided and placed on its frame.
        fb.speculate(&game, &chart, &map_sounds, tick.t, 0.0, 0);
        assert_eq!(fb.cues_len, 1);
        let cue = fb.cues[0];
        assert_eq!(cue.sound.ids[0], 19, "object 1's tick");
        let frames = (tick.t as f64 * SAMPLE_RATE as f64 / 1e6).round();
        assert_eq!(cue.at.map(f64::round), Some(frames));
        // Asked again, it is not decided again.
        fb.speculate(&game, &chart, &map_sounds, tick.t + 5 * MS, 0.0, 0);
        assert_eq!(fb.cues_len, 1);
        // The judgement that follows finds it already sounded.
        fb.on_event(
            &chart,
            &map_sounds,
            Event::SliderPoint { object: 1, kind: NestedKind::Tick, hit: true, t: tick.t },
        );
        assert_eq!(fb.cues_len, 1);
        // A tick forecast as missed and then hit is still sounded, late.
        let mut late = feedback();
        late.plan(1, tick.t, false);
        late.on_event(
            &chart,
            &map_sounds,
            Event::SliderPoint { object: 1, kind: NestedKind::Tick, hit: true, t: tick.t },
        );
        assert_eq!(late.cues_len, 1);
        assert_eq!(late.cues[0].at, None);
    }

    #[test]
    fn stable_visual_track_waits_for_stationary_frame_before_moving() {
        use rsu_formats::osr::Frame;
        let frames = vec![
            Frame { t: 0, x: 256.0, y: -500.0, keys: 5 },
            Frame { t: -1, x: 256.0, y: -500.0, keys: 0 },
            Frame { t: 10, x: 100.0, y: 50.0, keys: 0 },
            Frame { t: 20, x: 100.0, y: 50.0, keys: 5 },
            Frame { t: 30, x: 200.0, y: 50.0, keys: 0 },
        ];
        let track = VisualTrack::from_stable_frames(&frames);
        assert_eq!(track.position(0), None, "stable's initial sentinel is not a cursor");
        assert_eq!(track.position(15 * MS), Some((100.0, 50.0)));
        assert_eq!(track.position(20 * MS), Some((100.0, 50.0)));
        assert_eq!(track.position(25 * MS), Some((150.0, 50.0)));
        assert_eq!(track.position(30 * MS), Some((200.0, 50.0)));
        assert_eq!(track.position(40 * MS), Some((200.0, 50.0)));
    }

    #[test]
    fn finite_extreme_osr_coordinates_never_create_nonfinite_gpu_positions() {
        let frames = [Frame { t: 0, x: f32::MAX, y: -f32::MAX, keys: 0 },
                      Frame { t: 10, x: -f32::MAX, y: f32::MAX, keys: 0 }];
        let track = VisualTrack::from_stable_frames(&frames);
        let field = Field::new(1920.0, 1080.0);
        for t in [0, 1, 5, 9, 10] {
            let (x, y) = track.position(t * MS).unwrap();
            assert!(x.is_finite() && y.is_finite(), "track at {t} ms: {x}, {y}");
            let (sx, sy) = field.to_screen(x, y);
            assert!(sx.is_finite() && sy.is_finite(), "GPU at {t} ms: {sx}, {sy}");
        }
    }

    #[test]
    fn stable_visual_track_handles_duplicates_negative_times_and_gaps_without_keys() {
        let f = |t, x, keys| Frame { t, x, y: 30.0, keys };
        let track = VisualTrack::from_stable_frames(&[
            f(-5, 0.0, 1), f(-5, 10.0, 0), f(5, 30.0, 4),
            f(205, 90.0, 8), f(215, 100.0, 0),
        ]);
        assert_eq!(track.position(-6 * MS), None);
        assert_eq!(track.position(-5 * MS), Some((10.0, 30.0)), "last duplicate wins");
        assert_eq!(track.position(0), Some((20.0, 30.0)), "key changes do not alter position");
        assert_eq!(track.position(50 * MS), Some((30.0, 30.0)), "hold during a dropout");
        assert_eq!(track.position(205 * MS), Some((90.0, 30.0)), "resume on recorded frame");
        assert_eq!(track.position(300 * MS), Some((100.0, 30.0)));
        assert_eq!(VisualTrack::from_stable_frames(&[]).position(0), None);
    }

    #[test]
    fn stable_visual_track_seeks_and_trail_are_independent_of_render_history() {
        let frames = [Frame { t: 0, x: 0.0, y: 0.0, keys: 0 }, Frame { t: 20, x: 100.0, y: 50.0, keys: 5 }];
        let track = VisualTrack::from_stable_frames(&frames);
        let sample = |t| {
            let mut trail = Vec::new();
            track.trail(t, |p, a| trail.push((p, a)));
            (track.position(t), trail)
        };
        let at_ten = sample(10 * MS);
        for t in [20, 0, 15, 5, 10, 170, 10] {
            let _ = sample(t * MS); // simulate seeks and uneven render intervals
        }
        assert_eq!(sample(10 * MS), at_ten);
        assert_eq!(at_ten.0, Some((50.0, 25.0)));
        assert!(at_ten.1.iter().any(|(p, _)| *p == (25.0, 12.5)));
    }

    #[test]
    fn joined_trail_bounds_stamp_count_for_extreme_movement() {
        let frames: Vec<_> = (0..=100).map(|i| Frame {
            t: i * 5, x: if i % 2 == 0 { -4096.0 } else { 4096.0 }, y: 0.0, keys: 0,
        }).collect();
        let pb = Playback::new(&chart(), 0, vec![], 1000 * MS, None).with_stable_visual(&frames);
        let mut count = 0;
        pb.skin_trail(500 * MS, true, 1.0, |_, _| count += 1);
        assert!(count <= 700, "extreme replay emitted {count} trail stamps");
    }

    #[test]
    fn disjoint_replay_trail_uses_legacy_timing_and_is_seek_independent() {
        let frames = [Frame { t: 0, x: 0.0, y: 0.0, keys: 0 }, Frame { t: 100, x: 100.0, y: 0.0, keys: 0 }];
        let pb = Playback::new(&chart(), 0, vec![], 500 * MS, None).with_stable_visual(&frames);
        let sample = |t| {
            let mut points = Vec::new();
            pb.skin_trail(t, false, 10.0, |p, a| points.push((p, a)));
            points
        };
        let trail = sample(100 * MS);
        assert!(trail.len() >= 6 && trail.len() <= 7, "legacy trail is sampled at 60 Hz: {trail:?}");
        assert_eq!(trail.last().unwrap().0, (100.0, 0.0));
        assert_eq!(sample(100 * MS), trail);
        assert!(sample(-MS).is_empty());
        assert!(sample(300 * MS).iter().all(|(p, _)| *p == (100.0, 0.0)));
    }

    #[test]
    fn native_replay_trail_uses_recorded_moves_not_the_physical_pointer() {
        let chart = chart();
        let inputs = vec![
            Input { t: 0, kind: InputKind::Move { x: to_fx(12.0), y: to_fx(15.0) } },
            Input { t: 20 * MS, kind: InputKind::Press(Button::M1) },
            Input { t: 50 * MS, kind: InputKind::Move { x: to_fx(100.0), y: to_fx(30.0) } },
        ];
        let pb = Playback::new(&chart, 0, inputs.clone(), 300 * MS, None);
        let sample = |t| {
            let mut points = Vec::new();
            pb.trail(t, |pos, alpha| points.push((pos, alpha)));
            points
        };
        assert!(sample(-MS).is_empty(), "nothing should be drawn before the first move");
        assert_eq!(sample(40 * MS).last().unwrap().0, (12.0, 15.0));
        let at_eighty = sample(80 * MS);
        assert!(at_eighty.iter().any(|(p, _)| *p == (12.0, 15.0)));
        assert_eq!(at_eighty.last().unwrap().0, (100.0, 30.0));
        for t in [200, 0, 80, 50] { let _ = sample(t * MS); }
        assert_eq!(sample(80 * MS), at_eighty, "seek and pause never leave ghost points");
        assert_eq!(pb.inputs, inputs, "the judged input stream stays unchanged");
    }

    #[test]
    fn native_trail_indexes_moves_across_button_only_sections() {
        let chart = chart();
        let mut inputs = vec![Input { t: 0, kind: InputKind::Move { x: to_fx(12.0), y: to_fx(15.0) } }];
        inputs.extend((1..=4096).map(|n| Input { t: n * MS, kind: InputKind::Press(Button::M1) }));
        inputs.push(Input { t: 5_000 * MS, kind: InputKind::Move { x: to_fx(100.0), y: to_fx(30.0) } });
        let pb = Playback::new(&chart, 0, inputs, 6_000 * MS, None);
        assert_eq!(pb.native_moves.len(), 2, "each sample must search the move index, not thousands of button inputs");
        let mut latest = None;
        pb.trail(4_000 * MS, |point, _| latest = Some(point));
        assert_eq!(latest, Some((12.0, 15.0)));
        pb.trail(5_000 * MS, |point, _| latest = Some(point));
        assert_eq!(latest, Some((100.0, 30.0)));
    }

    #[test]
    fn stable_visual_track_never_changes_judgement_or_native_playback() {
        let chart = chart();
        let inputs = autoplay(&chart, 133);
        let end = 17_000_000;
        let expected = Game::simulate(chart.clone(), 0, &inputs, end);
        let frames = [Frame { t: 0, x: 456.0, y: 170.0, keys: 0 }, Frame { t: 10, x: 100.0, y: 50.0, keys: 5 }];
        let vanilla = Playback::new(&chart, 0, inputs.clone(), end, Some(expected));
        let stable = Playback::new(&chart, 0, inputs.clone(), end, Some(expected)).with_stable_visual(&frames);
        assert_eq!(vanilla.inputs, stable.inputs, "buttons and moves are never interpolated in the judge");
        assert!(vanilla.visual.is_none());
        let lead = Play::lead_in_for(&chart, 0);
        let mut vanilla = Play::new(chart.clone(), String::new(), [0; 16], lead, 0, 0, MapSounds::default(), Some(vanilla));
        let mut stable = Play::new(chart, String::new(), [0; 16], lead, 0, 0, MapSounds::default(), Some(stable));
        let field = Field::new(1920.0, 1080.0);
        let c = clock();
        for t in [0, 5_000, 10_000, 1_000_000, 8_000_000, 3_000_000, 17_000_000] {
            let now = (t + lead) as u64 * 1000;
            vanilla.update(&c, now);
            stable.update(&c, now);
            assert_eq!(vanilla.game.summary(), stable.game.summary());
            assert_eq!(vanilla.game.cursor(), stable.game.cursor());
            if t == 5_000 { assert_ne!(vanilla.display_cursor(&field), stable.display_cursor(&field)); }
        }
        assert_eq!(stable.game.summary(), expected);
    }

    #[test]
    fn stable_visual_uses_map_time_for_speed_pause_and_hr_coordinates() {
        let chart = chart();
        let lead = 1_500_000;
        let frames = [Frame { t: 450, x: 100.0, y: 60.0, keys: 0 },
                      Frame { t: 550, x: 200.0, y: 80.0, keys: 0 }];
        let pb = Playback::new(&chart, rsu_core::mods::HARD_ROCK, vec![], 2_000_000, None)
            .with_stable_visual(&frames);
        let mut play = Play::new(chart, String::new(), [0; 16], lead, 0,
            rsu_core::mods::HARD_ROCK, MapSounds::default(), Some(pb));
        let field = Field::new(1920.0, 1080.0);
        let normal = clock();
        play.update(&normal, 2_000_000_000);
        assert_eq!(play.display_cursor(&field), field.to_screen(150.0, 70.0));
        let mut fast = clock();
        fast.rate *= 2.0;
        play.update(&fast, 1_000_000_000);
        assert_eq!(play.display_cursor(&field), field.to_screen(150.0, 70.0));
        // A paused clock returns the same timeline position as wall time advances.
        let mut paused = clock();
        paused.pos0 = 2.0 * SAMPLE_RATE as f64;
        paused.rate = 0.0;
        play.update(&paused, 1_000_000_000);
        let still = play.display_cursor(&field);
        play.update(&paused, 20_000_000_000);
        assert_eq!(play.display_cursor(&field), still);
        assert_eq!(still, field.to_screen(150.0, 70.0));
        // If the previous GPU frame made us wait before updating, cursor and
        // history both sample the later map time, not the pre-wait timestamp.
        play.update(&normal, 2_030_000_000);
        assert_eq!(play.display_cursor(&field), field.to_screen(180.0, 76.0));
        let visual = play.playback.as_ref().unwrap().visual.as_ref().unwrap();
        let mut newest = None;
        visual.trail(play.now, |pos, _| newest = Some(pos));
        assert_eq!(newest, Some((180.0, 76.0)));
    }

    #[test]
    fn field_round_trips_and_is_centred() {
        let f = Field::new(1920.0, 1080.0);
        assert_eq!(f.scale, 2.25);
        let (x, y) = f.to_screen(256.0, 192.0);
        assert_eq!((x, y), (960.0, 540.0 + 18.0));
        let (ox, oy) = f.to_osu(x, y);
        assert!((ox - 256.0).abs() < 1e-4 && (oy - 192.0).abs() < 1e-4);
        // A tall window is limited by width instead.
        assert_eq!(Field::new(640.0, 2000.0).scale, 1.0);
    }

    #[test]
    fn map_time_accounts_for_lead_in() {
        let p = Play::new(chart(), String::new(), [0; 16], 1_500_000, 0, 0, MapSounds::default(), None);
        let c = clock();
        assert_eq!(p.map_time(&c, 0), -1_500_000);
        assert_eq!(p.map_time(&c, 1_500_000_000), 0);
        assert_eq!(p.map_time(&c, 2_000_000_000), 500_000);
    }

    #[test]
    fn the_offset_delays_the_whole_game_not_just_the_judgement() {
        let c = clock();
        // Sound arrives 20 ms after the device says: at the moment the
        // device's clock reads 0.5 s the player hears, and the game shows
        // and judges, 0.48 s.
        let mut p = Play::new(chart(), String::new(), [0; 16], 1_500_000, 20_000, 0, MapSounds::default(), None);
        assert_eq!(p.map_time(&c, 2_000_000_000), 480_000);
        p.update(&c, 2_000_000_000);
        assert_eq!(p.now, 480_000, "drawing uses the same time as judging");
        // Seeking to a map time lands on it.
        let frames = p.timeline_frames(480_000);
        assert_eq!(frames, c.pos_at(2_000_000_000));
        p.nudge_offset(-5);
        assert_eq!(p.map_time(&c, 2_000_000_000), 485_000);
        // At 1.5x, 20 ms of real time is 30 ms of the map.
        let dt = rsu_core::mods::DOUBLE_TIME;
        let p = Play::new(chart(), String::new(), [0; 16], 1_500_000, 20_000, dt, MapSounds::default(), None);
        assert_eq!(p.map_time(&c, 2_000_000_000), 470_000);
    }

    /// A rate of the player's own replaces the speed of the mods everywhere
    /// speed matters.
    #[test]
    fn a_custom_rate_replaces_the_speed_of_the_mods() {
        let c = clock();
        let dt = rsu_core::mods::DOUBLE_TIME;
        let mut p = Play::new(chart(), String::new(), [0; 16], 1_500_000, 20_000, dt, MapSounds::default(), None);
        p.rate_pct = 200;
        assert_eq!(p.speed(), 2.0);
        // 20 ms of real time is 40 ms of the map at 2x.
        assert_eq!(p.map_time(&c, 2_000_000_000), 460_000);
        p.rate_pct = 0;
        assert_eq!(p.speed(), 1.5);
    }

    /// The live path end to end, minus the window: inputs arrive as window
    /// pixels on a wall clock, in frames; what gets recorded must re-simulate
    /// to exactly the live result.
    #[test]
    fn a_live_session_records_a_replay_that_reproduces_it() {
        let chart = chart();
        let lead_in = Play::lead_in_for(&chart, 0);
        let mut live = Play::new(chart.clone(), String::new(), [0; 16], lead_in, 0, 0, sounds(&chart), None);
        let c = clock();
        let field = Field::new(1920.0, 1080.0);
        let script = autoplay(&chart, 133);
        let to_ns = |t: Time| (t + lead_in) as u64 * 1000;
        let mut next = 0;
        let mut now_ns = 0u64;
        let mut played = Vec::new();
        let (mut slides, mut spin_rates) = (Vec::new(), Vec::new());
        while !live.done {
            now_ns += 1_000_000; // 1000 fps
                                 // Deliver, in a scrambled order, the inputs stamped before this frame.
            let from = next;
            while next < script.len() && to_ns(script[next].t) <= now_ns {
                next += 1;
            }
            for input in script[from..next].iter().rev() {
                match input.kind {
                    InputKind::Move { x, y } => {
                        let (sx, sy) = field.to_screen(from_fx(x), from_fx(y));
                        live.queue_move(&c, to_ns(input.t), &field, sx, sy);
                    }
                    InputKind::Press(b) => live.queue_button(&c, to_ns(input.t), b, true),
                    InputKind::Release(b) => live.queue_button(&c, to_ns(input.t), b, false),
                }
            }
            live.update(&c, now_ns);
            for cue in live.cues() {
                played.push((cue.sound.ids[0], cue.from_click));
            }
            slides.extend(live.loops()[0].map(|l| (l.sample, l.volume)));
            spin_rates.extend(live.loops()[1].map(|l| l.rate));
            assert!(now_ns < 60_000_000_000, "the session never finished");
        }
        assert!(live.loops().iter().all(Option::is_none), "nothing is left humming");
        // Held sliders slide in the sample set and at the volume of their
        // timing section: normal at first, soft at 60% from the fifth second.
        slides.dedup();
        assert_eq!((slides.first(), slides.last()), (Some(&(901, 100)), Some(&(902, 60))));
        // The spinner hums higher as it fills, and stops rising at 2.5 times
        // what it needed.
        assert!(spin_rates.windows(2).all(|w| w[0] <= w[1]));
        assert!(spin_rates[0] < 0.6 && *spin_rates.last().unwrap() > 1.0, "{:?}", spin_rates.last());
        // Turns beyond what the spinner needed each ring the bonus.
        let bonus = played.iter().filter(|p| p.0 == 905).count();
        assert!(bonus > 0);
        played.retain(|p| p.0 != 905);
        assert!(played.iter().all(|p| p.0 != 906), "no combo was broken");
        // Every part of every object sounded once, in order: object 0 is a
        // circle (clicked), object 1 a slider: head (clicked), tick, repeat,
        // tail (the last three happen by themselves).
        assert_eq!(&played[..5], &[(0, true), (10, true), (19, false), (11, false), (12, false)]);
        // 20 circles + 10 sliders * 4 parts + the spinner.
        assert_eq!(played.len(), 20 + 40 + 1);
        assert_eq!(played.last(), Some(&(300, false)), "the spinner sounds when it ends");
        let s = live.game.summary();
        assert_eq!((s.n300, s.n100, s.n50, s.miss), (31, 0, 0, 0), "{s:?}");
        assert_eq!(live.late_inputs, 0);
        assert!(!live.record_overflow);
        // The recording, simulated in one go, lands on the same state.
        let again = Game::simulate(chart.clone(), 0, &live.recorded, live.game.time());
        assert_eq!(again, s);

        // And played back through the same frame loop, at another frame rate.
        let pb = Playback::new(&chart, 0, live.recorded.clone(), live.game.time(), Some(s));
        let mut replay = Play::new(chart, String::new(), [0; 16], lead_in, 0, 0, MapSounds::default(), Some(pb));
        let mut now_ns = 0u64;
        while !replay.done {
            now_ns += 6_944_444; // 144 fps
            replay.update(&c, now_ns);
        }
        assert_eq!(replay.game.summary(), s);
    }

    /// Seeking: wherever the clock jumps to, the state must be exactly what
    /// playing straight through to that moment gives.
    #[test]
    fn seeking_lands_on_the_same_state_as_playing_through() {
        let chart = chart();
        // The test seeks to -900 ms, so the simulated clock must start
        // early enough to represent that map time without unsigned wrapping.
        let lead_in = Play::lead_in_for(&chart, 0).max(900_000);
        let inputs = autoplay(&chart, 133);
        let end = 17_000_000;
        let expected = Game::simulate(chart.clone(), 0, &inputs, end);
        let c = clock();
        let to_ns = |t: Time| (t + lead_in) as u64 * 1000;
        // What straight play has at map time t.
        let straight = |t: Time| {
            let upto: Vec<_> = inputs.iter().copied().filter(|i| i.t <= t).collect();
            Game::simulate(chart.clone(), 0, &upto, t.min(end))
        };

        let pb = Playback::new(&chart, 0, inputs.clone(), end, Some(expected));
        assert!(pb.keyframe_count() > 10, "{} keyframes", pb.keyframe_count());
        let mut play = Play::new(chart.clone(), String::new(), [0; 16], lead_in, 0, 0, MapSounds::default(), Some(pb));
        // Jump around: forwards, backwards, into the middle of sliders and
        // the spinner, to the very start, past the end and back.
        let jumps = [3_000_000, 2_450_123, 9_999_999, 2_000_000, 14_800_000, 6_472_500, -900_000, 16_999_000, 5_000];
        for (n, &t) in jumps.iter().enumerate() {
            play.update(&c, to_ns(t));
            assert_eq!(play.game.summary(), straight(t), "after jump {n} to {t}");
            // And it keeps playing correctly from there.
            for step in 1..=40 {
                play.update(&c, to_ns(t + step * 7_000));
            }
            assert_eq!(play.game.summary(), straight(t + 280_000), "280 ms after jump {n}");
        }
        assert!(play.playback.as_ref().unwrap().seeks >= jumps.len() as u32 - 1);
        // Playing on to the end still arrives at the recorded result.
        let mut t = 5_000 + 280_000;
        while !play.done {
            t += 4_000;
            play.update(&c, to_ns(t));
        }
        assert_eq!(play.game.summary(), expected);
    }

    #[test]
    fn keyframe_spacing_respects_the_memory_budget() {
        // A map with very many objects must get fewer, wider keyframes.
        let mut b = ChartBuilder::new(Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 });
        for i in 0..200_000i64 {
            b.circle(i * 3 * MS, to_fx(100.0), to_fx(100.0));
        }
        let chart = Arc::new(b.build());
        let pb = Playback::new(&chart, 0, vec![Input { t: 0, kind: InputKind::Press(Button::K1) }], 600_000_000, None);
        // 200k objects are ~9.6 MB per snapshot: 64 MB allows six.
        assert!(pb.keyframe_count() <= 7, "{}", pb.keyframe_count());
    }
}
