//! Hit sound samples: finding the files a beatmap or skin provides, and
//! synthesised stand-ins for when there are none.
//!
//! Everything is loaded before play starts; the mixer only ever sees a
//! finished table of buffers.

use crate::{decode, SAMPLE_RATE};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SoundKind {
    Normal,
    Whistle,
    Finish,
    Clap,
    SliderTick,
    /// Loops while a slider is being held.
    SliderSlide,
    /// Loops while a spinner is being spun.
    SpinnerSpin,
    SpinnerBonus,
    ComboBreak,
}

impl SoundKind {
    fn file_stem(self) -> &'static str {
        match self {
            SoundKind::Normal => "hitnormal",
            SoundKind::Whistle => "hitwhistle",
            SoundKind::Finish => "hitfinish",
            SoundKind::Clap => "hitclap",
            SoundKind::SliderTick => "slidertick",
            SoundKind::SliderSlide => "sliderslide",
            SoundKind::SpinnerSpin => "spinnerspin",
            SoundKind::SpinnerBonus => "spinnerbonus",
            SoundKind::ComboBreak => "combobreak",
        }
    }

    /// Whether there is one such sound per sample set (as opposed to one
    /// for the whole skin).
    fn per_set(self) -> bool {
        !matches!(self, SoundKind::SpinnerSpin | SoundKind::SpinnerBonus | SoundKind::ComboBreak)
    }
}

/// A folder's files by lower-cased name: sample names come from a
/// case-insensitive world.
#[derive(Default)]
pub struct Folder {
    files: HashMap<String, PathBuf>,
}

impl Folder {
    pub fn open(dir: &Path) -> Folder {
        let files = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| (e.file_name().to_string_lossy().to_lowercase(), e.path()))
            .collect();
        Folder { files }
    }

    fn find(&self, stem: &str) -> Option<&PathBuf> {
        // The order osu! clients try them in.
        ["wav", "mp3", "ogg"].iter().find_map(|ext| self.files.get(&format!("{stem}.{ext}")))
    }
}

/// The table of loaded samples, each interleaved stereo at [`SAMPLE_RATE`].
#[derive(Default)]
pub struct SampleBank {
    samples: Vec<Arc<[f32]>>,
    ids: HashMap<(u8, SoundKind, u32), u16>,
    pub from_beatmap: u32,
    pub from_skin: u32,
    pub synthesised: u32,
}

impl SampleBank {
    pub fn new() -> SampleBank {
        SampleBank::default()
    }

    /// The id of a sample, loading it on first use. `set` is 1 normal,
    /// 2 soft, 3 drum; `index` 0 asks for the skin's sample, 1 and up for
    /// the beatmap's own file (falling back to the skin if it has none).
    pub fn id(&mut self, set: u8, kind: SoundKind, index: u32, beatmap: &Folder, skin: Option<&Folder>) -> u16 {
        if let Some(&id) = self.ids.get(&(set, kind, index)) {
            return id;
        }
        let set_name = ["normal", "normal", "soft", "drum"][set.min(3) as usize];
        let stem = |number: &str| match kind.per_set() {
            true => format!("{set_name}-{}{number}", kind.file_stem()),
            false => kind.file_stem().to_string(),
        };
        let custom = match index {
            0 => None,
            1 => beatmap.find(&stem("")),
            n => beatmap.find(&stem(&n.to_string())),
        };
        let found = custom.or_else(|| skin.and_then(|s| s.find(&stem(""))));
        let data: Arc<[f32]> = match found {
            // A file that exists but holds no audio is how mappers silence
            // a sound; that is respected, not papered over.
            Some(path) => {
                if custom.is_some() {
                    self.from_beatmap += 1;
                } else {
                    self.from_skin += 1;
                }
                decode::load(path).unwrap_or_default().into()
            }
            None => {
                let made = synthesise(set, kind);
                self.synthesised += u32::from(!made.is_empty());
                made.into()
            }
        };
        // A sample too short to be a sound (skins use such stubs to
        // silence something) is silence.
        let data: Arc<[f32]> = match data.len() {
            0..=3 => Arc::new([]),
            _ => data,
        };
        let id = self.samples.len().min(u16::MAX as usize) as u16;
        self.samples.push(data);
        self.ids.insert((set, kind, index), id);
        id
    }

    /// The id of a sound the skin has under a name of its own ("menuhit");
    /// silence where it has none. Every call adds an entry.
    pub fn named(&mut self, stem: &str, skin: Option<&Folder>) -> u16 {
        let found = skin.and_then(|s| s.find(stem)).and_then(|path| decode::load(path).ok());
        self.from_skin += u32::from(found.is_some());
        let id = self.samples.len().min(u16::MAX as usize) as u16;
        self.samples.push(found.filter(|data| data.len() > 3).map_or_else(|| Arc::new([]) as Arc<[f32]>, Into::into));
        id
    }

    pub fn into_table(self) -> Arc<[Arc<[f32]>]> {
        self.samples.into()
    }
}

