//! Getting from "the user wants this map" to something playable, and from
//! a finished play to its result: settings, beatmap lookup, loading, and
//! saving the replay.

use crate::config::Config;
use crate::play::{MapSounds, ObjectCues, Play, SoundIds};
use crate::ranking::rank;
use crate::scores;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use rsu_audio::bank::{Folder, SampleBank, SoundKind};
use rsu_core::game::{Game, Summary};
use rsu_core::{mods, Chart, Time, RULESET_VERSION};
use rsu_formats::md5::hex;
use rsu_formats::replay::Replay;
use rsu_formats::{hitsound, osu, osudb};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// What this run uses: the settings file with the command line on top.
pub struct Settings {
    pub songs: Option<PathBuf>,
    pub skin: Option<PathBuf>,
    pub offset_ms: f64,
    pub music_volume: f32,
    pub effect_volume: f32,
    /// Use a beatmap's own samples and per-object sound settings.
    pub beatmap_hitsounds: bool,
    /// How much of a beatmap's background is taken away during play.
    pub background_dim: f32,
}

/// Command line values that override or update the settings file.
#[derive(Default)]
pub struct Overrides {
    pub songs: Option<PathBuf>,
    pub skin: Option<String>,
    pub offset_ms: Option<f64>,
    pub volume: Option<f32>,
    pub effects: Option<f32>,
}

pub fn settings(over: &Overrides, config: &mut Config) -> Settings {
    // A folder or skin given on the command line is remembered.
    if over.songs.is_some() || over.skin.is_some() {
        config.songs = over.songs.clone().or(config.songs.take());
        config.skin = over.skin.clone().or(config.skin.take());
        if let Err(e) = config.save() {
            eprintln!("rsu: could not save settings: {e}");
        }
    }
    let usual = || {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        ["osu/Songs", ".local/share/osu-wine/osu!/Songs", ".wine/drive_c/osu!/Songs", "Games/osu!/Songs"]
            .iter()
            .map(|p| home.join(p))
            .find(|p| p.is_dir())
    };
    let songs = config.songs.clone().filter(|d| d.is_dir()).or_else(usual);
    // A skin is a folder, or a name in the Skins folder next to Songs.
    let skin = config.skin.as_ref().and_then(|name| {
        let direct = PathBuf::from(name);
        if direct.is_dir() {
            return Some(direct);
        }
        let named = songs.as_ref()?.parent()?.join("Skins").join(name);
        named.is_dir().then_some(named)
    });
    if let (Some(name), None) = (&config.skin, &skin) {
        eprintln!("rsu: skin '{name}' not found; using built-in hit sounds");
    }
    Settings {
        songs,
        skin,
        offset_ms: over.offset_ms.unwrap_or(config.offset_ms),
        music_volume: over.volume.unwrap_or(config.music_volume).clamp(0.0, 1.0),
        effect_volume: over.effects.unwrap_or(config.effect_volume).clamp(0.0, 1.0),
        beatmap_hitsounds: config.beatmap_hitsounds,
        background_dim: config.background_dim,
    }
}

/// Beatmaps come from a case-insensitive file system; ours is not.
pub fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    // A name with a folder in it is looked up one step at a time.
    let name = name.replace('\\', "/");
    if let Some((folder, rest)) = name.split_once('/') {
        let sub =
            std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
                p.is_dir() && p.file_name().is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(folder))
            })?;
        return find_file(&sub, rest);
    }
    let name = name.as_str();
    let exact = dir.join(name);
    if exact.is_file() {
        return Some(exact);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(name)) && p.is_file())
}

fn collect_maps(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect_maps(&path, out);
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("osu")) {
            out.push(path);
        }
    }
}

/// Turn what the user typed into a beatmap file: a path if it is one,
/// otherwise the one map in the songs folder whose name has all the words.
pub fn resolve_map(query: &str, songs: Option<&Path>) -> Res<PathBuf> {
    let direct = Path::new(query);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let Some(songs) = songs else {
        return Err(format!(
            "'{query}' is not a file, and no songs folder is known to search in; \
             pass the full path or tell rsu where your songs are with --songs DIR"
        )
        .into());
    };
    let mut maps = Vec::new();
    collect_maps(songs, &mut maps);
    let name_of = |p: &Path| p.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
    let wanted = query.to_lowercase();
    // A pasted file name wins over a word search.
    if let Some(exact) = maps.iter().find(|p| name_of(p) == wanted) {
        return Ok(exact.clone());
    }
    let words: Vec<&str> = wanted.trim_end_matches(".osu").split_whitespace().collect();
    let mut found: Vec<&PathBuf> = maps.iter().filter(|p| words.iter().all(|w| name_of(p).contains(w))).collect();
    found.sort();
    match found.len() {
        0 => Err(format!("no beatmap matching '{query}' in {} ({} maps)", songs.display(), maps.len()).into()),
        1 => Ok(found[0].clone()),
        n => {
            eprintln!("rsu: {n} beatmaps match '{query}'; add words to pick one:");
            for p in found.iter().take(25) {
                eprintln!("  {}", p.file_name().unwrap_or_default().to_string_lossy());
            }
            if n > 25 {
                eprintln!("  ... and {} more", n - 25);
            }
            Err("ambiguous beatmap name".into())
        }
    }
}

