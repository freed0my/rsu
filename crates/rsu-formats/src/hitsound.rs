//! Which sounds a beatmap asks for.
//!
//! In osu! a hit sound is a base sample plus optional additions (whistle,
//! finish, clap), each taken from one of three sample sets (normal, soft,
//! drum), possibly from the mapper's own numbered files, at a volume set by
//! the timing section. Objects can override every part of that. This module
//! turns all of it into a plain list of samples per object edge; finding
//! and playing the files is someone else's job.
//!
//! There are two ways of reading a map. [`Style::Stable`] is the full
//! scheme above. [`Style::McOsu`] is what the McOsu client does, ported
//! from its source (`OsuSkin::playHitCircleSound` and its callers): only the
//! skin's samples, the sample set and volume of the timing section alone,
//! and fixed levels for the base sound, whistle and clap.

use crate::osu::{Beatmap, ObjectSample, RawKind, RawObject, TimingPoint};
use rsu_core::chart::{Chart, NestedKind, ObjectKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SoundKind {
    Normal,
    Whistle,
    Finish,
    Clap,
    SliderTick,
}

/// One sample file's identity. `index` 0 is the skin's sample; 1 and up are
/// the beatmap's own (1 has no number in its file name).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SampleKey {
    /// 1 normal, 2 soft, 3 drum.
    pub set: u8,
    pub kind: SoundKind,
    pub index: u32,
}

/// What plays at one moment: up to four layered samples at one volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sound {
    pub layers: [Option<SampleKey>; 4],
    /// Each layer's own level in percent, on top of `volume`.
    pub levels: [u8; 4],
    /// 0..=100.
    pub volume: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectSounds {
    /// Circles and spinners: one entry. Sliders: head, each repeat, tail.
    pub edges: Vec<Sound>,
    /// Sliders: each tick's time (microseconds) and sound, in order.
    pub ticks: Vec<(i64, Sound)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// As osu! itself reads a map, beatmap samples included.
    Stable,
    /// As McOsu plays it.
    McOsu,
}

/// The sample set and volume in force over time, for sounds that follow
/// the moment rather than an object (a held slider, a spinner).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sections {
    /// (from this time in microseconds, sample set 1..=3, volume 0..=100),
    /// in order; the first entry also covers everything before it.
    pub points: Vec<(i64, u8, u8)>,
}

impl Sections {
    pub fn at(&self, t: i64) -> (u8, u8) {
        let n = self.points.partition_point(|p| p.0 <= t + (LENIENCY_MS * 1000.0) as i64);
        match n.checked_sub(1).map(|i| self.points[i]).or(self.points.first().copied()) {
            Some((_, set, volume)) => (set, volume),
            None => (1, 100),
        }
    }
}

/// Sample settings change a hair before the object they are meant for, so
/// lookups reach this far ahead, as the game does.
const LENIENCY_MS: f64 = 5.0;

struct Timeline<'a> {
    points: Vec<&'a TimingPoint>,
    default_set: u8,
    style: Style,
}