/// Built-in sounds, so the game is never mute. They are placeholders made
/// from sine bursts and noise, not anyone's samples.
fn synthesise(set: u8, kind: SoundKind) -> Vec<f32> {
    // (tone Hz, tone level, noise level, decay seconds, length seconds)
    let (tone, tone_level, noise_level, decay, length) = match kind {
        SoundKind::Normal => (1500.0, 0.7, 0.25, 0.012, 0.07),
        SoundKind::Whistle => (2300.0, 0.5, 0.0, 0.060, 0.25),
        SoundKind::Finish => (4200.0, 0.1, 0.55, 0.180, 0.70),
        SoundKind::Clap => (900.0, 0.2, 0.75, 0.035, 0.16),
        SoundKind::SliderTick => (3200.0, 0.4, 0.0, 0.006, 0.03),
        // No stand-ins for the sounds a game can do without.
        SoundKind::SliderSlide | SoundKind::SpinnerSpin | SoundKind::SpinnerBonus | SoundKind::ComboBreak => {
            return Vec::new()
        }
    };
    // Soft is quieter and duller, drum lower and thicker.
    let (pitch, level) = match set {
        2 => (0.8, 0.6),
        3 => (0.45, 1.0),
        _ => (1.0, 0.85),
    };
    let frames = (length * SAMPLE_RATE as f32) as usize;
    let mut seed = 0x9e37_79b9u32 ^ ((set as u32) << 8) ^ kind as u32;
    let mut out = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / SAMPLE_RATE as f32;
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (seed >> 8) as f32 / (1 << 23) as f32 - 1.0;
        let envelope = (-t / decay).exp();
        let s =
            ((std::f32::consts::TAU * tone * pitch * t).sin() * tone_level + noise * noise_level) * envelope * level;
        out.push(s);
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsu-bank-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal 16-bit mono WAV holding `samples`.
    fn wav(samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes()); // PCM
        f.extend_from_slice(&1u16.to_le_bytes()); // mono
        f.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        f.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&data);
        f
    }

    #[test]
    fn lookup_order_is_beatmap_then_skin_then_built_in() {
        let (map_dir, skin_dir) = (temp_dir("map"), temp_dir("skin"));
        // Mixed case on purpose: lookups must not care.
        std::fs::write(map_dir.join("Soft-HitClap2.WAV"), wav(&[1000; 480])).unwrap();
        std::fs::write(map_dir.join("normal-hitnormal.wav"), wav(&[2000; 240])).unwrap();
        std::fs::write(skin_dir.join("soft-hitclap.wav"), wav(&[3000; 96])).unwrap();
        let (map, skin) = (Folder::open(&map_dir), Folder::open(&skin_dir));
        let mut bank = SampleBank::new();

        let custom2 = bank.id(2, SoundKind::Clap, 2, &map, Some(&skin));
        let custom1 = bank.id(1, SoundKind::Normal, 1, &map, Some(&skin));
        let from_skin = bank.id(2, SoundKind::Clap, 0, &map, Some(&skin));
        let missing_custom = bank.id(2, SoundKind::Clap, 7, &map, Some(&skin)); // no clap7: skin's
        let built_in = bank.id(3, SoundKind::Finish, 0, &map, Some(&skin));
        assert_eq!(bank.id(2, SoundKind::Clap, 2, &map, Some(&skin)), custom2, "loaded once");
        assert_eq!((bank.from_beatmap, bank.from_skin, bank.synthesised), (2, 2, 1));

        let table = bank.into_table();
        // Lengths identify which file each came from (stereo frames).
        assert_eq!(table[custom2 as usize].len(), 480 * 2);
        assert_eq!(table[custom1 as usize].len(), 240 * 2);
        assert_eq!(table[from_skin as usize].len(), 96 * 2);
        assert_eq!(table[missing_custom as usize].len(), 96 * 2);
        assert!(table[built_in as usize].len() > 10_000);
        let _ = std::fs::remove_dir_all(map_dir);
        let _ = std::fs::remove_dir_all(skin_dir);
    }

    #[test]
    fn an_empty_or_broken_file_means_silence() {
        let map_dir = temp_dir("silent");
        std::fs::write(map_dir.join("normal-hitnormal.wav"), b"").unwrap();
        std::fs::write(map_dir.join("normal-hitclap.wav"), wav(&[])).unwrap();
        let map = Folder::open(&map_dir);
        let mut bank = SampleBank::new();
        let a = bank.id(1, SoundKind::Normal, 1, &map, None);
        let b = bank.id(1, SoundKind::Clap, 1, &map, None);
        assert_eq!(bank.synthesised, 0, "the mapper's silence must not be replaced");
        let table = bank.into_table();
        assert!(table[a as usize].is_empty() && table[b as usize].is_empty());
        let _ = std::fs::remove_dir_all(map_dir);
    }

    #[test]
    fn built_in_sounds_are_audible_bounded_and_distinct() {
        let mut seen = Vec::new();
        for set in 1..=3 {
            for kind in
                [SoundKind::Normal, SoundKind::Whistle, SoundKind::Finish, SoundKind::Clap, SoundKind::SliderTick]
            {
                let s = synthesise(set, kind);
                let peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                assert!((0.05..=1.0).contains(&peak), "{set} {kind:?}: peak {peak}");
                assert!(s[s.len() - 2].abs() < 0.02, "{set} {kind:?} does not decay to silence");
                assert!(!seen.contains(&s), "{set} {kind:?} duplicates another sound");
                seen.push(s);
            }
        }
    }
}