/// The sounds of the menus. Every sample table starts with them, in this
/// order, so that they can be played whatever map is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiSound {
    Click = 0,
    Hit = 1,
    Back = 2,
    CheckOn = 3,
    CheckOff = 4,
}

impl UiSound {
    const FILES: [&'static str; 5] = ["menuclick", "menuhit", "menuback", "check-on", "check-off"];

    pub fn ids(self) -> SoundIds {
        SoundIds::one(self as u16, 100)
    }
}

fn add_ui_sounds(bank: &mut SampleBank, skin: Option<&Folder>) {
    for name in UiSound::FILES {
        bank.named(name, skin);
    }
}

/// The table of samples for the menus: their own sounds, and the built-in
/// or skin sound of a tap.
pub fn tap_sound(settings: &Settings) -> (Arc<[Arc<[f32]>]>, SoundIds) {
    let mut bank = SampleBank::new();
    let skin = settings.skin.as_deref().map(Folder::open);
    add_ui_sounds(&mut bank, skin.as_ref());
    let id = bank.id(1, SoundKind::Normal, 0, &Folder::default(), skin.as_ref());
    (bank.into_table(), SoundIds::one(id, 100))
}

/// A beatmap ready to be played, everything but its music.
#[derive(Clone)]
pub struct LoadedMap {
    /// The chart without mods; [`LoadedMap::chart_for`] gives it with them.
    pub chart: Arc<Chart>,
    pub beatmap: Arc<osu::Beatmap>,
    pub title: String,
    pub creator: String,
    /// The background image, if the map names one that is there.
    pub background: Option<PathBuf>,
    pub md5: [u8; 16],
    /// Silence before the track starts, in map time.
    /// osu!'s own offsets for this map (local plus online), in map time.
    pub map_offset: Time,
    pub sounds: MapSounds,
    pub samples: Arc<[Arc<[f32]>]>,
    pub audio_path: Option<PathBuf>,
    /// The beatmap's file name without extension, for naming replays.
    pub file_stem: String,
}

impl LoadedMap {
    /// The chart as these mods make it (easy and hard rock change it).
    pub fn chart_for(&self, game_mods: u32) -> Arc<Chart> {
        if game_mods & (mods::EASY | mods::HARD_ROCK) == 0 {
            self.chart.clone()
        } else {
            Arc::new(self.beatmap.to_chart_with_mods(game_mods))
        }
    }
}

pub fn load_map(path: &Path, settings: &Settings, with_sounds: bool) -> Res<LoadedMap> {
    let (map, md5) = osu::load(path)?;
    let chart = Arc::new(map.to_chart());
    if chart.objects.is_empty() {
        return Err(format!("{}: no hit objects", path.display()).into());
    }
    let title = format!("{} - {} [{}]", map.artist, map.title, map.version);
    eprintln!("rsu: {title}, {} objects, md5 {}", chart.objects.len(), hex(&md5));
    let dir = path.parent().unwrap_or(Path::new("."));
    let audio_path = find_file(dir, &map.audio_filename);
    if audio_path.is_none() {
        eprintln!("rsu: audio file '{}' not found next to the beatmap", map.audio_filename);
    }

    // osu! keeps a timing correction per beatmap; McOsu applies both kinds.
    let map_offset = match map_offsets(settings.songs.as_deref()).get(&md5) {
        Some(o) => {
            eprintln!("rsu: osu! offsets for this map: local {} ms, online {} ms", o.local_ms, o.online_ms);
            (o.local_ms as Time + o.online_ms as Time) * 1000
        }
        None => 0,
    };

    // Hit sounds: work out what the map asks for and load each sample once.
    let mut bank = SampleBank::new();
    let mut sounds = MapSounds::default();
    if with_sounds {
        let skin = settings.skin.as_deref().map(Folder::open);
        add_ui_sounds(&mut bank, skin.as_ref());
        // The McOsu way never looks into the beatmap's folder.
        let (style, beatmap_folder) = match settings.beatmap_hitsounds {
            true => (hitsound::Style::Stable, Folder::open(dir)),
            false => (hitsound::Style::McOsu, Folder::default()),
        };
        let mut ids = |sound: &hitsound::Sound| {
            let mut out = SoundIds { volume: sound.volume, ..SoundIds::default() };
            for (key, level) in sound.layers.iter().zip(sound.levels).filter_map(|(k, l)| Some(((*k)?, l))) {
                out.levels[out.count as usize] = level;
                let kind = match key.kind {
                    hitsound::SoundKind::Normal => SoundKind::Normal,
                    hitsound::SoundKind::Whistle => SoundKind::Whistle,
                    hitsound::SoundKind::Finish => SoundKind::Finish,
                    hitsound::SoundKind::Clap => SoundKind::Clap,
                    hitsound::SoundKind::SliderTick => SoundKind::SliderTick,
                };
                out.ids[out.count as usize] = bank.id(key.set, kind, key.index, &beatmap_folder, skin.as_ref());
                out.count += 1;
            }
            out
        };
        sounds.objects = hitsound::resolve(&map, &chart, style)
            .iter()
            .map(|o| ObjectCues {
                edges: o.edges.iter().map(&mut ids).collect(),
                ticks: o.ticks.iter().map(|(t, s)| (*t, ids(s))).collect(),
            })
            .collect();
        sounds.sections = hitsound::sections(&map, style);
        // The sounds that follow the moment instead of an object.
        let mut extra = |set: u8, kind: SoundKind| Some(bank.id(set, kind, 0, &Folder::default(), skin.as_ref()));
        sounds.slide = [1, 2, 3].map(|set| extra(set, SoundKind::SliderSlide));
        sounds.spin = extra(0, SoundKind::SpinnerSpin);
        sounds.bonus = extra(0, SoundKind::SpinnerBonus);
        sounds.combo_break = extra(0, SoundKind::ComboBreak);
        eprintln!(
            "rsu: sounds: {} from the beatmap, {} from the skin, {} built in",
            bank.from_beatmap, bank.from_skin, bank.synthesised
        );
    }
    Ok(LoadedMap {
        chart,
        title,
        creator: map.creator.clone(),
        background: Some(&map.background).filter(|b| !b.is_empty()).and_then(|b| find_file(dir, b)),
        beatmap: Arc::new(map),
        md5,
        map_offset,
        sounds,
        samples: bank.into_table(),
        audio_path,
        file_stem: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
    })
}

/// The per-beatmap offsets from osu!'s database next to the Songs folder,
/// read once. Empty when there is no such file or it cannot be read.
fn map_offsets(songs: Option<&Path>) -> &'static HashMap<[u8; 16], osudb::Offsets> {
    static OFFSETS: OnceLock<HashMap<[u8; 16], osudb::Offsets>> = OnceLock::new();
    OFFSETS.get_or_init(|| {
        let path = songs.and_then(Path::parent).map(|p| p.join("osu!.db"));
        let Some(data) = path.as_ref().and_then(|p| std::fs::read(p).ok()) else {
            return HashMap::new();
        };
        match osudb::offsets(&data) {
            Some(found) => {
                eprintln!("rsu: osu!.db: {} beatmaps have an offset of their own", found.len());
                found
            }
            None => {
                eprintln!("rsu: osu!.db is in a layout this version does not read; per-map offsets are not applied");
                HashMap::new()
            }
        }
    })
}