impl Timeline<'_> {
    /// (sample set, custom index, volume) in force at `ms`.
    fn at(&self, ms: f64) -> (u8, u32, u8) {
        let n = self.points.partition_point(|p| p.time_ms <= ms + LENIENCY_MS);
        match n.checked_sub(1).map(|i| self.points[i]).or(self.points.first().copied()) {
            Some(p) => (if p.sample_set == 0 { self.default_set } else { p.sample_set }, p.sample_index, p.volume),
            None => (self.default_set, 0, 100),
        }
    }

    fn sound(&self, ms: f64, bits: u8, sets: (u8, u8), o: &ObjectSample) -> Sound {
        let (timing_set, timing_index, timing_volume) = self.at(ms);
        if self.style == Style::McOsu {
            // The section's set for everything, the skin's files only, and
            // McOsu's fixed levels: base 80%, whistle and clap 85%.
            let set = if matches!(timing_set, 2 | 3) { timing_set } else { 1 };
            let key = |kind| Some(SampleKey { set, kind, index: 0 });
            let mut sound = Sound { layers: [None; 4], levels: [80, 85, 100, 85], volume: timing_volume.min(100) };
            sound.layers[0] = key(SoundKind::Normal);
            for (slot, (bit, kind)) in
                [(2, SoundKind::Whistle), (4, SoundKind::Finish), (8, SoundKind::Clap)].into_iter().enumerate()
            {
                if bits & bit != 0 {
                    sound.layers[slot + 1] = key(kind);
                }
            }
            return sound;
        }
        let normal_set = [sets.0, o.normal_set, timing_set].into_iter().find(|s| *s != 0).unwrap_or(1);
        // Additions follow the base sound's set unless given their own.
        let addition_set = [sets.1, o.addition_set, normal_set].into_iter().find(|s| *s != 0).unwrap_or(1);
        let index = if o.index != 0 { o.index } else { timing_index };
        let mut layers = [None; 4];
        layers[0] = Some(SampleKey { set: normal_set, kind: SoundKind::Normal, index });
        for (slot, (bit, kind)) in
            [(2, SoundKind::Whistle), (4, SoundKind::Finish), (8, SoundKind::Clap)].iter().enumerate()
        {
            if bits & bit != 0 {
                layers[slot + 1] = Some(SampleKey { set: addition_set, kind: *kind, index });
            }
        }
        Sound { layers, levels: [100; 4], volume: if o.volume != 0 { o.volume } else { timing_volume } }
    }

    /// A slider tick at `ms`.
    fn tick(&self, ms: f64, own: &Sound) -> Sound {
        let base = match self.style {
            // McOsu: whatever section the tick itself falls in.
            Style::McOsu => {
                let set = self.at(ms).0;
                SampleKey { set: if matches!(set, 2 | 3) { set } else { 1 }, kind: SoundKind::SliderTick, index: 0 }
            }
            Style::Stable => SampleKey { kind: SoundKind::SliderTick, ..own.layers[0].unwrap() },
        };
        let volume = if self.style == Style::McOsu { self.at(ms).2.min(100) } else { own.volume };
        Sound { layers: [Some(base), None, None, None], levels: [100; 4], volume }
    }
}

fn timeline(map: &Beatmap, style: Style) -> Timeline<'_> {
    let mut points: Vec<&TimingPoint> = map.timing_points.iter().collect();
    points.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));
    // McOsu knows nothing of the map's default set: anything that is not
    // soft or drum is normal.
    let default_set = if style == Style::McOsu { 1 } else { map.sample_set.max(1) };
    Timeline { points, default_set, style }
}

/// The sample set and volume of each timing section.
pub fn sections(map: &Beatmap, style: Style) -> Sections {
    let timeline = timeline(map, style);
    let set = |p: &TimingPoint| match (style, p.sample_set) {
        (_, 0) => timeline.default_set,
        (Style::McOsu, s) if s > 3 => 1,
        (_, s) => s,
    };
    Sections {
        points: timeline.points.iter().map(|p| ((p.time_ms * 1000.0) as i64, set(p), p.volume.min(100))).collect(),
    }
}

