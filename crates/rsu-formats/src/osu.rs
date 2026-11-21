//! `.osu` beatmap parser (osu! file formats v3 to v14, and lazer's v128).
//!
//! Like the game it comes from, the parser is forgiving: a line it cannot
//! make sense of is skipped and counted, not fatal. Only a file that is not
//! a beatmap at all, or is for another game mode, is an error.

use crate::{curve, stacking};
use rsu_core::chart::{difficulty_range, object_scale, Chart, ChartBuilder, Difficulty, NestedKind};
use rsu_core::{to_fx, Time, MS};

#[derive(Clone, Debug, PartialEq)]
pub struct TimingPoint {
    pub time_ms: f64,
    /// Milliseconds per beat for uninherited points; a negative slider
    /// velocity percentage for inherited ones.
    pub beat_length: f64,
    pub uninherited: bool,
    /// Hit sound settings from this point on: sample set (0 = the map's
    /// default, 1 normal, 2 soft, 3 drum), custom sample index, volume 0..100.
    pub sample_set: u8,
    pub sample_index: u32,
    pub volume: u8,
}

/// The per-object hit sound overrides ("hitSample" in the file format).
/// Zero means "not overridden" in every field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObjectSample {
    pub normal_set: u8,
    pub addition_set: u8,
    pub index: u32,
    pub volume: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RawKind {
    Circle,
    Slider {
        curve_type: char,
        /// Control points after the head.
        points: Vec<(f32, f32)>,
        slides: u32,
        length: f64,
        /// Hit sound bits and sample sets for the head, each repeat and the
        /// tail; may be shorter than that (the object's own apply then).
        edge_sounds: Vec<u8>,
        edge_sets: Vec<(u8, u8)>,
    },
    Spinner {
        end_ms: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawObject {
    pub x: f32,
    pub y: f32,
    pub time_ms: f64,
    pub new_combo: bool,
    /// Extra combo colours to skip when this object starts a combo.
    pub combo_skip: u8,
    /// Hit sound bits: 2 whistle, 4 finish, 8 clap (the normal sound always plays).
    pub hit_sound: u8,
    pub sample: ObjectSample,
    pub kind: RawKind,
}

#[derive(Clone, Debug)]
pub struct Beatmap {
    pub format_version: u32,
    pub audio_filename: String,
    pub audio_lead_in_ms: i64,
    /// Where the song-select preview starts; negative means "not set".
    pub preview_ms: i64,
    /// Default sample set: 1 normal, 2 soft, 3 drum.
    pub sample_set: u8,
    pub stack_leniency: f32,
    pub title: String,
    pub artist: String,
    pub creator: String,
    pub version: String,
    pub source: String,
    pub tags: String,
    /// File name of the background image, as the map writes it; may be empty.
    pub background: String,
    pub difficulty: Difficulty,
    pub slider_multiplier: f64,
    pub slider_tick_rate: f64,
    pub breaks: Vec<(i64, i64)>,
    pub timing_points: Vec<TimingPoint>,
    pub objects: Vec<RawObject>,
    /// Lines that looked like data but could not be parsed.
    pub skipped_lines: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The first line is not "osu file format vN".
    NotABeatmap,
    /// taiko, catch or mania.
    UnsupportedMode(u8),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NotABeatmap => write!(f, "not an .osu beatmap"),
            ParseError::UnsupportedMode(m) => {
                let name = ["osu!", "osu!taiko", "osu!catch", "osu!mania"].get(*m as usize).unwrap_or(&"unknown");
                write!(f, "beatmap is for {name}; only osu! standard is supported")
            }
        }
    }
}

impl std::error::Error for ParseError {}

fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
    s.trim().parse().ok()
}

fn finite(s: &str) -> Option<f64> {
    num::<f64>(s).filter(|v| v.is_finite())
}

/// Parse from raw bytes; text that is not valid UTF-8 is read lossily
/// (old maps carry metadata in legacy code pages).
pub fn parse(bytes: &[u8]) -> Result<Beatmap, ParseError> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().map(|l| l.trim_start_matches('\u{feff}').trim_end());

    let header = lines.by_ref().find(|l| !l.trim().is_empty()).ok_or(ParseError::NotABeatmap)?;
    let format_version: u32 =
        header.trim().strip_prefix("osu file format v").and_then(num).ok_or(ParseError::NotABeatmap)?;
    // Maps older than v5 are timed against a decoder that was 24 ms off.
    let offset_ms = if format_version < 5 { 24.0 } else { 0.0 };

    let mut map = Beatmap {
        format_version,
        audio_filename: String::new(),
        audio_lead_in_ms: 0,
        preview_ms: -1,
        sample_set: 1,
        stack_leniency: 0.7,
        title: String::new(),
        artist: String::new(),
        creator: String::new(),
        version: String::new(),
        source: String::new(),
        tags: String::new(),
        background: String::new(),
        difficulty: Difficulty { cs: 5.0, od: 5.0, ar: 5.0, hp: 5.0 },
        slider_multiplier: 1.4,
        slider_tick_rate: 1.0,
        breaks: Vec::new(),
        timing_points: Vec::new(),
        objects: Vec::new(),
        skipped_lines: 0,
    };
    let mut has_ar = false;
    let mut section = "";

    for line in lines {
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let ok = match section {
            "General" | "Metadata" | "Difficulty" => {
                let Some((key, value)) = line.split_once(':') else { continue };
                let (key, value) = (key.trim(), value.trim());
                let diff = |v: &str| num::<f32>(v).filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 10.0));
                match key {
                    "AudioFilename" => map.audio_filename = value.to_string(),
                    "AudioLeadIn" => map.audio_lead_in_ms = finite(value).unwrap_or(0.0) as i64,
                    "PreviewTime" => map.preview_ms = finite(value).unwrap_or(-1.0) as i64,
                    "StackLeniency" => map.stack_leniency = num(value).unwrap_or(0.7),
                    "SampleSet" => map.sample_set = sample_set_named(value).max(1),
                    "Mode" => {
                        let mode = num::<u8>(value).unwrap_or(0);
                        if mode != 0 {
                            return Err(ParseError::UnsupportedMode(mode));
                        }
                    }
                    "Title" => map.title = value.to_string(),
                    "Artist" => map.artist = value.to_string(),
                    "Creator" => map.creator = value.to_string(),
                    "Version" => map.version = value.to_string(),
                    "Source" => map.source = value.to_string(),
                    "Tags" => map.tags = value.to_string(),
                    "HPDrainRate" => map.difficulty.hp = diff(value).unwrap_or(5.0),
                    "CircleSize" => map.difficulty.cs = diff(value).unwrap_or(5.0),
                    "OverallDifficulty" => map.difficulty.od = diff(value).unwrap_or(5.0),
                    "ApproachRate" => {
                        if let Some(v) = diff(value) {
                            map.difficulty.ar = v;
                            has_ar = true;
                        }
                    }
                    "SliderMultiplier" => map.slider_multiplier = finite(value).unwrap_or(1.4),
                    "SliderTickRate" => map.slider_tick_rate = finite(value).unwrap_or(1.0),
                    _ => {}
                }
                true
            }
            "Events" => {
                let mut f = line.split(',');
                match f.next().map(str::trim) {
                    Some("2") | Some("Break") => match (f.next().and_then(finite), f.next().and_then(finite)) {
                        (Some(a), Some(b)) => {
                            map.breaks.push(((a + offset_ms) as i64, (b + offset_ms) as i64));
                            true
                        }
                        _ => false,
                    },
                    // The first image event is the background.
                    Some("0") | Some("Background") if map.background.is_empty() => {
                        if let Some(name) = f.nth(1) {
                            map.background = name.trim().trim_matches('"').to_string();
                        }
                        true
                    }
                    _ => true, // videos, storyboard: not ours
                }
            }
            "TimingPoints" => parse_timing_point(line, offset_ms).map(|p| map.timing_points.push(p)).is_some(),
            "HitObjects" => match parse_object(line, offset_ms) {
                Some(Some(o)) => {
                    map.objects.push(o);
                    true
                }
                Some(None) => true, // a kind that does not exist in this mode
                None => false,
            },
            _ => true,
        };
        if !ok {
            map.skipped_lines += 1;
        }
    }
    // Before ApproachRate existed, OD doubled as AR.
    if !has_ar {
        map.difficulty.ar = map.difficulty.od;
    }
    Ok(map)
}