/// How a play ended, for the terminal and the results screen.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub summary: Summary,
    pub hit_mean_ms: f64,
    pub hit_sd_ms: f64,
    pub hit_count: u64,
    /// Live plays: whether re-simulating the recording gave the same result.
    /// Replays: whether playback reached the recorded result. `None` when
    /// there was nothing to compare.
    pub verified: Option<bool>,
    pub replay_saved: bool,
    /// Where the replay went, and when the play was (seconds since 1970).
    pub replay_path: Option<PathBuf>,
    pub unix_time: i64,
}

fn replays_dir() -> Res<PathBuf> {
    Ok(scores::dir().ok_or("neither XDG_DATA_HOME nor HOME is set")?)
}

/// Report the result; for a live session, check and save the replay.
/// `save_as` overrides where it goes (and saves an unfinished play too).
pub fn finish_play(play: &Play, save_as: Option<&Path>, file_stem: &str) -> Outcome {
    let s = play.game.summary();
    let errors = play.hit_error();
    let mut outcome = Outcome {
        summary: s,
        hit_mean_ms: errors.mean(),
        hit_sd_ms: errors.stddev(),
        hit_count: errors.n(),
        verified: None,
        replay_saved: false,
        replay_path: None,
        unix_time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64),
    };
    println!("{}", play.title);
    println!(
        "  score {}  accuracy {:.2}%  max combo {}  300/100/50/miss {}/{}/{}/{}  grade {}{}",
        s.score,
        s.accuracy() * 100.0,
        s.max_combo,
        s.n300,
        s.n100,
        s.n50,
        s.miss,
        rank(&s),
        if play.done { "" } else { "  (stopped early)" }
    );
    if play.mods != 0 {
        println!("  mods: {}", mods::name(play.mods));
    }
    if errors.n() > 0 {
        let (mean, n) = (errors.mean(), errors.n());
        println!("  hit error mean {:+.2} ms, sd {:.2} ms over {} hits", mean, errors.stddev(), n);
        // Only worth mentioning when it is clearly not noise.
        if play.playback.is_none() && n >= 100 && mean.abs() >= 5.0 {
            println!(
                "  you hit {:.0} ms {} on average; if it feels that way too, `{}` during a play moves the offset",
                mean.abs(),
                if mean < 0.0 { "early" } else { "late" },
                if mean < 0.0 { "-" } else { "=" }
            );
        }
    }

    if let Some(pb) = &play.playback {
        if pb.seeks > 0 {
            println!(
                "  {} seeks ({} keyframes); the last one took {:.3} ms",
                pb.seeks,
                pb.keyframe_count(),
                pb.last_seek_ns as f64 / 1e6
            );
        }
        if let (Some(expected), true) = (pb.expected, play.done) {
            outcome.verified = Some(expected == s);
            if expected == s {
                println!("  replay VERIFIED: playback reached the recorded result");
            } else {
                println!(
                    "  replay MISMATCH: recorded score {} state {:016x}, playback score {} state {:016x}",
                    expected.score, expected.state_hash, s.score, s.state_hash
                );
            }
        }
        return outcome;
    }

    if play.practice.is_active() || play.mods & (mods::SUDDEN_DEATH | mods::SPUN_OUT) != 0 {
        println!("  practice, SD/PF or SO: result and replay are not saved");
        return outcome;
    }

    // The recording must reproduce the session it came from. This is the
    // frame loop's own determinism check, run after every play.
    let end = play.game.time();
    let started = std::time::Instant::now();
    let again = Game::simulate(play.chart.clone(), play.mods, &play.recorded, end);
    outcome.verified = Some(again == s);
    println!(
        "  replay check: {} inputs re-simulated in {:.3} ms: {}",
        play.recorded.len(),
        started.elapsed().as_secs_f64() * 1e3,
        if again == s { "identical to the live result" } else { "DIFFERENT from the live result (a determinism bug)" }
    );
    if play.late_inputs > 0 {
        println!("  {} inputs arrived out of order and were moved to keep time monotonic", play.late_inputs);
    }
    if play.record_overflow {
        println!("  the recording buffer filled up; the replay is incomplete and was not saved");
        return outcome;
    }

    let unix = outcome.unix_time;
    let path = match (save_as, play.done) {
        (Some(p), _) => Ok(p.to_path_buf()),
        (None, true) => replays_dir().and_then(|dir| {
            std::fs::create_dir_all(&dir)?;
            Ok(dir.join(format!("{file_stem} {unix}.tkr")))
        }),
        (None, false) => return outcome, // an abandoned play is not kept
    };
    let replay = Replay {
        ruleset: RULESET_VERSION,
        mods: play.mods,
        rate_pct: play.rate_pct,
        map_md5: play.map_md5,
        unix_time: unix,
        player: std::env::var("USER").unwrap_or_default(),
        summary: s,
        end_time: end,
        inputs: play.recorded.clone(),
    };
    let bytes = replay.encode();
    match path.and_then(|p| Ok(std::fs::write(&p, &bytes).map(|_| p)?)) {
        Ok(p) => {
            outcome.replay_saved = true;
            println!("  replay saved: {} ({} bytes)", p.display(), bytes.len());
            outcome.replay_path = Some(p);
        }
        Err(e) => println!("  the replay could not be saved: {e}"),
    }
    outcome
}