/// Sounds for every object of `chart`, in the chart's order. `chart` must
/// have been built from `map`.
pub fn resolve(map: &Beatmap, chart: &Chart, style: Style) -> Vec<ObjectSounds> {
    let timeline = timeline(map, style);
    // The chart sorted the objects the same way: stably, by time.
    let mut raw: Vec<&RawObject> = map.objects.iter().collect();
    raw.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));

    raw.iter()
        .zip(&chart.objects)
        .map(|(r, o)| {
            let start = o.t as f64 / 1000.0;
            let own = timeline.sound(start, r.hit_sound, (0, 0), &r.sample);
            let mut ticks = Vec::new();
            let edges = match (&r.kind, o.kind) {
                (RawKind::Slider { edge_sounds, edge_sets, .. }, ObjectKind::Slider) => {
                    let s = chart.slider(o);
                    let nested = chart.slider_nested(s).iter().filter(|n| n.kind == NestedKind::Tick);
                    ticks = nested.map(|n| (n.t, timeline.tick(n.t as f64 / 1000.0, &own))).collect();
                    (0..=s.spans as usize)
                        .map(|k| {
                            let ms = (o.t + k as i64 * s.span_duration) as f64 / 1000.0;
                            // Edges without their own entry sound like the slider itself.
                            let bits = edge_sounds.get(k).copied().unwrap_or(r.hit_sound);
                            let sets = edge_sets.get(k).copied().unwrap_or((0, 0));
                            timeline.sound(ms, bits, sets, &r.sample)
                        })
                        .collect()
                }
                // A spinner sounds when it ends.
                (RawKind::Spinner { .. }, _) => {
                    vec![timeline.sound(o.end as f64 / 1000.0, r.hit_sound, (0, 0), &r.sample)]
                }
                _ => vec![own],
            };
            ObjectSounds { edges, ticks }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osu::parse;

    fn sounds(general: &str, timing: &str, objects: &str) -> Vec<ObjectSounds> {
        let text = format!(
            "osu file format v14\n[General]\n{general}[Difficulty]\nSliderMultiplier:1.4\n[TimingPoints]\n{timing}[HitObjects]\n{objects}"
        );
        let map = parse(text.as_bytes()).unwrap();
        let chart = map.to_chart();
        resolve(&map, &chart, Style::Stable)
    }

    fn mcosu(general: &str, timing: &str, objects: &str) -> (Vec<ObjectSounds>, Sections) {
        let text = format!(
            "osu file format v14\n[General]\n{general}[Difficulty]\nSliderMultiplier:1.4\n[TimingPoints]\n{timing}[HitObjects]\n{objects}"
        );
        let map = parse(text.as_bytes()).unwrap();
        let chart = map.to_chart();
        (resolve(&map, &chart, Style::McOsu), sections(&map, Style::McOsu))
    }

    #[test]
    fn mcosu_takes_set_and_volume_from_the_section_and_nothing_from_the_object() {
        // The map says soft by default and the object asks for drum
        // additions from custom set 3 at volume 55. McOsu hears none of it:
        // the section has no set of its own, so normal; skin files; the
        // section's volume; base 80%, finish 100%, clap 85%.
        let (s, sections) = mcosu(
            "SampleSet: Soft\n",
            "0,500,4,0,1,70,1,0\n1900,-100,4,3,2,40,0,0\n2900,-100,4,0,0,130,0,0\n",
            "10,10,1000,1,12,1:3:3:55:\n10,10,2000,1,2,0:0:0:0:\n10,10,3000,1,0\n",
        );
        assert_eq!(
            s[0].edges[0],
            Sound {
                layers: [key(1, SoundKind::Normal, 0), None, key(1, SoundKind::Finish, 0), key(1, SoundKind::Clap, 0)],
                levels: [80, 85, 100, 85],
                volume: 70
            }
        );
        assert_eq!(s[1].edges[0].layers, [key(3, SoundKind::Normal, 0), key(3, SoundKind::Whistle, 0), None, None]);
        assert_eq!(s[1].edges[0].volume, 40);
        // No set is normal again; volume stops at 100.
        assert_eq!((s[2].edges[0].layers[0], s[2].edges[0].volume), (key(1, SoundKind::Normal, 0), 100));
        assert_eq!(sections.points, [(0, 1, 70), (1_900_000, 3, 40), (2_900_000, 1, 100)]);
        assert_eq!(sections.at(1_000_000), (1, 70));
        assert_eq!(sections.at(1_896_000), (3, 40), "4 ms early is within the leniency");
        assert_eq!(sections.at(-5_000_000), (1, 70));
    }

    #[test]
    fn mcosu_slider_ticks_sound_like_the_section_they_fall_in() {
        // 280 px at 140 px/beat: one pass of 1000 ms with a tick in the
        // middle, which is already in the drum section; the head is not.
        let (s, _) =
            mcosu("", "0,500,4,2,0,100,1,0\n1400,-100,4,3,0,60,0,0\n", "100,100,1000,2,0,L|380:100,1,280,2|8\n");
        assert_eq!(s[0].edges.len(), 2);
        assert_eq!(s[0].edges[0].layers[..2], [key(2, SoundKind::Normal, 0), key(2, SoundKind::Whistle, 0)]);
        assert_eq!(s[0].edges[1].layers, [key(3, SoundKind::Normal, 0), None, None, key(3, SoundKind::Clap, 0)]);
        assert_eq!(s[0].ticks.len(), 1);
        let (t, tick) = s[0].ticks[0];
        assert_eq!(t, 1_500_000);
        assert_eq!((tick.layers[0], tick.levels[0], tick.volume), (key(3, SoundKind::SliderTick, 0), 100, 60));
    }

    fn key(set: u8, kind: SoundKind, index: u32) -> Option<SampleKey> {
        Some(SampleKey { set, kind, index })
    }

    #[test]
    fn timing_sections_set_the_defaults() {
        // Soft set from the map; a later point switches to drum, custom
        // index 2 at 40% volume.
        let s = sounds(
            "SampleSet: Soft\n",
            "0,500,4,0,0,80,1,0\n1900,-100,4,3,2,40,0,0\n",
            "10,10,1000,1,0,0:0:0:0:\n10,10,2000,1,2,0:0:0:0:\n",
        );
        assert_eq!(
            s[0].edges[0],
            Sound { layers: [key(2, SoundKind::Normal, 0), None, None, None], levels: [100; 4], volume: 80 }
        );
        assert_eq!(
            s[1].edges[0],
            Sound {
                layers: [key(3, SoundKind::Normal, 2), key(3, SoundKind::Whistle, 2), None, None],
                levels: [100; 4],
                volume: 40
            }
        );
    }

    #[test]
    fn a_point_slightly_after_the_object_still_applies() {
        let s = sounds("", "0,500,4,1,0,100,1,0\n1004,-100,4,2,0,30,0,0\n", "10,10,1000,1,0\n10,10,990,1,0\n");
        assert_eq!(s[1].edges[0].volume, 30, "4 ms late is within the leniency");
        assert_eq!(s[0].edges[0].volume, 100, "14 ms late is not");
    }

    #[test]
    fn objects_override_set_addition_index_and_volume() {
        // Normal base, drum additions, custom index 3, volume 55; finish + clap.
        let s = sounds("", "0,500,4,2,1,100,1,0\n", "10,10,1000,1,12,1:3:3:55:\n");
        assert_eq!(
            s[0].edges[0],
            Sound {
                layers: [key(1, SoundKind::Normal, 3), None, key(3, SoundKind::Finish, 3), key(3, SoundKind::Clap, 3)],
                levels: [100; 4],
                volume: 55
            }
        );
    }

    #[test]
    fn slider_edges_have_their_own_sounds_and_ticks_their_own_sample() {
        // 140 px at 140 px/beat: 500 ms per pass, 2 passes. Head: whistle in
        // the soft set; repeat: nothing extra; tail: clap, drum additions.
        let s = sounds(
            "",
            "0,500,4,1,0,100,1,0\n1400,-100,4,3,0,60,0,0\n",
            "100,100,1000,2,0,L|240:100,2,140,2|0|8,2:0|0:0|0:3,0:0:0:0:\n",
        );
        let e = &s[0].edges;
        assert_eq!(e.len(), 3);
        assert_eq!(
            e[0],
            Sound {
                layers: [key(2, SoundKind::Normal, 0), key(2, SoundKind::Whistle, 0), None, None],
                levels: [100; 4],
                volume: 100
            }
        );
        // The repeat (1500 ms) and the tail (2000 ms) fall in the drum section.
        assert_eq!(
            e[1],
            Sound { layers: [key(3, SoundKind::Normal, 0), None, None, None], levels: [100; 4], volume: 60 }
        );
        assert_eq!(
            e[2],
            Sound {
                layers: [key(3, SoundKind::Normal, 0), None, None, key(3, SoundKind::Clap, 0)],
                levels: [100; 4],
                volume: 60
            }
        );
        assert!(s[0].ticks.is_empty(), "a pass of one beat has no tick");
    }

    #[test]
    fn old_files_without_sound_fields_get_plain_sounds() {
        let s = sounds("", "0,500\n", "10,10,1000,1,0\n100,100,2000,2,0,L|240:100,1,140\n256,192,3000,12,4,4000\n");
        assert_eq!(
            s[0].edges[0],
            Sound { layers: [key(1, SoundKind::Normal, 0), None, None, None], levels: [100; 4], volume: 100 }
        );
        assert_eq!(s[1].edges.len(), 2);
        assert_eq!(s[2].edges[0].layers[2], key(1, SoundKind::Finish, 0), "spinner with a finish");
    }
}