fn sample_set_named(name: &str) -> u8 {
    match name.trim().to_ascii_lowercase().as_str() {
        "normal" => 1,
        "soft" => 2,
        "drum" => 3,
        _ => 0,
    }
}

/// "normalSet:additionSet:index:volume:filename"; missing parts are zero.
fn parse_object_sample(field: Option<&&str>) -> ObjectSample {
    let mut parts = field.map(|f| f.split(':')).into_iter().flatten();
    let mut next = || parts.next().and_then(|v| num::<u32>(v)).unwrap_or(0);
    let (normal_set, addition_set) = (next().min(3) as u8, next().min(3) as u8);
    ObjectSample { normal_set, addition_set, index: next(), volume: next().min(100) as u8 }
}

fn parse_timing_point(line: &str, offset_ms: f64) -> Option<TimingPoint> {
    let f: Vec<&str> = line.split(',').collect();
    let time_ms = finite(f.first()?)? + offset_ms;
    let beat_length = num::<f64>(f.get(1)?)?;
    // Field 6 is absent in old formats, where every point is uninherited
    // unless its beat length is negative.
    let uninherited = match f.get(6).and_then(|v| num::<i32>(v)) {
        Some(v) => v != 0,
        None => beat_length >= 0.0,
    };
    // NaN is a real, deliberate value on inherited points (mappers use it
    // to make sliders without ticks); a tempo cannot be NaN.
    if beat_length.is_nan() && uninherited {
        return None;
    }
    Some(TimingPoint {
        time_ms,
        beat_length,
        uninherited,
        sample_set: f.get(3).and_then(|v| num::<u8>(v)).unwrap_or(0).min(3),
        sample_index: f.get(4).and_then(|v| num::<u32>(v)).unwrap_or(0),
        volume: f.get(5).and_then(|v| num::<u32>(v)).unwrap_or(100).min(100) as u8,
    })
}