#[cfg(test)]
mod practice_tests {
    use super::*;
    use rsu_core::chart::{ChartBuilder, Difficulty};
    #[test]
    fn sd_and_pf_never_save_replays_even_to_explicit_path() {
        let chart = Arc::new(ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 9.0, hp: 5.0 }).build());
        let path = std::env::temp_dir().join(format!("rsu-sd-preserve-{}.tkr", std::process::id()));
        std::fs::write(&path, b"existing").unwrap();
        for mods in [32, 32 | 16384, mods::SPUN_OUT] {
            let mut play = Play::new(chart.clone(), String::new(), [0; 16], 0, 0, mods, MapSounds::default(), None);
            play.done = true;
            for dest in [Some(path.as_path()), None] {
                assert!(!finish_play(&play, dest, "sd-nosave-test").replay_saved);
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn practice_preserves_existing_replay_and_skips_normal_score_storage() {
        use rsu_formats::osu::PracticeDifficulty;
        let chart = Arc::new(ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 9.0, hp: 5.0 }).build());
        let mut play = Play::new(chart, String::new(), [0; 16], 0, 0, 0, MapSounds::default(), None);
        play.done = true;
        let stem = format!("rsu-practice-preservation-{}", std::process::id());
        let dir = replays_dir().unwrap();
        let matching = || std::fs::read_dir(&dir).ok().into_iter().flatten().flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&stem))
            .map(|entry| entry.path()).collect::<std::collections::HashSet<_>>();
        let before = matching();
        let path = std::env::temp_dir().join(format!("{stem}.tkr"));
        std::fs::write(&path, b"existing replay bytes").unwrap();
        for practice in [
            PracticeDifficulty { cs: Some(0.0), ..Default::default() },
            PracticeDifficulty { ar: Some(0.0), ..Default::default() },
            PracticeDifficulty { od: Some(0.0), ..Default::default() },
            PracticeDifficulty { hp: Some(0.0), ..Default::default() },
            PracticeDifficulty { ar_lock: true, ..Default::default() },
            PracticeDifficulty { od_lock: true, ..Default::default() },
            PracticeDifficulty { flip_left_right: true, ..Default::default() },
            PracticeDifficulty { flip_up_down: true, ..Default::default() },
            PracticeDifficulty { fading_cursor: true, ..Default::default() },
            PracticeDifficulty { no_spinners: true, ..Default::default() },
            PracticeDifficulty { no_50s: true, ..Default::default() },
        ] {
            play.practice = practice;
            for destination in [Some(path.as_path()), None] {
                let outcome = finish_play(&play, destination, &stem);
                assert!(!outcome.replay_saved && outcome.replay_path.is_none());
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"existing replay bytes");
            assert_eq!(matching(), before);
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn practice_cs_hp_and_locks_never_write_even_explicit_replay_path() {
        use rsu_formats::osu::PracticeDifficulty;
        for (label, practice) in [
            ("cs", PracticeDifficulty { cs: Some(0.0), ..Default::default() }),
            ("hp", PracticeDifficulty { hp: Some(0.0), ..Default::default() }),
            ("ar-lock", PracticeDifficulty { ar_lock: true, ..Default::default() }),
            ("od-lock", PracticeDifficulty { od_lock: true, ..Default::default() }),
        ] {
            let chart = Arc::new(ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 9.0, hp: 5.0 }).build());
            let mut play = Play::new(chart, String::new(), [0; 16], 0, 0, 0, MapSounds::default(), None);
            play.practice = practice;
            play.done = true;
            let path = std::env::temp_dir().join(format!("rsu-practice-{label}-test-{}.tkr", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let outcome = finish_play(&play, Some(&path), label);
            assert!(!outcome.replay_saved && outcome.replay_path.is_none());
            assert!(!path.exists());
        }
    }
    #[test]
    fn practice_ar_never_writes_even_explicit_replay_path() {
        let chart = Arc::new(ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 9.0, hp: 5.0 }).build());
        let mut play = Play::new(chart, String::new(), [0; 16], 0, 0, 0, MapSounds::default(), None);
        play.practice.ar = Some(5.0);
        play.done = true;
        let path = std::env::temp_dir().join(format!("rsu-practice-ar-test-{}.tkr", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let outcome = finish_play(&play, Some(&path), "practice-ar");
        assert!(!outcome.replay_saved && outcome.replay_path.is_none());
        assert!(!path.exists());
    }
    #[test]
    fn practice_never_writes_even_explicit_replay_path() {
        let chart = Arc::new(ChartBuilder::new(Difficulty { cs: 4.0, od: 5.0, ar: 9.0, hp: 5.0 }).build());
        let mut play = Play::new(chart, String::new(), [0; 16], 0, 0, 0, MapSounds::default(), None);
        play.practice.od = Some(5.0);
        play.done = true;
        let path = std::env::temp_dir().join(format!("rsu-practice-test-{}.tkr", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let outcome = finish_play(&play, Some(&path), "practice");
        assert!(!outcome.replay_saved && outcome.replay_path.is_none());
        assert!(!path.exists());
    }
}