/// `None`: malformed. `Some(None)`: valid but not an osu!-standard object.
fn parse_object(line: &str, offset_ms: f64) -> Option<Option<RawObject>> {
    let f: Vec<&str> = line.split(',').collect();
    let x = num::<f32>(f.first()?).filter(|v| v.is_finite())?;
    let y = num::<f32>(f.get(1)?).filter(|v| v.is_finite())?;
    let time_ms = finite(f.get(2)?)? + offset_ms;
    let ty = num::<u32>(f.get(3)?)?;
    let hit_sound = f.get(4).and_then(|v| num::<u32>(v)).unwrap_or(0) as u8 & 0b1110;
    let sample;
    let kind = if ty & 1 != 0 {
        sample = parse_object_sample(f.get(5));
        RawKind::Circle
    } else if ty & 2 != 0 {
        sample = parse_object_sample(f.get(10));
        let mut parts = f.get(5)?.split('|');
        let curve_type = parts.next()?.trim().chars().next().unwrap_or('B');
        let points = parts
            .filter_map(|p| {
                let (px, py) = p.split_once(':')?;
                Some((num::<f32>(px)?, num::<f32>(py)?))
            })
            .collect();
        RawKind::Slider {
            curve_type,
            points,
            slides: f.get(6).and_then(|v| num::<u32>(v)).unwrap_or(1).max(1),
            length: f.get(7).and_then(|v| finite(v)).unwrap_or(0.0).max(0.0),
            edge_sounds: f
                .get(8)
                .map(|v| v.split('|').map(|e| num::<u32>(e).unwrap_or(0) as u8 & 0b1110).collect())
                .unwrap_or_default(),
            edge_sets: f
                .get(9)
                .map(|v| {
                    v.split('|')
                        .map(|e| {
                            let (n, a) = e.split_once(':').unwrap_or((e, "0"));
                            (num::<u8>(n).unwrap_or(0).min(3), num::<u8>(a).unwrap_or(0).min(3))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    } else if ty & 8 != 0 {
        sample = parse_object_sample(f.get(6));
        RawKind::Spinner { end_ms: f.get(5).and_then(|v| finite(v)).map_or(time_ms, |e| e + offset_ms).max(time_ms) }
    } else {
        return Some(None); // mania hold note or an unknown type
    };
    Some(Some(RawObject {
        x,
        y,
        time_ms,
        new_combo: ty & 4 != 0,
        combo_skip: (ty >> 4 & 7) as u8,
        hit_sound,
        sample,
        kind,
    }))
}

fn to_us(ms: f64) -> Time {
    (ms * 1000.0).round() as Time
}

/// The timing in force at one moment.
#[derive(Clone, Copy)]
struct Timing {
    beat_length: f64,
    /// Slider velocity multiplier from the last inherited point.
    velocity: f64,
    /// False under a NaN inherited point: such sliders have no ticks.
    ticks: bool,
}

/// Most sliders have a handful of ticks; this only stops a malformed map
/// (near-zero tick distance, absurd repeat count) from eating all memory.
const MAX_NESTED: usize = 20_000;

/// A slider worked out in floating point, before stacking and quantising.
struct SliderGeometry {
    path: Vec<curve::P>,
    spans: u32,
    span_us: Time,
    nested: Vec<(Time, NestedKind)>,
}

impl Beatmap {
    /// Timing for each of `times` (which must be ascending).
    fn timing_at(&self, times: &[f64]) -> Vec<Timing> {
        let mut points: Vec<&TimingPoint> = self.timing_points.iter().collect();
        points.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms)); // stable: file order within a time
                                                                // Before the first tempo point, its tempo already applies.
        let first_beat = points.iter().find(|p| p.uninherited).map_or(1000.0, |p| p.beat_length);
        let mut now = Timing { beat_length: first_beat, velocity: 1.0, ticks: true };
        let mut next = 0;
        times
            .iter()
            .map(|&t| {
                while next < points.len() && points[next].time_ms <= t {
                    let p = points[next];
                    if p.uninherited {
                        // A new tempo also resets the velocity multiplier.
                        now = Timing { beat_length: p.beat_length, velocity: 1.0, ticks: true };
                    } else {
                        // -100 means 1x, -50 means 2x; NaN means "1x, no ticks".
                        let percent = if p.beat_length < 0.0 { (-p.beat_length).clamp(10.0, 1000.0) } else { 100.0 };
                        now.velocity = 100.0 / percent;
                        now.ticks = !p.beat_length.is_nan();
                    }
                    next += 1;
                }
                now
            })
            .collect()
    }

    fn slider_geometry(&self, o: &RawObject, timing: Timing) -> SliderGeometry {
        let RawKind::Slider { curve_type, points, slides, length, .. } = &o.kind else {
            unreachable!("slider_geometry is only called for sliders")
        };
        let mut control: Vec<curve::P> = vec![[o.x as f64, o.y as f64]];
        control.extend(points.iter().map(|&(x, y)| [x as f64, y as f64]));
        let path = curve::build(*curve_type, &control, *length);
        let length = path.length();
        let spans = (*slides).clamp(1, 10_000);

        // Pixels per beat is 100 * multiplier at 1x velocity.
        let per_beat = 100.0 * self.slider_multiplier * timing.velocity;
        let px_per_ms = per_beat / timing.beat_length;
        let span_ms = length / px_per_ms;
        let span_us = if span_ms.is_finite() && span_ms > 0.0 { to_us(span_ms).clamp(1, 3_600_000_000) } else { 1 };
        let start = to_us(o.time_ms);
        let end = start + spans as Time * span_us;

        let mut nested = Vec::new();
        // Before format v8 the velocity multiplier did not affect tick spacing.
        let legacy_ticks = if self.format_version < 8 { 1.0 / timing.velocity } else { 1.0 };
        let tick_distance = (per_beat / self.slider_tick_rate * legacy_ticks).clamp(0.0, length.min(100_000.0));
        if timing.ticks && tick_distance > 0.0 && tick_distance.is_finite() && px_per_ms.is_finite() {
            // No tick in the last 10 ms of a pass: it would sit on the end.
            let last = length - px_per_ms * 10.0;
            'spans: for span in 0..spans {
                let span_start = start + span as Time * span_us;
                let mut d = tick_distance;
                while d <= length && d < last {
                    let along = if span % 2 == 1 { 1.0 - d / length } else { d / length };
                    nested.push((span_start + (along * span_us as f64).round() as Time, NestedKind::Tick));
                    if nested.len() >= MAX_NESTED {
                        break 'spans;
                    }
                    d += tick_distance;
                }
            }
        }
        for span in 1..spans.min(MAX_NESTED as u32) {
            nested.push((start + span as Time * span_us, NestedKind::Repeat));
        }
        // The end is judged 36 ms early (but never before the midpoint), a
        // leniency the original has always had.
        nested.push(((start + (end - start) / 2).max(end - 36 * MS), NestedKind::Tail));
        SliderGeometry { path: path.points, spans, span_us, nested }
    }

    /// Build the runtime form the rules work on.
    pub fn to_chart(&self) -> Chart {
        self.to_chart_with_mods(0)
    }

    /// The chart as the mods make it: easy and hard rock change the
    /// difficulty, hard rock also flips the playfield upside down. The flip
    /// comes before stacking, so stacks still lean up and to the left as
    /// the player sees them (as McOsu draws it).
    pub fn to_chart_with_mods(&self, game_mods: u32) -> Chart {
        self.to_chart_with_practice(game_mods, PracticeDifficulty::default())
    }

    /// Override the final OD after mods, without changing stacking or score difficulty.
    pub fn to_chart_with_od(&self, game_mods: u32, od: Option<f32>) -> Chart {
        self.to_chart_with_practice(game_mods, PracticeDifficulty { od, ..Default::default() })
    }

    /// Practice values replace the difficulty after mods, before stacking.
    pub fn to_chart_with_practice(&self, game_mods: u32, practice: PracticeDifficulty) -> Chart {
        self.to_chart_with_practice_at_rate(game_mods, practice, rsu_core::mods::speed(game_mods))
    }

    /// `rate` is the effective map speed (custom rate takes precedence over DT/HT).
    pub fn to_chart_with_practice_at_rate(&self, game_mods: u32, practice: PracticeDifficulty, rate: f64) -> Chart {
        let rate = if rate.is_finite() && rate > 0.0 { rate } else { 1.0 };
        let difficulty = practice.apply(self.difficulty, game_mods);
        let flip = rsu_core::mods::flips(game_mods) ^ practice.flip_up_down;
        let flip_x = |x: f64| if practice.flip_left_right { 512.0 - x } else { x };
        let flip_y = |y: f64| if flip { 384.0 - y } else { y };
        // McOsu reads combo flags in file order before omitting spinners. Its
        // spinner flags reset the number and add skips, but don't advance it.
        let mut practice_combos = Vec::new();
        if practice.no_spinners {
            let (mut number, mut colour, mut seen) = (1u16, 0u16, false);
            for o in &self.objects {
                let spinner = matches!(o.kind, RawKind::Spinner { .. });
                if o.new_combo {
                    number = 1;
                    colour = colour.wrapping_add(o.combo_skip as u16);
                    if !spinner && seen { colour = colour.wrapping_add(1); }
                }
                practice_combos.push((number, colour));
                if !spinner {
                    seen = true;
                    number = number.saturating_add(1);
                }
            }
        }
        let mut indices: Vec<usize> = (0..self.objects.len()).filter(|&i| !practice.no_spinners || !matches!(self.objects[i].kind, RawKind::Spinner { .. })).collect();
        indices.sort_by(|&a, &b| self.objects[a].time_ms.total_cmp(&self.objects[b].time_ms)); // stable sort
        let raw: Vec<&RawObject> = indices.iter().map(|&i| &self.objects[i]).collect();
        let times: Vec<f64> = raw.iter().map(|o| o.time_ms).collect();
        let timing = self.timing_at(&times);

        let geometry: Vec<Option<SliderGeometry>> = raw
            .iter()
            .zip(&timing)
            .map(|(o, &t)| {
                matches!(o.kind, RawKind::Slider { .. }).then(|| {
                    let mut g = self.slider_geometry(o, t);
                    for p in &mut g.path {
                        p[0] = flip_x(p[0]);
                        p[1] = flip_y(p[1]);
                    }
                    g
                })
            })
            .collect();

        // Stacking moves objects, so it comes before positions are final.
        let items: Vec<stacking::Item> = raw
            .iter()
            .zip(&geometry)
            .map(|(o, g)| {
                let pos = [flip_x(o.x as f64), flip_y(o.y as f64)];
                match (&o.kind, g) {
                    (RawKind::Slider { .. }, Some(g)) => stacking::Item {
                        kind: stacking::Kind::Slider,
                        start: o.time_ms,
                        end: o.time_ms + g.spans as f64 * g.span_us as f64 / 1000.0,
                        pos,
                        // An even number of passes ends back on the head.
                        end_pos: if g.spans % 2 == 1 { g.path.last().copied().unwrap_or(pos) } else { pos },
                    },
                    (RawKind::Spinner { end_ms }, _) => stacking::Item {
                        kind: stacking::Kind::Spinner,
                        start: o.time_ms,
                        end: *end_ms,
                        pos,
                        end_pos: pos,
                    },
                    _ => stacking::Item {
                        kind: stacking::Kind::Circle,
                        start: o.time_ms,
                        end: o.time_ms,
                        pos,
                        end_pos: pos,
                    },
                }
            })
            .collect();
        let ar_scale = if practice.ar_lock { rate } else { 1.0 };
        let preempt_ms = difficulty_range(difficulty.ar, 1800.0, 1200.0, 450.0) as f64 * ar_scale;
        let heights = stacking::stack_heights(&items, preempt_ms, self.stack_leniency as f64, self.format_version);
        let stack_step = -6.4 * object_scale(difficulty.cs) as f64;

        let mut builder = ChartBuilder::new(difficulty);
        builder.score_difficulty(self.difficulty).breaks(&self.breaks);
        let (mut number, mut colour) = (0u16, 0u16);
        let mut force_new_combo = true; // the first object, and any object after a spinner
        for (n, o) in raw.iter().enumerate() {
            let shift = heights[n] as f64 * stack_step;
            let fx = |p: curve::P| (to_fx((p[0] + if practice.flip_left_right { -shift } else { shift }) as f32), to_fx((p[1] + if practice.flip_up_down { -shift } else { shift }) as f32));
            let t = to_us(o.time_ms);
            let is_spinner = matches!(o.kind, RawKind::Spinner { .. });
            if o.new_combo || force_new_combo {
                if n > 0 {
                    colour = colour.wrapping_add(1 + o.combo_skip as u16);
                }
                number = 0;
            }
            force_new_combo = is_spinner;
            number = number.saturating_add(1);
            let object = match (&o.kind, &geometry[n]) {
                (RawKind::Slider { .. }, Some(g)) => {
                    let points: Vec<_> = g.path.iter().map(|&p| fx(p)).collect();
                    builder.slider(t, &points, g.spans, g.span_us, &g.nested)
                }
                (RawKind::Spinner { end_ms }, _) => builder.spinner(t, to_us(*end_ms)),
                _ => {
                    let (x, y) = fx([flip_x(o.x as f64), flip_y(o.y as f64)]);
                    builder.circle(t, x, y)
                }
            };
            (object.combo_number, object.combo_index) = if practice.no_spinners {
                practice_combos[indices[n]]
            } else {
                (number, colour)
            };
        }
        let mut chart = builder.build();
        // The transport already advances the game clock at `rate`. Scale the
        // native durations, not AR/OD: an inverse difficulty conversion would
        // re-quantize or clamp them and could only preserve one OD window.
        let scale = |us| (us as f64 * rate).round() as i64;
        if practice.ar_lock {
            chart.preempt = scale(chart.preempt);
            chart.fade_in = scale(chart.fade_in);
        }
        if practice.od_lock {
            chart.window_300 = scale(chart.window_300);
            chart.window_100 = scale(chart.window_100);
            chart.window_50 = scale(chart.window_50);
        }
        chart
    }
}

/// Optional absolute difficulty overrides for an unranked practice run.
/// `Some(0.0)` is an override; only `None` leaves the value to the mods.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PracticeDifficulty {
    pub cs: Option<f32>,
    pub ar: Option<f32>,
    pub od: Option<f32>,
    pub hp: Option<f32>,
    pub ar_lock: bool,
    pub od_lock: bool,
    pub flip_left_right: bool,
    pub flip_up_down: bool,
    pub fading_cursor: bool,
    pub no_spinners: bool,
    pub no_50s: bool,
}

impl PracticeDifficulty {
    /// The largest tenth that still gives a positive circle radius.
    pub const CS_MAX: f32 = 12.1;
    pub const HP_MAX: f32 = 12.5;

    pub fn is_active(self) -> bool {
        self.cs.is_some() || self.ar.is_some() || self.od.is_some() || self.hp.is_some() || self.ar_lock || self.od_lock || self.flip_left_right || self.flip_up_down || self.fading_cursor || self.no_spinners || self.no_50s
    }

    /// Apply HR/EZ first, then replace only the overridden values.
    pub fn apply(self, base: Difficulty, game_mods: u32) -> Difficulty {
        let Difficulty { cs, ar, od, hp } = rsu_core::mods::difficulty(base, game_mods);
        let value = |override_value: Option<f32>, fallback, max| {
            override_value.filter(|v| v.is_finite()).map_or(fallback, |v| v.clamp(0.0, max))
        };
        Difficulty {
            cs: value(self.cs, cs, Self::CS_MAX),
            ar: value(self.ar, ar, 10.0),
            od: value(self.od, od, 10.0),
            hp: value(self.hp, hp, Self::HP_MAX),
        }
    }
}

/// Read and parse a `.osu` file. Also returns the MD5 of its bytes, which is
/// how osu! (and our replays) identify a beatmap.
pub fn load(path: &std::path::Path) -> Result<(Beatmap, [u8; 16]), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let map = parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((map, crate::md5::md5(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsu_core::chart::ObjectKind;
    use rsu_core::from_fx;

    const MAP: &str = "\u{feff}osu file format v14\r\n\r\n[General]\r\nAudioFilename: song.mp3\r\nAudioLeadIn: 500\r\n\
Mode: 0\r\nStackLeniency: 0.5\r\n\r\n[Metadata]\r\nTitle:Some: Title\r\nArtist:Someone\r\nCreator:Mapper\r\n\
Version:Insane\r\n\r\n[Difficulty]\r\nHPDrainRate:6\r\nCircleSize:4\r\nOverallDifficulty:8\r\nApproachRate:9.3\r\n\
SliderMultiplier:1.8\r\nSliderTickRate:2\r\n\r\n[Events]\r\n//Background\r\n0,0,\"bg.jpg\",0,0\r\n2,9490,17533\r\n\r\n\
[TimingPoints]\r\n148,285.714285714286,4,1,1,60,1,0\r\n9290,-100,4,1,1,70,0,1\r\n\r\n[HitObjects]\r\n\
256,192,1000,5,0,0:0:0:0:\r\n100,100,1500,1,0,0:0:0:0:\r\n295,120,2000,6,0,P|261:96|231:53,2,80,8|0,0:0|0:0,0:0:0:0:\r\n\
256,192,3000,12,0,7005,0:0:0:0:\r\n50,60,8000,1,0\r\nthis line is garbage\r\n10,20,9000,33,0\r\n";

    #[test]
    fn practice_cs_changes_hit_geometry_and_stacking_after_hr_and_ez() {
        use rsu_core::{Button, Game, Input, InputKind};
        use std::sync::Arc;
        let map = parse(b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n100,100,1100,1,0\n").unwrap();
        for mods in [rsu_core::mods::HARD_ROCK, rsu_core::mods::EASY] {
            let chart = |cs| Arc::new(map.to_chart_with_practice(mods, PracticeDifficulty { cs: Some(cs), ..Default::default() }));
            let (large, small) = (chart(0.0), chart(10.0));
            let normal = map.to_chart_with_mods(mods);
            assert_eq!((large.difficulty.cs, small.difficulty.cs), (0.0, 10.0));
            assert!(large.radius > normal.radius && normal.radius > small.radius);
            assert!(large.objects[0].x < small.objects[0].x);
            assert_eq!((large.objects[1].x, large.objects[1].y), (normal.objects[1].x, normal.objects[1].y));
            assert_eq!(large.score_multiplier, normal.score_multiplier);
            assert_eq!(small.score_multiplier, normal.score_multiplier);
            let run = |chart: Arc<Chart>| {
                let o = chart.objects[0];
                let input = [Input { t: 0, kind: InputKind::Move { x: o.x + to_fx(30.0), y: o.y } },
                    Input { t: o.t, kind: InputKind::Press(Button::K1) }];
                Game::simulate(chart, mods, &input, o.t + 400_000).n300
            };
            assert_eq!(run(large), 1);
            assert_eq!(run(small), 0);
        }
    }

    #[test]
    fn practice_hp_changes_live_drain_and_failure_without_changing_geometry_or_score_multiplier() {
        use rsu_core::Game;
        let objects = (0..8).map(|i| format!("100,100,{},1,0\n", 1000 + i * 800)).collect::<String>();
        let map = parse(format!("osu file format v14\n[HitObjects]\n{objects}").as_bytes()).unwrap();
        for mods in [rsu_core::mods::HARD_ROCK, rsu_core::mods::EASY] {
            let chart = |hp| map.to_chart_with_practice(mods, PracticeDifficulty { hp: Some(hp), ..Default::default() });
            let (low, high) = (chart(0.0), chart(12.5));
            let normal = map.to_chart_with_mods(mods);
            for chart in [&low, &high] {
                assert_eq!(chart.radius, normal.radius);
                assert_eq!(chart.preempt, normal.preempt);
                assert_eq!(chart.window_300, normal.window_300);
                assert_eq!(chart.score_multiplier, normal.score_multiplier);
                assert!(chart.objects.iter().zip(&normal.objects).all(|(a, b)| (a.x, a.y) == (b.x, b.y)));
            }
            assert_eq!((low.difficulty.hp, high.difficulty.hp), (0.0, 12.5));
            assert!(rsu_core::hp::drain_rate(&low) < rsu_core::hp::drain_rate(&high));
            let run = |chart| {
                let mut game = Game::with_mods(std::sync::Arc::new(chart), mods);
                game.advance_to(8_000_000, &mut |_| {});
                (game.hp(), game.failed())
            };
            let (low_hp, low_failed) = run(low);
            assert!(low_hp > 0.0 && low_failed.is_none());
            let (high_hp, high_failed) = run(high);
            assert_eq!(high_hp, 0.0);
            assert!(high_failed.is_some());
        }
    }

    #[test]
    fn practice_ar_changes_preempt_and_stacking_after_mods() {
        let text = "osu file format v14\n[General]\nStackLeniency:0.7\n[Difficulty]\nCircleSize:4\nOverallDifficulty:5\nApproachRate:5\nHPDrainRate:5\n[HitObjects]\n100,100,1000,1,0\n100,100,1700,1,0\n";
        let map = parse(text.as_bytes()).unwrap();
        for mods in [0, rsu_core::mods::HARD_ROCK, rsu_core::mods::EASY] {
            let fast = map.to_chart_with_practice(mods, PracticeDifficulty { ar: Some(10.0), ..Default::default() });
            let slow = map.to_chart_with_practice(mods, PracticeDifficulty { ar: Some(0.0), ..Default::default() });
            let normal = map.to_chart_with_mods(mods);
            assert_eq!(fast.preempt, 450_000);
            assert_eq!(slow.preempt, 1_800_000);
            assert_eq!(fast.difficulty.cs, normal.difficulty.cs);
            assert_eq!(fast.difficulty.hp, normal.difficulty.hp);
            assert_eq!(fast.difficulty.od, normal.difficulty.od);
            assert_eq!(fast.score_multiplier, normal.score_multiplier);
            assert_ne!(fast.objects[0].x, slow.objects[0].x);
        }
    }

    #[test]
    fn practice_od_is_independent_of_hard_rock() {
        let map = parse(MAP.as_bytes()).unwrap();
        let chart = map.to_chart_with_od(rsu_core::mods::HARD_ROCK, Some(5.0));
        assert_eq!(chart.difficulty.od, 5.0);
        let normal = map.to_chart_with_mods(rsu_core::mods::HARD_ROCK);
        assert_eq!(chart.difficulty.ar, normal.difficulty.ar);
        assert_eq!(chart.difficulty.cs, normal.difficulty.cs);
        assert_eq!(chart.difficulty.hp, normal.difficulty.hp);
        assert_eq!(chart.preempt, normal.preempt);
        assert_eq!(chart.radius, normal.radius);
        assert_eq!(chart.score_multiplier, normal.score_multiplier);
        assert!(chart.window_300 > normal.window_300);
        assert!(chart.objects.iter().zip(&normal.objects).all(|(a,b)| (a.x,a.y) == (b.x,b.y)));
        let spinner = chart.objects.iter().find(|o| o.kind == ObjectKind::Spinner).unwrap();
        let normal_spinner = normal.objects.iter().find(|o| o.kind == ObjectKind::Spinner).unwrap();
        assert!(spinner.extra < normal_spinner.extra);
    }

    #[test]
    fn practice_od_changes_actual_judgement() {
        use rsu_core::{Button, Game, Input, InputKind};
        use std::sync::Arc;
        let map = parse(MAP.as_bytes()).unwrap();
        let run = |od| {
            let chart = Arc::new(map.to_chart_with_od(0, od));
            let object = chart.objects[0];
            let inputs = [Input { t: 0, kind: InputKind::Move { x: object.x, y: object.y } },
                Input { t: object.t + 40_000, kind: InputKind::Press(Button::K1) }];
            Game::simulate(chart, 0, &inputs, object.t + 200_000)
        };
        assert_eq!(run(Some(0.0)).n300, 1);
        assert_eq!(run(Some(10.0)).n300, 0);
    }

    #[test]
    fn parses_a_typical_v14_file() {
        let m = parse(MAP.as_bytes()).unwrap();
        assert_eq!(m.format_version, 14);
        assert_eq!(m.audio_filename, "song.mp3");
        assert_eq!(m.audio_lead_in_ms, 500);
        assert_eq!(m.title, "Some: Title", "only the first colon separates");
        assert_eq!(m.version, "Insane");
        assert_eq!(m.difficulty, Difficulty { cs: 4.0, od: 8.0, ar: 9.3, hp: 6.0 });
        assert_eq!(m.slider_multiplier, 1.8);
        assert_eq!(m.breaks, vec![(9490, 17533)]);
        assert_eq!(m.timing_points.len(), 2);
        assert!(m.timing_points[0].uninherited && !m.timing_points[1].uninherited);
        assert_eq!(m.objects.len(), 6);
        assert_eq!(m.skipped_lines, 1);
        assert_eq!(
            m.objects[2].kind,
            RawKind::Slider {
                curve_type: 'P',
                points: vec![(261.0, 96.0), (231.0, 53.0)],
                slides: 2,
                length: 80.0,
                edge_sounds: vec![8, 0],
                edge_sets: vec![(0, 0), (0, 0)],
            }
        );
        assert_eq!(m.objects[0].sample, ObjectSample::default());
        assert_eq!(m.timing_points[1].volume, 70);
        assert_eq!((m.timing_points[0].sample_set, m.timing_points[0].sample_index), (1, 1));
        assert_eq!(m.objects[3].kind, RawKind::Spinner { end_ms: 7005.0 });
        assert_eq!(m.objects[5].combo_skip, 2, "type 33 = circle + skip 2 colours");
    }

    #[test]
    fn flip_up_down_mirrors_stacks_slider_and_hit_input_with_hr_and_horizontal() {
        use rsu_core::{Button, Game, Input, InputKind};
        use std::sync::Arc;
        let map = parse(b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n100,100,1100,1,0\n80,130,3000,2,0,B|160:30|220:180,2,180\n").unwrap();
        for mods in [0, rsu_core::mods::HARD_ROCK] {
            for horizontal in [false, true] {
                let base = PracticeDifficulty { flip_left_right: horizontal, ..Default::default() };
                let normal = map.to_chart_with_practice(mods, base);
                let flipped = map.to_chart_with_practice(mods, PracticeDifficulty { flip_up_down: true, ..base });
                for (a, b) in normal.objects.iter().zip(&flipped.objects) {
                    assert_eq!((a.x, 384 * 256 - a.y), (b.x, b.y));
                }
                let (a, b) = (normal.slider(&normal.objects[2]), flipped.slider(&flipped.objects[2]));
                for (a, b) in normal.slider_path(a).iter().zip(flipped.slider_path(b)) {
                    assert_eq!(a.x, b.x);
                    assert!((384 * 256 - a.y - b.y).abs() <= 2);
                    assert!((a.d - b.d).abs() <= 2);
                }
                let o = flipped.objects[0];
                let input = [Input { t: 0, kind: InputKind::Move { x: o.x, y: o.y } }, Input { t: o.t, kind: InputKind::Press(Button::K1) }];
                assert_eq!(Game::simulate(Arc::new(flipped), mods, &input, o.t + 100_000).n300, 1);
            }
        }
    }

    #[test]
    fn flip_left_right_mirrors_stacked_circles_and_slider_with_hr_and_hit_input() {
        use rsu_core::{Button, Game, Input, InputKind};
        use std::sync::Arc;
        let map = parse(b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n100,100,1100,1,0\n80,130,3000,2,0,B|160:30|220:180,2,180\n").unwrap();
        for mods in [0, rsu_core::mods::HARD_ROCK] {
            let normal = map.to_chart_with_mods(mods);
            let flipped = map.to_chart_with_practice(mods, PracticeDifficulty { flip_left_right: true, ..Default::default() });
            for (a, b) in normal.objects.iter().zip(&flipped.objects) {
                assert_eq!((512 * 256 - a.x, a.y), (b.x, b.y));
            }
            let (a, b) = (normal.slider(&normal.objects[2]), flipped.slider(&flipped.objects[2]));
            for (a, b) in normal.slider_path(a).iter().zip(flipped.slider_path(b)) {
                assert_eq!((512 * 256 - a.x, a.y, a.d), (b.x, b.y, b.d));
            }
            let object = flipped.objects[0];
            let input = [Input { t: 0, kind: InputKind::Move { x: object.x, y: object.y } }, Input { t: object.t, kind: InputKind::Press(Button::K1) }];
            assert_eq!(Game::simulate(Arc::new(flipped), mods, &input, object.t + 100_000).n300, 1);
        }
    }

    #[test]
    fn hard_rock_flips_and_easy_shrinks() {
        let m = parse(MAP.as_bytes()).unwrap();
        let plain = m.to_chart();
        let hr = m.to_chart_with_mods(rsu_core::mods::HARD_ROCK);
        let ez = m.to_chart_with_mods(rsu_core::mods::EASY);
        // The second circle (100, 100) stands alone: no stacking.
        assert_eq!((from_fx(hr.objects[1].x), from_fx(hr.objects[1].y)), (100.0, 284.0));
        // Slider paths are flipped too, point by point.
        let (s, f) = (plain.slider(&plain.objects[2]), hr.slider(&hr.objects[2]));
        for (a, b) in plain.slider_path(s).iter().zip(hr.slider_path(f)) {
            assert_eq!((a.x, 384 * 256 - a.y), (b.x, b.y));
        }
        assert!(hr.radius < plain.radius && ez.radius > plain.radius);
        assert!(hr.window_300 < plain.window_300 && ez.preempt > plain.preempt);
        // The score's difficulty multiplier stays the map's own.
        assert_eq!((hr.score_multiplier, ez.score_multiplier), (plain.score_multiplier, plain.score_multiplier));
    }

    #[test]
    fn no_spinners_preserves_mcosu_combo_flags_and_colour_skips() {
        let map = parse(b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n256,192,2000,44,0,2500\n100,100,3000,1,0\n256,192,4000,8,0,4500\n100,100,5000,1,0\n256,192,6000,28,0,6500\n256,192,7000,44,0,7500\n100,100,8000,5,0\n100,100,8100,1,0\n").unwrap();
        let chart = map.to_chart_with_practice(0, PracticeDifficulty { no_spinners: true, ..Default::default() });
        let combos: Vec<_> = chart.objects.iter().map(|o| (o.combo_number, o.combo_index)).collect();
        assert_eq!(combos, vec![(1, 0), (1, 2), (2, 2), (1, 6), (2, 6)]);
        // Filtering happens before stacking; compare with an otherwise identical map without spinners.
        let stripped = parse(b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n100,100,3000,1,0\n100,100,5000,1,0\n100,100,8000,5,0\n100,100,8100,1,0\n").unwrap().to_chart();
        assert_eq!(chart.objects.iter().map(|o| (o.x, o.y)).collect::<Vec<_>>(), stripped.objects.iter().map(|o| (o.x, o.y)).collect::<Vec<_>>());
    }

    #[test]
    fn chart_numbers_combos_like_the_game() {
        let c = parse(MAP.as_bytes()).unwrap().to_chart();
        let combos: Vec<_> = c.objects.iter().map(|o| (o.combo_number, o.combo_index)).collect();
        // new combo, +1, new combo (slider), spinner (new combo), then the
        // object after a spinner always starts a combo, then +1.
        assert_eq!(combos, vec![(1, 0), (2, 0), (1, 1), (1, 2), (1, 3), (2, 3)]);
        assert_eq!(c.objects[0].t, 1000 * MS);
        assert_eq!(c.objects[3].end, 7005 * MS);
        assert_eq!(c.objects[3].kind, ObjectKind::Spinner);
        assert_eq!(c.window_300, 31_500);
    }

    #[test]
    fn old_formats_get_their_quirks() {
        let old = "osu file format v4\n[Difficulty]\nOverallDifficulty:7\n[TimingPoints]\n100,300\n200,-50\n\
[HitObjects]\n10,10,1000,1,0\n";
        let m = parse(old.as_bytes()).unwrap();
        assert_eq!(m.difficulty.ar, 7.0, "AR falls back to OD");
        assert_eq!(m.objects[0].time_ms, 1024.0, "v4 and older are shifted by 24 ms");
        assert_eq!(m.timing_points[0].time_ms, 124.0);
        assert!(m.timing_points[0].uninherited && !m.timing_points[1].uninherited);
    }

    #[test]
    fn lazer_decimals_keep_their_precision() {
        let m = parse(b"osu file format v128\n[HitObjects]\n100.5,200.25,1000.75,1,0\n").unwrap();
        let c = m.to_chart();
        assert_eq!(c.objects[0].t, 1_000_750);
        assert_eq!(c.objects[0].x, to_fx(100.5));
    }

    /// 120 BPM (500 ms per beat), slider multiplier 1.4: 140 px per beat.
    fn slider_map(extra_timing: &str, objects: &str) -> Chart {
        let text = format!(
            "osu file format v14\n[Difficulty]\nCircleSize:4\nOverallDifficulty:8\nApproachRate:9\n\
SliderMultiplier:1.4\nSliderTickRate:1\n[TimingPoints]\n0,500,4,1,0,50,1,0\n{extra_timing}[HitObjects]\n{objects}"
        );
        parse(text.as_bytes()).unwrap().to_chart()
    }

    fn nested_of(c: &Chart, object: usize) -> Vec<(Time, NestedKind)> {
        c.slider_nested(c.slider(&c.objects[object])).iter().map(|n| (n.t, n.kind)).collect()
    }

    #[test]
    fn slider_duration_follows_tempo_and_velocity() {
        // 280 px at 140 px per beat = 2 beats = 1000 ms per pass, 2 passes.
        let c = slider_map("", "100,100,1000,2,0,L|380:100,2,280\n");
        let o = c.objects[0];
        assert_eq!(c.slider(&o).span_duration, 1_000_000);
        assert_eq!(o.end, 3_000_000);
        // Ticks every beat (140 px), but not on the end; a repeat; the tail
        // check 36 ms before the end.
        assert_eq!(
            nested_of(&c, 0),
            vec![
                (1_500_000, NestedKind::Tick),
                (2_000_000, NestedKind::Repeat),
                (2_500_000, NestedKind::Tick),
                (2_964_000, NestedKind::Tail),
            ]
        );
        // The tick on the way back is at the same place as the first one.
        let n = c.slider_nested(c.slider(&o));
        assert_eq!((n[0].x, n[0].y), (to_fx(240.0), to_fx(100.0)));
        assert_eq!((n[2].x, n[2].y), (to_fx(240.0), to_fx(100.0)));
        assert_eq!((n[1].x, n[1].y), (to_fx(380.0), to_fx(100.0)));

        // An inherited point of -50 doubles the velocity: half the duration,
        // and (from v8 on) ticks twice as far apart, so none fits.
        let c = slider_map("900,-50,4,1,0,50,0,0\n", "100,100,1000,2,0,L|380:100,1,280\n");
        assert_eq!(c.slider(&c.objects[0]).span_duration, 500_000);
        assert_eq!(nested_of(&c, 0), vec![(1_464_000, NestedKind::Tail)]);
        // A new tempo point resets the multiplier.
        let c = slider_map("900,-50,4,1,0,50,0,0\n950,250,4,1,0,50,1,0\n", "100,100,1000,2,0,L|380:100,1,280\n");
        assert_eq!(c.slider(&c.objects[0]).span_duration, 500_000, "250 ms beats at 1x");
        assert_eq!(nested_of(&c, 0).len(), 2, "one tick again");
    }

    #[test]
    fn nan_velocity_points_remove_ticks() {
        let c = slider_map("900,NaN,4,1,0,50,0,0\n", "100,100,1000,2,0,L|380:100,1,280\n");
        assert_eq!(c.slider(&c.objects[0]).span_duration, 1_000_000);
        assert_eq!(nested_of(&c, 0), vec![(1_964_000, NestedKind::Tail)]);
    }

    #[test]
    fn short_sliders_judge_the_tail_at_the_midpoint() {
        // 14 px = 50 ms: 36 ms before the end would be before the middle.
        let c = slider_map("", "100,100,1000,2,0,L|114:100,1,14\n");
        assert_eq!(nested_of(&c, 0), vec![(1_025_000, NestedKind::Tail)]);
    }

    #[test]
    fn stacked_objects_are_shifted_including_slider_paths() {
        // CS 4: scale 0.57, so one stack step is 3.648 px up-left.
        let step = 6.4 * 0.57;
        let c = slider_map("", "100,100,1000,1,0\n100,100,1200,1,0\n100,100,1400,2,0,L|240:100,1,140\n");
        let at = |i: usize| (from_fx(c.objects[i].x), from_fx(c.objects[i].y));
        assert!((at(0).0 - (100.0 - 2.0 * step)).abs() < 0.01 && (at(0).1 - (100.0 - 2.0 * step)).abs() < 0.01);
        assert!((at(1).0 - (100.0 - step)).abs() < 0.01);
        assert_eq!(at(2), (100.0, 100.0));
        // A circle under a slider's end goes the other way, and the slider
        // (stack 0) keeps its path.
        let c = slider_map("", "100,100,1000,2,0,L|240:100,1,140\n240,100,1700,1,0\n");
        let o = c.objects[1];
        assert!((from_fx(o.x) - (240.0 + step)).abs() < 0.01 && (from_fx(o.y) - (100.0 + step)).abs() < 0.01);
        assert_eq!(c.slider_end_position(&c.objects[0]), (to_fx(240.0), to_fx(100.0)));
    }

    #[test]
    fn hostile_slider_values_stay_bounded() {
        // Zero-length path, zero tempo, a million repeats, a microscopic tick distance.
        let text = "osu file format v14\n[Difficulty]\nSliderMultiplier:0.0000001\nSliderTickRate:1000000\n\
[TimingPoints]\n0,0,4,1,0,50,1,0\n[HitObjects]\n10,10,1000,2,0,L|10:10,1000000,0\n\
10,10,2000,2,0,B|500:10|10:300|500:300,3,100000\n";
        let c = parse(text.as_bytes()).unwrap().to_chart();
        assert_eq!(c.objects.len(), 2);
        assert!(c.nested.len() <= 3 * MAX_NESTED);
        assert!(c.objects.iter().all(|o| o.end >= o.t));
    }

    #[test]
    fn nan_slider_velocity_points_are_kept() {
        let m = parse(
            b"osu file format v14\n[TimingPoints]\n0,300,4,1,0,50,1,0\n500,NaN,4,2,0,50,0,8\n600,NaN,4,2,0,50,1,0\n",
        )
        .unwrap();
        assert_eq!(m.timing_points.len(), 2);
        assert!(m.timing_points[1].beat_length.is_nan() && !m.timing_points[1].uninherited);
        assert_eq!(m.skipped_lines, 1, "a NaN tempo is still rejected");
    }

    #[test]
    fn rejects_what_it_cannot_play() {
        assert_eq!(parse(b"hello").unwrap_err(), ParseError::NotABeatmap);
        assert_eq!(parse(b"").unwrap_err(), ParseError::NotABeatmap);
        assert_eq!(parse(b"osu file format v14\n[General]\nMode: 3\n").unwrap_err(), ParseError::UnsupportedMode(3));
    }

    #[test]
    fn unsorted_objects_are_sorted_stably() {
        let m =
            parse(b"osu file format v14\n[HitObjects]\n10,10,2000,1,0\n200,20,1000,1,0\n300,30,2000,1,0\n").unwrap();
        let c = m.to_chart();
        let xs: Vec<_> = c.objects.iter().map(|o| from_fx(o.x)).collect();
        assert_eq!(xs, vec![200.0, 10.0, 300.0]);
    }
}
