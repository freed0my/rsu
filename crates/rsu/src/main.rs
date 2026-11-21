//! rsu: the frame loop.
//!
//! One loop serves every screen: song selection, play, results, and the
//! timing test pattern. The order of steps inside a frame is the point of
//! the whole program; see the numbered comments in `run`.

mod absmap;
mod atlas;
mod background;
mod binds;
mod browser;
mod config;
mod demo;
mod fps;
mod image;
mod jukebox;
mod library;
mod menu;
mod modselect;
mod options;
mod overlay;
mod pause;
mod play;
mod ranking;
mod scores;
mod session;
mod skin;
mod smoke;
mod text;
mod transport;
mod ui;

use absmap::AbsMap;
use atlas::Atlas;
use background::Backgrounds;
use browser::{Browser, BrowserAction, Group, Sort};
use config::Config;
use demo::Demo;
use jukebox::{Jukebox, Song};
use library::Scan;
use menu::{MainMenu, MenuAction, MusicView};
use modselect::ModSelect;
use options::{Options, OptionsAction, Tunables};
use overlay::{Metrics, SentFrame};
use pause::{PauseAction, PauseMenu};
use play::{Field, Play, Playback};
use ranking::{Results, ResultsAction};
use scores::Score;
use session::{finish_play, load_map, resolve_map, LoadedMap, Outcome, Overrides, Res, Settings, UiSound};
use skin::Skin;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use rsu_audio::clock::ClockModel;
use rsu_audio::mixer::{AudioCmd, BeatGrid, Cycle, Mixer, MixerConfig};
use rsu_audio::stream::{Stream, Streamer};
use rsu_audio::SAMPLE_RATE;
use rsu_core::auto::autoplay;
use rsu_core::game::Button;
use rsu_core::{mods, RULESET_VERSION};
use rsu_formats::md5::hex;
use rsu_formats::osu::PracticeDifficulty;
use rsu_gfx::{Gfx, PresentMode, SurfaceHandle, FLAG_AT_CURSOR, PAGE_BACKGROUND, PAGE_UI};
use rsu_platform::audio_pw::{self, PwOptions};
use rsu_platform::evdev::{self, CursorSample, InputKind};
use rsu_platform::window::{Backend, FrameFeedback, NativeSurface, WinEvent, Window, WindowOptions};
use rsu_rt::alloc_guard::{CountingAlloc, HotSection};
use rsu_rt::clock::{now_ns, sleep_until_precise};
use rsu_rt::{seqlock, spsc};
use transport::Transport;
use ui::{Backdrop, Notice, VolumeOverlay};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const KEY_ESC: u32 = 1;
const KEY_MINUS: u32 = 12;
const KEY_EQUAL: u32 = 13;
const KEY_SPACE: u32 = 57;
const KEY_LEFT_ALT: u32 = 56;
const KEY_RIGHT_ALT: u32 = 100;
const KEY_LEFT_CTRL: u32 = 29;
const KEY_RIGHT_CTRL: u32 = 97;
const KEY_LEFT_SHIFT: u32 = 42;
const KEY_RIGHT_SHIFT: u32 = 54;
const KEY_O: u32 = 24;
const KEY_R: u32 = 19;
/// The measurements: show, next present mode, start over.
const KEY_F9: u32 = 67;
const KEY_F10: u32 = 68;
const KEY_F11: u32 = 87;
const KEY_HOME: u32 = 102;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;
/// Left mouse button and pen tip, as evdev reports them.
const BTN_LEFT: u16 = 0x110;
const BTN_TOUCH: u16 = 0x14a;

/// How long the quick retry key is held for.
const QUICK_RETRY_NS: u64 = 270_000_000;

#[derive(Default)]
struct BrowserRepeat {
    held: Option<(u32, u64)>,
}

impl BrowserRepeat {
    fn key(&mut self, key: &KeyEvent) {
        if !matches!(key.code, KEY_UP | KEY_DOWN) {
            return;
        }
        if key.pressed {
            if !self.held.is_some_and(|(code, _)| code == key.code) {
                self.held = Some((key.code, key.t_ns.saturating_add(400_000_000)));
            }
        } else if self.held.is_some_and(|(code, _)| code == key.code) {
            self.held = None;
        }
    }

    fn update(&mut self, active: bool, now: u64) -> Option<u32> {
        if !active {
            self.held = None;
            return None;
        }
        let (code, due) = self.held?;
        if now < due {
            return None;
        }
        // Don't replay a backlog after a long frame.
        self.held = Some((code, now.saturating_add(50_000_000)));
        Some(code)
    }
}

#[cfg(test)]
mod browser_repeat_tests {
    use super::*;

    #[test]
    fn held_arrow_repeats_after_delay_and_stops_on_release() {
        let mut repeat = BrowserRepeat::default();
        repeat.key(&KeyEvent { code: KEY_DOWN, pressed: true, t_ns: 1_000_000_000 });
        assert_eq!(repeat.update(true, 1_399_999_999), None);
        assert_eq!(repeat.update(true, 1_400_000_000), Some(KEY_DOWN));
        assert_eq!(repeat.update(true, 1_420_000_000), None);
        assert_eq!(repeat.update(true, 1_450_000_000), Some(KEY_DOWN));
        repeat.key(&KeyEvent { code: KEY_DOWN, pressed: false, t_ns: 1_460_000_000 });
        assert_eq!(repeat.update(true, 2_000_000_000), None);
    }

    #[test]
    fn repeat_cancels_when_inactive_and_latest_direction_wins() {
        let mut repeat = BrowserRepeat::default();
        let key = |code, pressed, t_ns| KeyEvent { code, pressed, t_ns };
        repeat.key(&key(KEY_UP, true, 0));
        repeat.key(&key(KEY_DOWN, true, 10));
        repeat.key(&key(KEY_UP, false, 20));
        assert_eq!(repeat.update(true, 400_000_010), Some(KEY_DOWN));
        assert_eq!(repeat.update(false, 500_000_000), None);
        assert_eq!(repeat.update(true, 600_000_000), None);
        repeat.key(&key(ui::KEY_ENTER, true, 600_000_000));
        assert_eq!(repeat.update(true, 2_000_000_000), None);
    }
}

fn quick_retry_key(since: &mut Option<u64>, live: bool, matches: bool, key: &KeyEvent) {
    if live && matches {
        *since = if key.pressed { since.or(Some(key.t_ns)) } else { None };
    }
}

fn quick_retry_ready(since: &mut Option<u64>, live: bool, now: u64) -> bool {
    if !live {
        *since = None;
    }
    since.is_some_and(|start| now.saturating_sub(start) >= QUICK_RETRY_NS)
}

#[cfg(test)]
mod replay_trail_tests {
    use super::*;
    use std::mem::MaybeUninit;

    #[test]
    fn fading_cursor_clamps_combo_and_restores_visibility_on_pause() {
        for (combo, expected) in [(0, 1.0), (25, 0.5), (50, 0.0), (100, 0.0), (u32::MAX, 0.0)] {
            assert_eq!(fading_cursor_opacity(true, false, combo), expected);
            assert_eq!(fading_cursor_opacity(true, true, combo), 1.0);
            assert_eq!(fading_cursor_opacity(false, false, combo), 1.0);
        }
    }

    #[test]
    fn autoplay_pause_cursor_matches_pause_menu_pointer_and_resume_restores_playback() {
        let path = std::env::temp_dir().join(format!("rsu-auto-pause-{}.osu", std::process::id()));
        std::fs::write(&path, b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n").unwrap();
        let settings = Settings { songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0, beatmap_hitsounds: false, background_dim: 0.0 };
        let map = load_map(&path, &settings, false).unwrap();
        std::fs::remove_file(path).unwrap();
        let current = Current { map, music: None, stable_source: false, mods: 0, rate_pct: 0, auto: true, practice: PracticeDifficulty { fading_cursor: true, ..Default::default() } };
        let (tx, _rx) = spsc::channel(32);
        let (_clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
        let (_retired_tx, retired_rx) = spsc::channel(32);
        let mut transport = Transport::new(tx, clock_rx, retired_rx, false, 0);
        let screen = Screen::Play(begin_play(&current, None, &settings, &mut transport, 0));
        assert!(matches!(&screen, Screen::Play(play) if play.practice.fading_cursor && play.playback.is_some()));
        let field = Field::new(1920.0, 1080.0);
        let physical = (960.0, 540.0);
        let playback_position = match &screen { Screen::Play(play) => play.display_cursor(&field), _ => unreachable!() };
        assert_ne!(playback_position, physical);
        assert_eq!(frame_cursor(&screen, &field, None, physical, false), playback_position);
        let visible = frame_cursor(&screen, &field, None, physical, true);
        assert_eq!(visible, physical, "the cursor shown over the pause menu must be the pointer used by buttons");
        let input = ui::Input { x: visible.0, y: visible.1, pressed: true, down: true, scroll: 0.0, keys: &[], text: "", dt: 0.016, now_ns: 0 };
        assert_eq!(PauseMenu::new(0).update(&input, &ui::Metrics::new(1920.0, 1080.0), &Skin::default()), PauseAction::Retry);
        assert_eq!(frame_cursor(&screen, &field, None, physical, false), playback_position);
    }

    #[test]
    fn transparent_cursor_preserves_middle_without_inventing_fallback() {
        let mut skin = Skin::default();
        skin.cursor = Some(skin::Sprite::default());
        skin.cursor_middle = Some(skin::Sprite { w: 32.0, h: 32.0, ..Default::default() });
        let mut storage: [MaybeUninit<rsu_gfx::Inst>; 4] = [MaybeUninit::uninit(); 4];
        let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        draw_game_cursor(&mut batch, &skin, 1.0, 1.0);
        assert_eq!(batch.len(), 1, "only the visible middle layer should be drawn");
        let shape = unsafe { storage[0].assume_init() };
        assert_eq!(shape.color, [255, 255, 255, 255]);
        skin.cursor_middle = None;
        let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        draw_game_cursor(&mut batch, &skin, 1.0, 1.0);
        assert_eq!(batch.len(), 0, "an explicitly transparent cursor must not fall back to a ring");
    }

    #[test]
    fn cursor_layers_and_fallback_use_straight_alpha() {
        let mut skin = Skin::default();
        let sprite = skin::Sprite { w: 32.0, h: 32.0, ..Default::default() };
        skin.cursor = Some(sprite);
        skin.cursor_middle = Some(sprite);
        for skinned in [true, false] {
            if !skinned { skin.cursor = None; skin.cursor_middle = None; }
            let mut storage: [MaybeUninit<rsu_gfx::Inst>; 4] = [MaybeUninit::uninit(); 4];
            let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
            draw_game_cursor(&mut batch, &skin, 1.0, 0.5);
            assert_eq!(batch.len(), 2);
            let shapes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
            assert!(shapes.iter().all(|shape| shape.color[3] == 128));
            assert_eq!(shapes[0].color[..3], if skinned { [255,255,255] } else { [255,220,60] });
            assert!(shapes.iter().all(|shape| shape.kind & FLAG_AT_CURSOR != 0));
        }
    }

    #[test]
    fn replay_trail_opacity_scales_alpha_without_squaring_tint() {
        let mut storage: [MaybeUninit<rsu_gfx::Inst>; 4] = [MaybeUninit::uninit(); 4];
        let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        draw_replay_trail_stamp(&mut batch, None, 0.0, 0.0, 1.0, 128, 0.5);
        let stamp = unsafe { storage[0].assume_init() };
        assert_eq!(stamp.color, [128, 128, 128, 64]);
    }

    #[test]
    fn an_explicitly_transparent_cursortrail_hides_replay_trail() {
        let mut storage: [MaybeUninit<rsu_gfx::Inst>; 4] = [MaybeUninit::uninit(); 4];
        let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
        draw_replay_trail_stamp(&mut batch, Some(&skin::Sprite::default()), 100.0, 100.0, 1.0, 255, 1.0);
        assert_eq!(batch.len(), 0, "an empty skin sprite intentionally disables the trail");
        draw_replay_trail_stamp(&mut batch, None, 100.0, 100.0, 1.0, 255, 1.0);
        assert_eq!(batch.len(), 1, "an absent skin image can use the default dot");
    }
}

#[cfg(test)]
mod quick_retry_tests {
    use super::*;

    #[test]
    fn held_retry_works_while_paused() {
        let mut since = None;
        let key = KeyEvent { code: 14, pressed: true, t_ns: 100 };
        quick_retry_key(&mut since, true, true, &key);
        assert!(!quick_retry_ready(&mut since, true, 100 + QUICK_RETRY_NS - 1));
        assert!(quick_retry_ready(&mut since, true, 100 + QUICK_RETRY_NS));
    }

    #[test]
    fn releasing_retry_cancels_the_hold() {
        let mut since = None;
        let mut key = KeyEvent { code: 14, pressed: true, t_ns: 100 };
        quick_retry_key(&mut since, true, true, &key);
        key.pressed = false;
        quick_retry_key(&mut since, true, true, &key);
        assert!(!quick_retry_ready(&mut since, true, 100 + QUICK_RETRY_NS));
    }

    #[test]
    fn retry_is_disabled_outside_live_play() {
        let mut since = Some(100);
        assert!(!quick_retry_ready(&mut since, false, 100 + QUICK_RETRY_NS));
    }
}

const SPEEDS: [f64; 6] = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0];

const HELP: &str = "\
rsu [MAP] [options]

Without arguments rsu opens its main menu. Given a map it plays that map
and exits when it is over.

  MAP                   a .osu file, or words from its name to look up in the
                        songs folder: rsu nevermind hayakou
  --songs DIR           the osu! Songs folder (remembered)
  --mods LIST           dt (1.5x), nc (same, the name osu! gives it with raised
                        pitch) or ht (0.75x). Pitch follows speed in both.
                        sd fails on a miss; pf retries on less than 300.
                        so spins automatically (not with --auto).
                        SD/PF/SO results and replays are not saved.
  --skin NAME           look and hit sounds from this skin: a folder, or the name of one
                        in the Skins folder next to Songs (remembered)
  --replay FILE          watch a .tkr or osu!stable .osr replay of that beatmap
  --auto                watch a generated perfect play
  --save-replay FILE    where to write the replay (default: rsu's data folder,
                        and only when the map was played to the end)
  --start-ms N          when watching: start at this time of the map
  --ar N                practice AR 0 to 10 after mods; no score or replay saved
  --od N                practice OD 0 to 10 after mods; no score or replay saved
  --ar-lock             preserve approach timing at the map rate (practice)
  --od-lock             preserve hit windows at the map rate (practice)
  --cs N                practice CS 0 to 12.1 after mods; no score or replay saved
  --hp N                practice HP 0 to 12.5 after mods; no score or replay saved
  --rate X              play at this speed, 0.05 to 2.5, instead of the one the
                        mods give (McOsu's speed multiplier); pitch follows
  --speed X             when watching: playback speed, 0.25 to 2

  --calibrate           tap along with a metronome; on exit your average
                        timing error is saved as the universal offset
  --pattern             the timing test pattern: one circle on a beat grid
  --audio FILE          test pattern: play a track instead of the metronome
  --bpm N               test pattern: tempo (default 120)
  --beat-offset-ms N    test pattern: time of the first beat in the track

  --windowed            open a window instead of going fullscreen
  --x11 | --wayland     force a window backend (default: Wayland if available)
  --flip-left-right     experimental mirrored playfield (direct beatmap only; no saves)
  --flip-up-down        experimental vertical mirror (direct beatmap only; no saves)
  --present MODE        immediate | mailbox | fifo-relaxed | fifo
                        (default: the lowest-latency mode the surface offers)
  --fps-cap N           frame limit for non-vsynced modes, 0 = unlimited (default: what
                        the options say, 1000 at first)
  --offset-ms N         universal offset for this run instead of the saved one:
                        how much later the sound reaches you than the audio
                        device reports; the game runs that much later
                        (`-` and `=` change it during a play)
  --display-latency-ms N  test pattern: draw this far ahead of the audio
  --quantum N           requested PipeWire quantum in frames (default 64)
  --volume N            music volume, 0.0..1.0 (default 0.4)
  --effects N           hit sound volume, 0.0..1.0 (default 1.0)
  --no-evdev            use only window-system input
  --no-audio            run without an audio device (system clock)
  --smoke SECONDS       run for a while, print the measurements, exit

Main menu: click the square (or Enter), then Play, Options or Exit.
Song browser: type to search, Up/Down through the buttons, Left/Right through
the sets, Enter or a click on the selected difficulty to play, F1 mods,
F2 random, F3 options, Esc clears the search or goes back.
Playing: Z/X hit, Esc pauses, -/= move the offset, the wheel the volume.
Results: Enter back, R retry, W watch the replay.
Watching: Left/Right seek 5 s, Up/Down speed, Space pause, Home restart.
Everywhere: F9 measurements, F10 next present mode, F11 reset measurements.";

struct Args {
    map: Option<String>,
    songs: Option<PathBuf>,
    skin: Option<String>,
    mods: u32,
    rate_pct: u16,
    practice: PracticeDifficulty,
    calibrate: bool,
    pattern: bool,
    replay: Option<PathBuf>,
    auto: bool,
    save_replay: Option<PathBuf>,
    start_ms: Option<f64>,
    speed: Option<f64>,
    fullscreen: bool,
    backend: Backend,
    present: Option<PresentMode>,
    fps_cap: Option<u32>,
    audio: Option<PathBuf>,
    bpm: f64,
    beat_offset_ms: f64,
    offset_ms: Option<f64>,
    display_latency_ms: f64,
    quantum: u32,
    volume: Option<f32>,
    effects: Option<f32>,
    evdev: bool,
    audio_enabled: bool,
    smoke_s: Option<f64>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        map: None,
        songs: None,
        skin: None,
        mods: 0,
        rate_pct: 0,
        practice: PracticeDifficulty::default(),
        calibrate: false,
        pattern: false,
        replay: None,
        auto: false,
        save_replay: None,
        start_ms: None,
        speed: None,
        fullscreen: true,
        backend: Backend::Auto,
        present: None,
        fps_cap: None,
        audio: None,
        bpm: 120.0,
        beat_offset_ms: 0.0,
        offset_ms: None,
        display_latency_ms: 0.0,
        quantum: 64,
        volume: None,
        effects: None,
        evdev: true,
        audio_enabled: true,
        smoke_s: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        fn num<T: std::str::FromStr>(name: &str, v: String) -> Result<T, String> {
            v.parse().map_err(|_| format!("{name}: bad number '{v}'"))
        }
        match arg.as_str() {
            "--windowed" => a.fullscreen = false,
            "--x11" => a.backend = Backend::X11,
            "--wayland" => a.backend = Backend::Wayland,
            "--no-evdev" => a.evdev = false,
            "--no-audio" => a.audio_enabled = false,
            "--auto" => a.auto = true,
            "--replay" => a.replay = Some(value("--replay")?.into()),
            "--songs" => a.songs = Some(value("--songs")?.into()),
            "--skin" => a.skin = Some(value("--skin")?),
            "--mods" => a.mods = mods::parse_live(&value("--mods")?)?,
            "--flip-left-right" => a.practice.flip_left_right = true,
            "--flip-up-down" => a.practice.flip_up_down = true,
            "--ar-lock" => a.practice.ar_lock = true,
            "--od-lock" => a.practice.od_lock = true,
            "--cs" | "--ar" | "--od" | "--hp" => {
                let (field, max) = match arg.as_str() {
                    "--cs" => (&mut a.practice.cs, PracticeDifficulty::CS_MAX),
                    "--ar" => (&mut a.practice.ar, 10.0),
                    "--od" => (&mut a.practice.od, 10.0),
                    _ => (&mut a.practice.hp, PracticeDifficulty::HP_MAX),
                };
                let v: f32 = num(&arg, value(&arg)?)?;
                if !(0.0..=max).contains(&v) {
                    return Err(format!("{arg} must be between 0 and {max}"));
                }
                *field = Some(v);
            }
            "--rate" => {
                let rate: f64 = num("--rate", value("--rate")?)?;
                if !(0.05..=2.5).contains(&rate) {
                    return Err("--rate must be between 0.05 and 2.5".into());
                }
                a.rate_pct = (rate * 100.0).round() as u16;
            }
            "--calibrate" => a.calibrate = true,
            "--pattern" => a.pattern = true,
            "--effects" => a.effects = Some(num("--effects", value("--effects")?)?),
            "--start-ms" => a.start_ms = Some(num("--start-ms", value("--start-ms")?)?),
            "--speed" => a.speed = Some(num("--speed", value("--speed")?)?),
            "--save-replay" => a.save_replay = Some(value("--save-replay")?.into()),
            "--present" => {
                a.present = Some(match value("--present")?.as_str() {
                    "immediate" => PresentMode::Immediate,
                    "mailbox" => PresentMode::Mailbox,
                    "fifo-relaxed" => PresentMode::FifoRelaxed,
                    "fifo" => PresentMode::Fifo,
                    other => return Err(format!("--present: unknown mode '{other}'")),
                })
            }
            "--fps-cap" => a.fps_cap = Some(num("--fps-cap", value("--fps-cap")?)?),
            "--audio" => a.audio = Some(value("--audio")?.into()),
            "--bpm" => a.bpm = num("--bpm", value("--bpm")?)?,
            "--beat-offset-ms" => a.beat_offset_ms = num("--beat-offset-ms", value("--beat-offset-ms")?)?,
            "--offset-ms" => a.offset_ms = Some(num("--offset-ms", value("--offset-ms")?)?),
            "--display-latency-ms" => {
                a.display_latency_ms = num("--display-latency-ms", value("--display-latency-ms")?)?
            }
            "--quantum" => a.quantum = num("--quantum", value("--quantum")?)?,
            "--volume" => a.volume = Some(num("--volume", value("--volume")?)?),
            "--smoke" => a.smoke_s = Some(num("--smoke", value("--smoke")?)?),
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other if other.starts_with('-') => return Err(format!("unknown argument '{other}' (try --help)")),
            // Several bare words are one search: rsu nevermind hayakou
            word => {
                a.map = Some(match a.map.take() {
                    Some(so_far) => format!("{so_far} {word}"),
                    None => word.to_string(),
                })
            }
        }
    }
    if a.auto && a.mods & mods::SPUN_OUT != 0 {
        return Err("--auto cannot be combined with SO".into());
    }
    if !(a.bpm > 0.0) {
        return Err("--bpm must be positive".into());
    }
    if a.map.is_none() && (a.replay.is_some() || a.auto) {
        return Err("--replay and --auto need a beatmap".into());
    }
    if (a.start_ms.is_some() || a.speed.is_some()) && a.replay.is_none() && !a.auto {
        return Err("--start-ms and --speed apply when watching (--replay or --auto)".into());
    }
    if a.speed.is_some_and(|s| !(0.25..=2.0).contains(&s)) {
        return Err("--speed must be between 0.25 and 2".into());
    }
    // The test pattern is what --calibrate and its own options run on.
    a.pattern |= a.calibrate || a.audio.is_some();
    if a.pattern && a.map.is_some() {
        return Err("--pattern and --calibrate run on the metronome, without a beatmap".into());
    }
    if a.map.is_none() || a.replay.is_some() || a.pattern {
        for (flag, locked) in [("--ar-lock", a.practice.ar_lock), ("--od-lock", a.practice.od_lock), ("--flip-left-right", a.practice.flip_left_right), ("--flip-up-down", a.practice.flip_up_down)] {
            if locked {
                return Err(format!("{flag} requires a beatmap and cannot be used with replay or pattern"));
            }
        }
        for (flag, value) in [("--ar", a.practice.ar), ("--od", a.practice.od), ("--cs", a.practice.cs), ("--hp", a.practice.hp)] {
            if value.is_some() {
                return Err(format!("{flag} requires a beatmap and cannot be used with replay or pattern"));
            }
        }
    }
    if a.rate_pct != 0 && a.replay.is_some() {
        return Err("a replay already carries the rate it was played at".into());
    }
    if a.mods != 0 && a.replay.is_some() {
        return Err("a replay already carries the mods it was played with".into());
    }
    Ok(a)
}

#[cfg(test)]
mod modselect_routing_tests {
    use super::*;

    #[test]
    fn experimental_retreat_preserves_both_flips_and_unblocks_grid() {
        for (w, h) in [(800.0, 600.0), (1920.0, 1080.0)] {
            let m = ui::Metrics::new(w, h); let skin = Skin::default();
            let mut s = Some(ModSelect::new()); let mut b = Some(Browser::new(None, Sort::Artist));
            let idle = ui::Input { x: 1.0, y: h * 0.5, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
            update_modselect(&mut s, &mut b, false, &idle, &m, &skin);
            for y in [0.1, 0.16] {
                let click = ui::Input { x: w * 0.1, y: h * y, pressed: true, down: true, ..idle };
                update_modselect(&mut s, &mut b, false, &click, &m, &skin);
            }
            let away = ui::Input { x: w * 0.8, ..idle };
            update_modselect(&mut s, &mut b, false, &away, &m, &skin);
            assert!(b.as_ref().unwrap().practice.flip_left_right && b.as_ref().unwrap().practice.flip_up_down);
            let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
            let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
            s.as_ref().unwrap().draw_experimental(&mut batch, &m, &away, b.as_ref().unwrap().practice);
            let first = unsafe { storage[0].assume_init() };
            assert!(first.half[1] < h * 0.2, "panel must close after pointer leaves");
        }
    }

    #[test]
    fn captured_slider_keeps_dragging_across_experimental_edge() {
        for (w, h) in [(800.0, 600.0), (1920.0, 1080.0)] {
            for row_index in [0, 4] {
                let m = ui::Metrics::new(w, h); let skin = Skin::default();
                let mut s = Some(ModSelect::new()); let mut b = Some(Browser::new(None, Sort::Artist));
                let row = ModSelect::override_row(&m, &skin, row_index);
                let press = ui::Input { x: row.x + row.w * 0.5, y: row.y + row.h * 0.5, pressed: true, down: true, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
                update_modselect(&mut s, &mut b, false, &press, &m, &skin);
                let edge = ui::Input { x: 1.0, pressed: false, ..press };
                update_modselect(&mut s, &mut b, false, &edge, &m, &skin);
                let right = ui::Input { x: row.x + row.w, pressed: false, ..press };
                update_modselect(&mut s, &mut b, false, &right, &m, &skin);
                let list = b.as_ref().unwrap();
                if row_index == 0 { assert_eq!(list.practice.cs, Some(PracticeDifficulty::CS_MAX)); }
                else { assert_eq!(list.rate_pct, mods::RATE_MAX_PCT); }
            }
        }
    }

    #[test]
    fn experimental_render_stays_inside_panel() {
        for (w, h) in [(100.0, 600.0), (320.0, 600.0), (800.0, 600.0), (1920.0, 1080.0)] {
            let m = ui::Metrics::new(w, h); let skin = Skin::default();
            let mut s = Some(ModSelect::new()); let mut b = Some(Browser::new(None, Sort::Artist));
            let input = ui::Input { x: 1.0, y: h * 0.5, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
            update_modselect(&mut s, &mut b, false, &input, &m, &skin);
            let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
            let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
            s.as_ref().unwrap().draw_experimental(&mut batch, &m, &input, b.as_ref().unwrap().practice);
            let shapes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
            let panel = shapes[0]; let edge = panel.center[0] + panel.half[0];
            for shape in &shapes[1..] { assert!(shape.center[0] + shape.half[0] <= edge + 0.01, "shape beyond panel: {:?}, edge {}", shape, edge); }
        }
    }

    #[test]
    fn options_cover_blocks_mod_grid_hover_drawing() {
        for (w, h) in [(800.0, 600.0), (1920.0, 1080.0)] {
            let m = ui::Metrics::new(w, h); let skin = Skin::default();
            let mut s = Some(ModSelect::new()); let mut b = Some(Browser::new(None, Sort::Artist));
            let base = ui::Input { x: w * 0.5, y: h * 0.5, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 1.0, now_ns: 0 };
            update_modselect(&mut s, &mut b, false, &base, &m, &skin);
            let render = |input: &ui::Input| {
                let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
                let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
                draw_modselect(s.as_ref().unwrap(), b.as_ref().unwrap(), true, &mut batch, &m, &skin, input);
                format!("{:?}", unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) })
            };
            // Flashlight is unavailable and emits a hover tooltip in the grid.
            let unit = h / 768.0; let cell_h = (66.0 * unit).min(h * 0.088); let cell_w = 68.0 * unit;
            let x = w * 0.5 - cell_w * 5.5 + 8.0 * cell_w + cell_w * 0.5;
            let hover = ui::Input { x, y: h * 0.5 - cell_h * 1.75 + cell_h * 1.25 + cell_h * 0.5, ..base };
            let away = ui::Input { x: w - 1.0, y: h - 1.0, ..base };
            assert_eq!(render(&hover), render(&away), "covered grid must not react to pointer");
        }
    }

    #[test]
    fn no_spinners_filters_chart_and_keeps_matching_sound_cues_on_start_and_retry() {
        let path = std::env::temp_dir().join(format!("rsu-no-spinner-{}.osu", std::process::id()));
        std::fs::write(&path, b"osu file format v14\n[HitObjects]\n100,100,1000,1,2\n256,192,2000,12,4,2500\n200,100,3000,1,8\n").unwrap();
        let settings = Settings { songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0, beatmap_hitsounds: false, background_dim: 0.0 };
        let map = load_map(&path, &settings, true).unwrap();
        std::fs::remove_file(path).unwrap();
        let expected = format!("{:?}", map.sounds.objects[2]);
        let metrics = ui::Metrics::new(1280.0, 720.0);
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let hover = ui::Input { x: 1.0, y: 360.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
        update_modselect(&mut selection, &mut browser, false, &hover, &metrics, &Skin::default());
        let click = ui::Input { x: 128.0, y: 720.0 * 0.28, pressed: true, down: true, ..hover };
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &Skin::default());
        let mut current = Current { map, music: None, stable_source: false, mods: 0, rate_pct: 0, auto: false, practice: browser.unwrap().practice };
        let (tx, _rx) = spsc::channel(32);
        let (_ctx, crx) = seqlock::channel(ClockModel::INVALID);
        let (_rtx, rrx) = spsc::channel(32);
        let mut transport = Transport::new(tx, crx, rrx, false, 0);
        for play in [begin_play(&current, None, &settings, &mut transport, 0), retry_play(&mut current, &settings, &mut transport, 0)] {
            assert_eq!(play.chart.objects.len(), 2);
            assert_eq!((play.chart.objects[1].combo_number, play.chart.objects[1].combo_index), (1, 0));
            assert_eq!(format!("{:?}", play.sounds.objects[1]), expected);
        }
    }

    #[test]
    fn no_spinners_spinner_only_and_practice_combinations() {
        for only in [false, true] {
            let path = std::env::temp_dir().join(format!("rsu-no-spinner-only-{}-{only}.osu", std::process::id()));
            let objects = if only { "256,192,1000,8,4,2000\n" } else { "256,192,1000,8,4,2000\n100,100,3000,1,8\n" };
            std::fs::write(&path, format!("osu file format v14\n[HitObjects]\n{objects}")).unwrap();
            let settings = Settings { songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0, beatmap_hitsounds: false, background_dim: 0.0 };
            let map = load_map(&path, &settings, true).unwrap();
            std::fs::remove_file(path).unwrap();
            let practice = PracticeDifficulty { no_spinners: true, flip_left_right: true, ar: Some(3.0), od_lock: true, ..Default::default() };
            let mut current = Current { map, music: None, stable_source: false, mods: mods::HARD_ROCK, rate_pct: 150, auto: false, practice };
            let (tx, _rx) = spsc::channel(32);
            let (_ctx, crx) = seqlock::channel(ClockModel::INVALID);
            let (_rtx, rrx) = spsc::channel(32);
            let mut transport = Transport::new(tx, crx, rrx, false, 0);
            for play in [begin_play(&current, None, &settings, &mut transport, 0), retry_play(&mut current, &settings, &mut transport, 0)] {
                assert_eq!(play.chart.objects.len(), usize::from(!only));
                assert_eq!(play.sounds.objects.len(), usize::from(!only));
                assert_eq!(play.practice, practice);
                assert_eq!(play.chart.difficulty.ar, 3.0);
                if !only { assert_eq!(play.chart.objects[0].combo_number, 1); }
            }
        }
    }

    #[test]
    fn no_50s_live_circle_is_miss_after_100_window() {
        let path = std::env::temp_dir().join(format!("rsu-no50-{}.osu", std::process::id()));
        std::fs::write(&path, b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n").unwrap();
        let settings = Settings { songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0, beatmap_hitsounds: false, background_dim: 0.0 };
        let map = load_map(&path, &settings, false).unwrap();
        std::fs::remove_file(path).unwrap();
        let current = Current { map, music: None, stable_source: false, mods: 0, rate_pct: 0, auto: false, practice: PracticeDifficulty { no_50s: true, ..Default::default() } };
        let (tx, _rx) = spsc::channel(32);
        let (_ctx, crx) = seqlock::channel(ClockModel::INVALID);
        let (_rtx, rrx) = spsc::channel(32);
        let mut transport = Transport::new(tx, crx, rrx, false, 0);
        let mut play = begin_play(&current, None, &settings, &mut transport, 0);
        let at = 1_000_000 - play.chart.window_100 - 1;
        let obj = &play.chart.objects[0];
        let mut events = Vec::new();
        play.game.push(rsu_core::game::Input { t: at, kind: rsu_core::game::InputKind::Move { x: obj.x, y: obj.y } }, &mut |e| events.push(e));
        play.game.push(rsu_core::game::Input { t: at, kind: rsu_core::game::InputKind::Press(rsu_core::game::Button::K1) }, &mut |e| events.push(e));
        assert!(events.iter().any(|e| matches!(e, rsu_core::game::Event::Judged { result: rsu_core::game::HitResult::Miss, .. })));
    }

    #[test]
    fn experimental_no_50s_row_toggles_practice() {
        let metrics = ui::Metrics::new(1280.0, 720.0);
        let skin = Skin::default();
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let hover = ui::Input { x: 1.0, y: 360.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
        update_modselect(&mut selection, &mut browser, false, &hover, &metrics, &skin);
        let click = ui::Input { x: 128.0, y: 720.0 * 0.34, pressed: true, down: true, ..hover };
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(browser.as_ref().unwrap().practice.is_active());
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(!browser.as_ref().unwrap().practice.is_active());
    }

    #[test]
    fn experimental_no_spinners_row_toggles_practice() {
        let metrics = ui::Metrics::new(1280.0, 720.0);
        let skin = Skin::default();
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let hover = ui::Input { x: 1.0, y: 360.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
        update_modselect(&mut selection, &mut browser, false, &hover, &metrics, &skin);
        let click = ui::Input { x: 128.0, y: 720.0 * 0.28, pressed: true, down: true, ..hover };
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(browser.as_ref().unwrap().practice.is_active());
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(!browser.as_ref().unwrap().practice.is_active());
    }

    #[test]
    fn experimental_fading_cursor_row_toggles_practice() {
        let metrics = ui::Metrics::new(1280.0, 720.0);
        let skin = Skin::default();
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let hover = ui::Input { x: 1.0, y: 360.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
        update_modselect(&mut selection, &mut browser, false, &hover, &metrics, &skin);
        let click = ui::Input { x: 128.0, y: 720.0 * 0.22, pressed: true, down: true, ..hover };
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(browser.as_ref().unwrap().practice.is_active());
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(!browser.as_ref().unwrap().practice.is_active());
    }

    #[test]
    fn experimental_second_flip_row_toggles_independently() {
        let metrics = ui::Metrics::new(1280.0, 720.0);
        let skin = Skin::default();
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let hover = ui::Input { x: 1.0, y: 360.0, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
        update_modselect(&mut selection, &mut browser, false, &hover, &metrics, &skin);
        let click = ui::Input { x: 128.0, y: 720.0 * 0.16, pressed: true, down: true, ..hover };
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(browser.as_ref().unwrap().practice.is_active());
        assert!(!browser.as_ref().unwrap().practice.flip_left_right);
        update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
        assert!(!browser.as_ref().unwrap().practice.is_active());
    }

    #[test]
    fn experimental_flip_is_accessible_from_left_edge() {
        for (w, h) in [(1280.0, 720.0), (1920.0, 1080.0)] {
            let metrics = ui::Metrics::new(w, h);
            let skin = Skin::default();
            let mut selection = Some(ModSelect::new());
            let mut browser = Some(Browser::new(None, Sort::Artist));
            let hover = ui::Input { x: 1.0, y: h * 0.5, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
            update_modselect(&mut selection, &mut browser, false, &hover, &metrics, &skin);
            let click = ui::Input { x: w * 0.1, y: h * 0.1, pressed: true, down: true, ..hover };
            update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
            assert!(browser.as_ref().unwrap().practice.flip_left_right);
        }
    }

    #[test]
    fn experimental_render_toggle_and_options_cover() {
        for (w, h) in [(800.0, 600.0), (1920.0, 1080.0)] {
            let metrics = ui::Metrics::new(w, h);
            let skin = Skin::default();
            let mut selection = Some(ModSelect::new());
            let mut browser = Some(Browser::new(None, Sort::Artist));
            let idle = ui::Input { x: 1.0, y: h * 0.5, pressed: false, down: false, scroll: 0.0, keys: &[], text: "", dt: 0.1, now_ns: 0 };
            let render = |selection: &Option<ModSelect>, practice| {
                let mut storage = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
                let mut batch = unsafe { rsu_gfx::Batch::over(storage.as_mut_ptr().cast(), storage.len()) };
                selection.as_ref().unwrap().draw_experimental(&mut batch, &metrics, &idle, practice);
                format!("{:?}", unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) })
            };
            let blank = render(&selection, browser.as_ref().unwrap().practice);
            update_modselect(&mut selection, &mut browser, false, &idle, &metrics, &skin);
            let off = render(&selection, browser.as_ref().unwrap().practice);
            assert_ne!(blank, off);
            let click = ui::Input { x: w * 0.1, y: h * 0.1, pressed: true, down: true, ..idle };
            update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
            let on = render(&selection, browser.as_ref().unwrap().practice);
            assert_ne!(off, on);
            update_modselect(&mut selection, &mut browser, true, &click, &metrics, &skin);
            assert!(browser.as_ref().unwrap().practice.flip_left_right);
            assert_eq!(blank, render(&selection, browser.as_ref().unwrap().practice));
            update_modselect(&mut selection, &mut browser, false, &idle, &metrics, &skin);
            update_modselect(&mut selection, &mut browser, false, &click, &metrics, &skin);
            assert!(!browser.as_ref().unwrap().practice.flip_left_right);
            let away = ui::Input { x: w * 0.8, ..idle };
            update_modselect(&mut selection, &mut browser, false, &away, &metrics, &skin);
            assert_eq!(blank, render(&selection, browser.as_ref().unwrap().practice));
        }
    }

    #[test]
    fn modselect_up_changes_practice_summary() {
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let metrics = ui::Metrics::new(1920.0, 1080.0);
        let skin = Skin::default();
        let render = |selection: &mut Option<ModSelect>, browser: &mut Option<Browser>, keys: &[u32]| {
            let input = ui::Input { x: 0.0, y: 0.0, pressed: false, down: false, scroll: 0.0, keys, text: "", dt: 0.016, now_ns: 0 };
            update_modselect(selection, browser, false, &input, &metrics, &skin);
            let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
            let mut batch = unsafe { rsu_gfx::Batch::over(store.as_mut_ptr().cast(), store.len()) };
            selection.as_ref().unwrap().draw_practice(&mut batch, &metrics, &skin, &input, browser.as_ref().unwrap().practice);
            format!("{:?}", unsafe { std::slice::from_raw_parts(store.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) })
        };
        let before = render(&mut selection, &mut browser, &[]);
        let after = render(&mut selection, &mut browser, &[ui::KEY_UP]);
        assert_ne!(before, after, "AR adjustment should change the visible practice summary");
    }

    #[test]
    fn modselect_mouse_changes_ar_at_upstream_override_row() {
        for (w, h) in [(1920.0, 1080.0), (800.0, 600.0)] {
            let mut selection = Some(ModSelect::new());
            let mut browser = Some(Browser::new(None, Sort::Artist));
            let metrics = ui::Metrics::new(w, h);
            let skin = Skin::default();
            // Upstream places override sliders above the mod grid; AR is row 1.
            let unit = h / 768.0;
            let cell_h = (66.0 * unit).min(h * 0.088);
            let grid_top = h * 0.5 - cell_h * 3.5 * 0.5;
            let height = 25.0 * unit;
            let gap = ((grid_top - 5.0 * height) / 4.0) * 0.35;
            let start = grid_top / 2.0 - (5.0 * height + 4.0 * gap) / 1.75;
            let input = ui::Input { x: w * 0.5, y: start + height + gap + height * 0.5, pressed: true, down: true, scroll: 0.0, keys: &[], text: "", dt: 0.016, now_ns: 0 };
            update_modselect(&mut selection, &mut browser, false, &input, &metrics, &skin);
            assert!(browser.as_ref().unwrap().practice.ar.is_some(), "AR slider must respond to a mouse press");
        }
    }

    #[test]
    fn modselect_routes_cs_and_hp_without_touching_ar_od_or_speed() {
        for (w, h) in [(1920.0, 1080.0), (800.0, 600.0)] {
            let mut selection = Some(ModSelect::new());
            let mut browser = Some(Browser::new(None, Sort::Artist));
            let metrics = ui::Metrics::new(w, h);
            let skin = Skin::default();
            let click = |i, x_fraction, keys: &'static [u32]| {
                let row = ModSelect::override_row(&metrics, &skin, i);
                ui::Input { x: row.x + row.w * x_fraction, y: row.y + row.h / 2.0, pressed: true, down: true, scroll: 0.0, keys, text: "", dt: 0.016, now_ns: 0 }
            };
            update_modselect(&mut selection, &mut browser, true, &click(0, 1.0, &[]), &metrics, &skin);
            assert!(!browser.as_ref().unwrap().practice.is_active());
            update_modselect(&mut selection, &mut browser, false, &click(0, 0.98, &[]), &metrics, &skin);
            assert_eq!(browser.as_ref().unwrap().practice.cs, Some(PracticeDifficulty::CS_MAX));
            update_modselect(&mut selection, &mut browser, false, &click(3, 0.98, &[]), &metrics, &skin);
            let list = browser.as_ref().unwrap();
            assert_eq!(list.practice.hp, Some(PracticeDifficulty::HP_MAX));
            assert_eq!((list.practice.ar, list.practice.od, list.rate_pct), (None, None, 0));
            update_modselect(&mut selection, &mut browser, false, &click(0, 0.01, &[]), &metrics, &skin);
            assert_eq!(browser.as_ref().unwrap().practice.cs, None);
            assert!(browser.as_ref().unwrap().practice.hp.is_some());
            for keys in [&[ui::KEY_BACKSPACE][..], &[2][..]] {
                update_modselect(&mut selection, &mut browser, false, &click(0, 0.98, keys), &metrics, &skin);
                assert!(!browser.as_ref().unwrap().practice.is_active());
            }
        }
    }

    #[test]
    fn ar_override_lead_in_uses_effective_chart_on_start_retry_and_watch() {
        let path = std::env::temp_dir().join(format!("rsu-practice-lead-{}.osu", std::process::id()));
        std::fs::write(&path, b"osu file format v14\n[Difficulty]\nApproachRate:10\n[HitObjects]\n100,100,100,1,0\n").unwrap();
        let settings = Settings { songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0, beatmap_hitsounds: false, background_dim: 0.0 };
        let map = load_map(&path, &settings, false).unwrap();
        std::fs::remove_file(&path).unwrap();
        let current = Current { map, music: None, stable_source: false, mods: 0, rate_pct: 0, auto: false, practice: PracticeDifficulty { ar: Some(0.0), ..Default::default() } };
        let (cmd_tx, _cmd_rx) = spsc::channel(32);
        let (_clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
        let (_retired_tx, retired_rx) = spsc::channel(32);
        let mut transport = Transport::new(cmd_tx, clock_rx, retired_rx, false, 0);
        let chart = current.chart();
        let expected = Play::lead_in_for(&chart, current.map.beatmap.audio_lead_in_ms) as f64 * SAMPLE_RATE as f64 / 1e6;
        for playback in [None, None, Some(perfect_play(&chart, 0))] {
            let play = begin_play(&current, playback, &settings, &mut transport, 0);
            assert_eq!(play.timeline_frames(0), expected);
        }
    }

    #[test]
    fn stable_retry_clears_unimplemented_mods_before_live_play() {
        let path = std::env::temp_dir().join(format!("rsu-retry-foreign-{}.osu", std::process::id()));
        std::fs::write(&path, b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n").unwrap();
        let settings = Settings { songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0, beatmap_hitsounds: false, background_dim: 0.0 };
        let map = load_map(&path, &settings, false).unwrap();
        std::fs::remove_file(path).unwrap();
        let mut current = Current { map, music: None, stable_source: true, mods: mods::HIDDEN | 1024, rate_pct: 0, auto: false, practice: PracticeDifficulty::default() };
        let (tx, _rx) = spsc::channel(16);
        let (_clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
        let (_retired_tx, retired_rx) = spsc::channel(16);
        let mut transport = Transport::new(tx, clock_rx, retired_rx, false, 0);
        let play = retry_play(&mut current, &settings, &mut transport, 0);
        assert_eq!(play.mods, mods::HIDDEN, "Flashlight cannot be recorded as a live mod");
        assert_eq!(current.mods, mods::HIDDEN);
        let replay_path = std::env::temp_dir().join(format!("rsu-retry-foreign-{}.tkr", std::process::id()));
        let outcome = finish_play(&play, Some(&replay_path), "retry");
        assert!(outcome.replay_saved);
        assert_eq!(rsu_formats::replay::Replay::read(&replay_path).unwrap().mods, mods::HIDDEN);
        std::fs::remove_file(replay_path).unwrap();
    }

    #[test]
    fn practice_overrides_and_locks_survive_retry_and_in_memory_watch() {
        let path = std::env::temp_dir().join(format!("rsu-practice-lifecycle-{}.osu", std::process::id()));
        std::fs::write(&path, b"osu file format v14\n[HitObjects]\n100,100,1000,1,0\n100,100,1100,1,0\n").unwrap();
        let settings = Settings {
            songs: None, skin: None, offset_ms: 0.0, music_volume: 1.0, effect_volume: 1.0,
            beatmap_hitsounds: false, background_dim: 0.0,
        };
        let map = load_map(&path, &settings, false);
        std::fs::remove_file(&path).unwrap();
        let practice = PracticeDifficulty { cs: Some(0.0), hp: Some(12.5), ar_lock: true, od_lock: true, flip_left_right: true, flip_up_down: true, fading_cursor: true, ..Default::default() };
        let game_mods = mods::HARD_ROCK | mods::HALF_TIME;
        let current = Current { map: map.unwrap(), music: None, stable_source: false, mods: game_mods, rate_pct: 200, auto: false, practice };
        let (cmd_tx, _cmd_rx) = spsc::channel(16);
        let (_clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
        let (_retired_tx, retired_rx) = spsc::channel(16);
        let mut transport = Transport::new(cmd_tx, clock_rx, retired_rx, false, 0);
        let first = begin_play(&current, None, &settings, &mut transport, 0);
        let retry = begin_play(&current, None, &settings, &mut transport, 0);
        let chart = current.chart();
        let playback = perfect_play(&chart, current.mods);
        let watched = begin_play(&current, Some(playback), &settings, &mut transport, 0);
        assert!(watched.playback.is_some());
        let normal = current.map.chart_for(current.mods);
        assert!(!watched.practice.fading_cursor, "old recordings must not inherit the visual mod");
        for (play, expected) in [(first, practice), (retry, practice), (watched, PracticeDifficulty { fading_cursor: false, ..practice })] {
            assert_eq!(play.practice, expected);
            assert_eq!(play.speed(), 2.0);
            assert_eq!(play.chart.preempt, normal.preempt * 2);
            assert_eq!(play.chart.window_300, normal.window_300 * 2);
            assert_eq!((play.chart.difficulty.cs, play.chart.difficulty.hp), (0.0, 12.5));
            assert_eq!(play.chart.radius, chart.radius);
            assert_eq!(play.chart.objects[0].x, chart.objects[0].x);
        }
        assert_ne!(normal.radius, chart.radius, "practice must not pollute the normal chart cache");
    }

    #[test]
    fn options_blocks_modselect_keyboard_mouse_and_close() {
        let mut selection = Some(ModSelect::new());
        let mut browser = Some(Browser::new(None, Sort::Artist));
        let metrics = ui::Metrics::new(1920.0, 1080.0);
        let skin = Skin::default();
        let keys = [ui::KEY_RIGHT, ui::KEY_UP, 16, ui::KEY_ESC];
        let input = ui::Input { x: 960.0, y: 540.0, pressed: true, down: true, scroll: 1.0, keys: &keys, text: "", dt: 0.016, now_ns: 0 };
        assert!(!update_modselect(&mut selection, &mut browser, true, &input, &metrics, &skin));
        let list = browser.as_ref().unwrap();
        assert_eq!((list.practice, list.mods, list.auto, list.rate_pct), (PracticeDifficulty::default(), 0, false, 0));
        assert!(update_modselect(&mut selection, &mut browser, false, &input, &metrics, &skin));
        let list = browser.as_ref().unwrap();
        assert_eq!(list.practice.od, Some(5.5));
        assert_eq!(list.practice.ar, Some(5.5));
        assert_ne!(list.mods, 0);
    }
}

fn update_modselect(selection: &mut Option<ModSelect>, browser: &mut Option<Browser>, covered: bool, input: &ui::Input, metrics: &ui::Metrics, skin: &Skin) -> bool {
    if let (Some(selection), Some(list)) = (selection, browser) {
        if covered { selection.hide_experimental(); }
        let panel_hit = !covered && !(selection.slider_captured() && input.down) && selection.update_experimental(input, metrics, &mut list.practice);
        let input = if covered { input.covered() } else if panel_hit { ui::Input { keys: input.keys, text: input.text, ..input.covered() } } else { *input };
        let base = list.selected_map().map(|map| {
            mods::difficulty(rsu_core::Difficulty { cs: map.cs, ar: map.ar, od: map.od, hp: map.hp }, list.mods)
        });
        selection.update_practice(&input, metrics, skin, &mut list.practice, base);
        selection.update(&input, metrics, skin, &mut list.mods, &mut list.auto, &mut list.rate_pct)
    } else { false }
}

fn draw_modselect(selection: &ModSelect, list: &Browser, covered: bool, batch: &mut rsu_gfx::Batch, metrics: &ui::Metrics, skin: &Skin, input: &ui::Input) {
    let input = if covered { input.covered() } else { *input };
    selection.draw(batch, metrics, skin, &input, list.mods, list.auto, list.rate_pct);
    selection.draw_practice(batch, metrics, skin, &input, list.practice);
    selection.draw_experimental(batch, metrics, &input, list.practice);
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rsu: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("rsu: {e}");
        std::process::exit(1);
    }
}

/// What is on screen.
enum Screen {
    /// The timing test pattern (also what calibration runs on).
    Pattern(Demo),
    Menu,
    /// The song browser; its state lives outside so it survives a play.
    Select,
    Play(Box<Play>),
    Results(Box<Results>),
    /// Nothing: the program is on its way out.
    Done,
}

/// The map in play, kept so it can be retried or its replay watched.
struct Current {
    map: LoadedMap,
    music: Option<Arc<Stream>>,
    stable_source: bool,
    mods: u32,
    /// The player's own playback rate in percent; 0 leaves it to the mods.
    rate_pct: u16,
    /// Let the game play by itself.
    auto: bool,
    practice: PracticeDifficulty,
}

impl Current {
    fn speed(&self) -> f64 {
        mods::speed_with(self.mods, self.rate_pct)
    }

    fn chart(&self) -> Arc<rsu_core::Chart> {
        if self.practice.is_active() {
            Arc::new(self.map.beatmap.to_chart_with_practice_at_rate(self.mods, self.practice, self.speed()))
        } else {
            self.map.chart_for(self.mods)
        }
    }
}

/// A change of screen, decided inside the frame and carried out after it:
/// loading maps and dropping screens allocates and frees, which the frame
/// itself must not.
enum Transition {
    Quit,
    OpenBrowser,
    ToMenu,
    OpenOptions,
    CloseOptions,
    OpenMods,
    CloseMods,
    /// Play the map selected in the song browser.
    Start,
    /// Stop a play for a moment, and go on with it.
    Pause,
    /// The health ran out.
    Fail,
    Resume,
    /// Give a play up.
    Leave,
    /// The map is over.
    Finished,
    Back,
    Retry,
    Watch,
    /// Use the skin with this index in the options' list.
    SetSkin(usize),
    SetSongs,
    NextPresentMode,
    /// Show the result of an earlier play.
    ShowScore(usize),
}

fn play_transition(next: Option<Transition>, paused: bool, retry: bool, failed: bool, done: bool) -> Option<Transition> {
    if !paused && matches!(next, None | Some(Transition::Pause)) {
        if retry { return Some(Transition::Retry); }
        if failed { return Some(Transition::Fail); }
    }
    if done && next.is_none() { return Some(Transition::Finished); }
    next
}

#[cfg(test)]
mod play_transition_tests {
    use super::*;
    #[test]
    fn failure_and_retry_override_pause_but_not_other_actions() {
        assert!(matches!(play_transition(Some(Transition::Pause), false, true, false, false), Some(Transition::Retry)));
        assert!(matches!(play_transition(Some(Transition::Pause), false, false, true, false), Some(Transition::Fail)));
        assert!(matches!(play_transition(Some(Transition::Pause), false, false, false, false), Some(Transition::Pause)));
        assert!(matches!(play_transition(Some(Transition::Leave), false, true, false, false), Some(Transition::Leave)));
        assert!(matches!(play_transition(None, true, false, true, false), None));
    }
}

/// What a key asks of the replay viewer.
#[derive(Clone, Copy)]
enum ViewAction {
    SeekBy(f64),
    Restart,
    SpeedStep(i32),
    TogglePause,
}

#[derive(Clone, Copy, Default)]
struct KeyEvent {
    code: u32,
    pressed: bool,
    t_ns: u64,
}

/// The primary button, which a pen reports twice: from the device and as a
/// pointer button from the window system. It is down while either says so.
#[derive(Default)]
struct Buttons {
    device: bool,
    window: bool,
    /// It went down since the last frame (and may be up again already).
    tapped: bool,
}

impl Buttons {
    fn set(&mut self, from_device: bool, down: bool) {
        let before = self.device || self.window;
        if from_device {
            self.device = down;
        } else {
            self.window = down;
        }
        self.tapped |= !before && down;
    }

    /// Whether it went down since the last frame, and whether it is down.
    fn frame(&mut self) -> (bool, bool) {
        (std::mem::take(&mut self.tapped), self.device || self.window)
    }
}

/// Something a test script does.
#[derive(Clone, Copy)]
enum Scripted {
    Key(u32),
    Hold(u32, bool),
    Click(f32, f32),
    Move(f32, f32),
    Wheel(f32),
}

/// Scripted input for unattended runs of the interface, from the
/// `TAKT_KEYS` environment variable: entries separated by `;`, each
/// `MILLISECONDS:WHAT`, where WHAT is a key code, text to type, `@X,Y` for
/// a click at that window position, `~X,Y` to move the pointer there, or
/// `^N` for N notches of the wheel, `>CODE` and `<CODE` to press and
/// release a key apart (for Ctrl). This is a test hook, the only way to
/// exercise the screens without a person at the controls and without
/// injecting input into the whole desktop.
fn key_script() -> Vec<(u64, Scripted)> {
    let mut script = Vec::new();
    let point = |text: &str| {
        let (x, y) = text.split_once(',')?;
        Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
    };
    for entry in std::env::var("TAKT_KEYS").unwrap_or_default().split(';') {
        let Some((at, what)) = entry.split_once(':') else {
            continue;
        };
        let Ok(at) = at.trim().parse::<u64>() else {
            continue;
        };
        let what = what.trim();
        if let Some((x, y)) = what.strip_prefix('@').and_then(point) {
            script.push((at, Scripted::Click(x, y)));
        } else if let Some((x, y)) = what.strip_prefix('~').and_then(point) {
            script.push((at, Scripted::Move(x, y)));
        } else if let Some(n) = what.strip_prefix('^').and_then(|n| n.parse().ok()) {
            script.push((at, Scripted::Wheel(n)));
        } else if let Some(code) = what.strip_prefix('>').and_then(|c| c.parse().ok()) {
            script.push((at, Scripted::Hold(code, true)));
        } else if let Some(code) = what.strip_prefix('<').and_then(|c| c.parse().ok()) {
            script.push((at, Scripted::Hold(code, false)));
        } else if let Ok(code) = what.parse::<u32>() {
            script.push((at, Scripted::Key(code)));
        } else {
            for (n, c) in what.chars().enumerate() {
                if let Some(code) = (0..128).find(|&k| ui::key_char(k) == Some(c)) {
                    script.push((at + n as u64 * 20, Scripted::Key(code)));
                }
            }
        }
    }
    script.sort_by_key(|e| e.0);
    script
}

fn unsupported_retry_mods(game_mods: u32) -> u32 {
    game_mods & !(mods::KNOWN | mods::SUDDEN_DEATH | mods::PERFECT | mods::SPUN_OUT)
}

fn retry_play(current: &mut Current, settings: &Settings, transport: &mut Transport, now: u64) -> Box<Play> {
    if current.stable_source {
        let removed = unsupported_retry_mods(current.mods);
        if removed != 0 {
            eprintln!("rsu: Retry cleared unsupported stable mod bits 0x{removed:x}");
            current.mods &= !removed;
        }
        current.stable_source = false;
    }
    begin_play(current, None, settings, transport, now)
}

/// Point the audio at a map and make the game for it.
fn begin_play(
    current: &Current,
    playback: Option<Playback>,
    settings: &Settings,
    transport: &mut Transport,
    now: u64,
) -> Box<Play> {
    let map = &current.map;
    let chart = current.chart();
    let lead_in = Play::lead_in_for(&chart, map.beatmap.audio_lead_in_ms);
    let music_start = (lead_in as f64 * SAMPLE_RATE as f64 / 1e6).round() as u64;
    if let Some(music) = &current.music {
        // Every jump during a play lands on the very frames a decode from
        // the start gives, so the music keeps its time with the map.
        music.set_exact(true);
    }
    transport.load(current.music.clone(), music_start, Some(map.samples.clone()));
    transport.set_paused(false, now);
    transport.set_speed(current.speed(), now);
    transport.seek(0.0, now);
    // The auto mod is a perfect play, watched.
    let watching_recording = playback.is_some();
    let playback = playback.or_else(|| current.auto.then(|| perfect_play(&chart, current.mods)));
    let mut sounds = map.sounds.clone();
    if current.practice.no_spinners {
        let mut raw: Vec<_> = map.beatmap.objects.iter().collect();
        raw.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));
        sounds.objects = raw.into_iter().zip(sounds.objects).filter_map(|(object, cues)|
            (!matches!(object.kind, rsu_formats::osu::RawKind::Spinner { .. })).then_some(cues)
        ).collect();
    }
    let mut play = Box::new(Play::new(
        chart,
        map.title.clone(),
        map.md5,
        lead_in,
        (settings.offset_ms * 1000.0).round() as i64,
        current.mods,
        sounds,
        playback,
    ));
    play.map_offset = map.map_offset;
    play.rate_pct = current.rate_pct;
    play.practice = current.practice;
    if watching_recording {
        play.practice.fading_cursor = false;
        play.practice.no_50s = false;
    } else if current.practice.no_50s {
        play.game.enable_live_no_50s();
    }
    play
}

fn perfect_play(chart: &Arc<rsu_core::Chart>, game_mods: u32) -> Playback {
    let end = chart.objects.last().map_or(0, |o| o.end.max(o.t)) + 2_000_000;
    Playback::new(chart, game_mods, autoplay(chart, 133), end, None)
}

/// An absent trail image may use the built-in dot; an explicitly empty
/// skin image is intentional and must remain hidden.
fn draw_game_cursor(batch: &mut rsu_gfx::Batch, skin: &Skin, unit: f32, cursor_opacity: f32) {
    if skin.cursor.is_some() || skin.cursor_middle.is_some() {
        // Cursor images are sized for a screen 768 pixels tall.
        let unit = unit;
        for sprite in [&skin.cursor, &skin.cursor_middle].into_iter().flatten() {
            // Not centred means the image hangs from its top-left corner.
            let hang = if skin.cursor_centred { 0.0 } else { unit * 0.5 };
            sprite.draw_turned(batch, sprite.w * hang, sprite.h * hang, unit, [255, 255, 255, (cursor_opacity * 255.0).round() as u8], 0.0, FLAG_AT_CURSOR);
        }
    } else {
        batch.ring(0.0, 0.0, 14.0, 3.0, [255, 220, 60, (cursor_opacity * 255.0).round() as u8], FLAG_AT_CURSOR);
        batch.disc(0.0, 0.0, 3.0, [255, 255, 255, (cursor_opacity * 255.0).round() as u8], FLAG_AT_CURSOR);
    }
}

// Cursor chosen at the final GPU latch; playback remains sampled in map time.
fn frame_cursor(screen: &Screen, field: &Field, script: Option<(f32, f32)>, physical: (f32, f32), paused: bool) -> (f32, f32) {
    match (screen, script) {
        (Screen::Play(play), _) if play.playback.is_some() && !paused => play.display_cursor(field),
        (_, Some(position)) => position,
        _ => physical,
    }
}

fn fading_cursor_opacity(enabled: bool, paused: bool, combo: u32) -> f32 {
    if enabled && !paused { 1.0 - (combo as f32 / 50.0).clamp(0.0, 1.0) } else { 1.0 }
}

fn draw_replay_trail_stamp(batch: &mut rsu_gfx::Batch, sprite: Option<&skin::Sprite>, x: f32, y: f32, unit: f32, alpha: u8, opacity: f32) {
    let color = [alpha, alpha, alpha, (alpha as f32 * opacity.clamp(0.0, 1.0)).round() as u8];
    match sprite {
        Some(s) => s.draw_turned(batch, x, y, unit, color, 0.0, rsu_gfx::FLAG_TRAIL),
        None => batch.disc(x, y, 7.0 * unit, color, rsu_gfx::FLAG_TRAIL),
    }
}

/// Pack the images of the skin in `dir` into the atlases; without a
/// folder, the look rsu has on its own.
fn load_skin(dir: Option<&Path>, atlas: &mut Atlas, ui_atlas: &mut Atlas) -> Skin {
    let Some(dir) = dir else {
        return Skin::default();
    };
    let started = std::time::Instant::now();
    let skin = Skin::load(dir, atlas, ui_atlas);
    eprintln!(
        "rsu: skin '{}': {} images in {:.0} ms",
        skin.name,
        skin.images,
        started.elapsed().as_secs_f64() * 1000.0
    );
    skin
}

/// Carry what was changed in this run over into the settings file's
/// values. What still is as the program found it stays as the file has it:
/// a value from the command line is for one run.
fn remember(config: &mut Config, now: &Tunables, at_start: &Tunables, view: Option<(Sort, Group)>) {
    if now.master != at_start.master {
        config.master_volume = now.master;
    }
    if now.music != at_start.music {
        config.music_volume = now.music;
    }
    if now.effects != at_start.effects {
        config.effect_volume = now.effects;
    }
    if now.offset_ms != at_start.offset_ms {
        config.offset_ms = now.offset_ms as f64;
    }
    if now.dim != at_start.dim {
        config.background_dim = now.dim;
    }
    if now.cursor_scale != at_start.cursor_scale {
        config.cursor_scale = now.cursor_scale;
    }
    if now.replay_smooth_trail != at_start.replay_smooth_trail {
        config.replay_smooth_trail = now.replay_smooth_trail;
    }
    if now.fps_cap != at_start.fps_cap {
        config.fps_cap = Some(now.fps_cap);
    }
    if now.beatmap_hitsounds != at_start.beatmap_hitsounds {
        config.beatmap_hitsounds = now.beatmap_hitsounds;
    }
    if now.songselect_top != at_start.songselect_top {
        config.songselect_top = now.songselect_top;
    }
    if now.songselect_bottom != at_start.songselect_bottom {
        config.songselect_bottom = now.songselect_bottom;
    }
    if now.binds != at_start.binds {
        config.binds = now.binds;
    }
    if let Some((sort, group)) = view {
        config.sort = Some(sort.name().into());
        config.group = Some(group.name().into());
    }
    if let Err(e) = config.save() {
        eprintln!("rsu: could not save settings: {e}");
    }
}

fn run(args: Args) -> Res<()> {
    let mut config = Config::load();
    let mut settings = session::settings(
        &Overrides {
            songs: args.songs.clone(),
            skin: args.skin.clone(),
            offset_ms: args.offset_ms,
            volume: args.volume,
            effects: args.effects,
        },
        &mut config,
    );
    // A map on the command line means: play it and leave.
    let oneshot = args.map.is_some();
    // Music is played from its files, decoded on a thread of its own.
    let streamer = if args.audio_enabled {
        Streamer::start().map_err(|e| eprintln!("rsu: could not start the music thread: {e}")).ok()
    } else {
        None
    };
    let mut jukebox = Jukebox::new(streamer);

    // ---- what to start with (anything slow happens before real time does)
    let mut current: Option<Current> = None;
    let mut first_playback: Option<Playback> = None;
    let mut pattern: Option<(Demo, BeatGrid, Option<Arc<Stream>>)> = None;
    if let Some(query) = &args.map {
        let path = resolve_map(query, settings.songs.as_deref())?;
        let map = load_map(&path, &settings, args.audio_enabled)?;
        let (game_mods, rate_pct) = if let Some(replay_path) = &args.replay {
            let (replay, stable_frames) = scores::read_replay(replay_path)?;
            if replay.map_md5 != map.md5 {
                return Err(format!(
                    "{} was recorded on another beatmap (md5 {})",
                    replay_path.display(),
                    hex(&replay.map_md5)
                )
                .into());
            }
            if replay.ruleset == rsu_formats::osr::STABLE_RULESET {
                eprintln!("rsu: an osu! stable replay; rsu's judgement may differ from stable's");
            } else if replay.ruleset != RULESET_VERSION {
                eprintln!(
                    "rsu: replay was recorded under rules v{}, this build has v{RULESET_VERSION}; the result may differ",
                    replay.ruleset
                );
            }
            eprintln!("rsu: replay by '{}', {} inputs", replay.player, replay.inputs.len());
            let expected = Some(replay.summary).filter(|_| replay.ruleset == RULESET_VERSION);
            let end = replay.end_time.max(map.chart.objects.last().map_or(0, |o| o.end.max(o.t)));
            let playback = Playback::new(&map.chart_for(replay.mods), replay.mods, replay.inputs, end, expected);
            first_playback = Some(match stable_frames {
                Some(frames) => playback.with_stable_visual(&frames),
                None => playback,
            });
            (replay.mods, replay.rate_pct)
        } else {
            (args.mods, args.rate_pct)
        };
        let music = map.audio_path.as_ref().and_then(|p| jukebox.open(p));
        current = Some(Current { map, music, stable_source: args.replay.as_ref().is_some_and(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("osr"))), mods: game_mods, rate_pct, auto: args.auto && args.replay.is_none(), practice: args.practice });
    } else if args.pattern {
        let grid = BeatGrid {
            period_frames: 60.0 / args.bpm * SAMPLE_RATE as f64,
            offset_frames: args.beat_offset_ms / 1000.0 * SAMPLE_RATE as f64,
        };
        let track = args.audio.as_ref().and_then(|p| jukebox.open(p));
        let demo = Demo::new(grid, settings.offset_ms / 1000.0 * SAMPLE_RATE as f64, args.calibrate);
        pattern = Some((demo, grid, track));
    } else if settings.songs.is_none() {
        eprintln!("rsu: no songs folder found (pass --songs DIR)");
    }
    let (tap_table, mut tap) = session::tap_sound(&settings);

    // What the options change while the program runs.
    let mut tun = Tunables {
        master: config.master_volume,
        music: settings.music_volume,
        effects: settings.effect_volume,
        offset_ms: settings.offset_ms as f32,
        dim: settings.background_dim,
        cursor_scale: config.cursor_scale,
        replay_smooth_trail: config.replay_smooth_trail,
        songselect_top: config.songselect_top,
        songselect_bottom: config.songselect_bottom,
        fps_cap: args.fps_cap.or(config.fps_cap).unwrap_or(1000),
        beatmap_hitsounds: settings.beatmap_hitsounds,
        // The measurements are the subject of the test pattern; elsewhere
        // they are one key away.
        show_overlay: pattern.is_some(),
        binds: config.binds,
    };
    let tun_at_start = tun;
    let mut applied = tun;

    // ---- audio -----------------------------------------------------------
    let (cmd_tx, cmd_rx) = spsc::channel::<AudioCmd>(256);
    let (clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
    let (latency_tx, mut latency_rx) = spsc::channel::<u64>(256);
    let (retired_tx, retired_rx) = spsc::channel(64);
    let pattern_track = pattern.as_ref().and_then(|(_, _, track)| track.clone());
    let mut mixer = Mixer::new(
        MixerConfig {
            music_gain: tun.music * tun.master,
            music_start: 0,
            metronome: pattern.as_ref().map(|(_, grid, _)| *grid),
            // With a track the grid is only drawn; without one it is heard.
            metronome_gain: if pattern_track.is_some() { 0.0 } else { tun.music * tun.master },
            music: pattern_track,
            samples: tap_table,
        },
        cmd_rx,
        clock_tx,
        latency_tx,
        retired_tx,
    );
    let pw = if args.audio_enabled {
        let opts = PwOptions { sample_rate: SAMPLE_RATE, channels: 2, quantum: args.quantum };
        match audio_pw::start(opts, move |out, t| {
            mixer.process(out, Cycle { now_ns: t.now_ns, delay_frames: t.delay_frames })
        }) {
            Ok(pw) => Some(pw),
            Err(e) => {
                eprintln!("rsu: no audio ({e}); running on the system clock");
                None
            }
        }
    } else {
        None
    };
    let mut transport = Transport::new(cmd_tx, clock_rx, retired_rx, pw.is_some(), now_ns());

    // ---- input -----------------------------------------------------------
    let mut input = if args.evdev {
        match evdev::start() {
            Ok(i) if !i.devices.is_empty() => Some(i),
            Ok(_) => {
                eprintln!(
                    "rsu: no readable input devices in /dev/input (add your user to the 'input' \
                     group for the low-latency path); using window-system input"
                );
                None
            }
            Err(e) => {
                eprintln!("rsu: evdev unavailable ({e}); using window-system input");
                None
            }
        }
    } else {
        None
    };
    if let Some(i) = &input {
        for d in &i.devices {
            eprintln!("rsu: input {:?}: {}", d.kind, d.name);
        }
    }
    let evdev_keys = input.as_ref().is_some_and(|i| i.has_keyboard());

    // ---- window and renderer --------------------------------------------
    let mut window = Window::open(&WindowOptions {
        title: "rsu".into(),
        fullscreen: args.fullscreen,
        size: (1280, 720),
        backend: args.backend,
    })?;
    let surface = match window.native() {
        NativeSurface::Wayland { display, surface } => SurfaceHandle::Wayland { display, surface },
        NativeSurface::Xcb { connection, window } => SurfaceHandle::Xcb { connection, window },
    };
    // SAFETY: `gfx` is declared after `window`, so it is dropped first and
    // never outlives the handles it was created from.
    let mut gfx = unsafe { Gfx::new(surface, window.size(), args.present)? };
    eprintln!(
        "rsu: {} on {}, present modes {:?}, using {}",
        gfx.device_name(),
        window.backend_name(),
        gfx.available_modes().iter().map(|m| m.name()).collect::<Vec<_>>(),
        gfx.present_mode().name()
    );

    // ---- images: the font first, so that it stays put when the skin on
    // top of it is exchanged. The images of the menus have an atlas of
    // their own.
    let (mut atlas, mut ui_atlas) = (Atlas::new(), Atlas::new());
    match text::system_font().map(|data| text::Font::rasterise(&data, &mut atlas)) {
        Some(Ok(font)) => text::install(font),
        Some(Err(e)) => eprintln!("rsu: the system font cannot be used ({e}); using the built-in one"),
        None => eprintln!("rsu: no system font found; using the built-in one"),
    }
    let font_end = atlas.mark();
    let ui_start = ui_atlas.mark();
    let mut skin = load_skin(settings.skin.as_deref(), &mut atlas, &mut ui_atlas);
    gfx.upload_atlas(atlas.rows(), atlas.pixels())?;
    gfx.upload(PAGE_UI, rsu_gfx::ATLAS_SIZE, ui_atlas.rows(), ui_atlas.pixels())?;

    // ---- the first screen --------------------------------------------------
    let backgrounds = Backgrounds::new();
    // The background that was asked for, and the one that is showing.
    let mut wanted_picture: Option<PathBuf> = None;
    let mut backdrop = Backdrop::default();
    let mut browser: Option<Browser> = None;
    let sort_at_start = config.sort.as_deref().and_then(Sort::named).unwrap_or(Sort::DateAdded);
    let group_at_start = config.group.as_deref().and_then(Group::named).unwrap_or(Group::None);
    let mut screen = if let Some((demo, _, _)) = pattern.take() {
        Screen::Pattern(demo)
    } else if let Some(cur) = &current {
        let now = now_ns();
        let play = begin_play(cur, first_playback.take(), &settings, &mut transport, now);
        // Viewing options: where and how fast to start.
        if play.playback.is_some() {
            if let Some(speed) = args.speed {
                transport.set_speed(play.speed() * speed, now);
            }
            if let Some(ms) = args.start_ms {
                let pos = play.timeline_frames((ms * 1000.0) as i64);
                transport.seek(pos.clamp(0.0, play.timeline_frames(play.end_time())), now);
            }
        }
        if let Some(picture) = &cur.map.background {
            wanted_picture = Some(picture.clone());
            backgrounds.request(picture.clone());
        }
        Screen::Play(play)
    } else {
        let mut b = Browser::new(settings.songs.clone().map(Scan::start), sort_at_start);
        b.set_group(group_at_start);
        browser = Some(b);
        Screen::Menu
    };
    let mut menu = MainMenu::new();
    let mut options: Option<Options> = None;
    let mut modselect: Option<ModSelect> = None;
    let mut pause: Option<PauseMenu> = None;
    let mut notice = Notice::new();
    let mut volume = VolumeOverlay::new();
    // The main menu chooses a song of its own once, when the library is there.
    let mut menu_song_chosen = false;
    // The result screen to come back to after watching its replay.
    let mut last_results: Option<Box<Results>> = None;

    let mut absmap = AbsMap::new();
    absmap.configure(input.as_ref().map_or(&[], |i| i.devices.as_slice()), args.fullscreen.then(|| gfx.extent()));
    let mut metrics = Metrics::new();
    let display_latency_ns = (args.display_latency_ms.max(0.0) * 1e6) as u64;
    let mut win_events: Vec<WinEvent> = Vec::with_capacity(256);
    let mut focused = false;
    let mut quit = false;
    let started = now_ns();
    let mut next_frame = started;
    let mut last_frame_ns = started;
    let mut last_present = 0u64;
    let mut last_latched_ns = 0u64;
    // Frames are numbered so the compositor's answers about them (when
    // they reached the screen) can be matched up.
    let mut frame_tag = 0u64;
    let mut feedback: Vec<FrameFeedback> = Vec::with_capacity(64);
    let mut idle_inhibited = false;
    let mut last_motion_ns = [0u64; 32];
    let mut last_clock_t0 = 0u64;
    let mut keys = [KeyEvent::default(); 64];
    let mut ui_keys = [0u32; 64];
    let mut browser_repeat = BrowserRepeat::default();
    // Characters typed in this frame.
    let mut ui_text = String::with_capacity(32);
    let mut buttons = Buttons::default();
    let mut smoke = smoke::Smoke::default();
    let mut trail = smoke::Smoke::trail();
    let mut smoke_held = false;
    // Since when the quick retry key has been down.
    let mut retry_since: Option<u64> = None;
    let mut alt_down = false;
    let mut ctrl_down = false;
    let mut shift_down = [false; 2];
    let mut fps_meter = fps::FpsMeter::default();
    let script = key_script();
    let mut script_at = 0;
    // Where the script has put the pointer, and for how many more frames
    // it holds the button down.
    let mut script_pointer: Option<(f32, f32)> = None;
    let mut script_hold = 0u32;

    while !quit {
        // ---- step 0: pacing. Vsynced modes are paced by acquire/present.
        let cap_ns = if tun.fps_cap > 0 { 1_000_000_000 / tun.fps_cap as u64 } else { 0 };
        if cap_ns > 0 && !gfx.present_mode().is_vsynced() {
            next_frame += cap_ns;
            let now = now_ns();
            if next_frame <= now {
                next_frame = now;
            } else {
                sleep_until_precise(next_frame, 60_000);
            }
        }
        if let Some(limit) = args.smoke_s {
            quit |= (now_ns() - started) as f64 / 1e9 >= limit;
        }

        let clock = transport.clock();
        let (w, h) = gfx.extent();
        let field = Field::new(w as f32, h as f32);
        let ui_metrics = ui::Metrics::new(w as f32, h as f32);
        let fullscreen_size = args.fullscreen.then_some((w, h));
        let paused = pause.is_some();
        let mut keys_len = 0;
        let mut scrolled = 0.0f32;
        ui_text.clear();
        let mut lost_focus = false;
        let mut push_key = |code: u32, pressed: bool, t_ns: u64| {
            if keys_len < keys.len() {
                keys[keys_len] = KeyEvent { code, pressed, t_ns };
                keys_len += 1;
            }
        };

        // Drains the evdev queue: pointer samples into the mapping and the
        // game, keys and buttons into this frame's lists. A macro because
        // it runs twice a frame and borrows half the loop's state.
        macro_rules! drain_evdev {
            () => {
                if let Some(inp) = &mut input {
                    while let Some(ev) = inp.events.pop() {
                        match ev.kind {
                            InputKind::Motion => {
                                let sample = CursorSample { t_ns: ev.t_ns, x: ev.x, y: ev.y, dev: ev.dev };
                                absmap.on_sample(sample);
                                let last = &mut last_motion_ns[ev.dev as usize % 32];
                                if *last != 0 {
                                    metrics.pointer_interval.record(ev.t_ns - *last);
                                }
                                *last = ev.t_ns;
                                if let (Screen::Play(play), true, false) = (&mut screen, clock.valid, paused) {
                                    let c = absmap.cursor(Some(sample), &inp.devices, fullscreen_size, ev.t_ns);
                                    if c.source.is_evdev() {
                                        play.queue_move(&clock, ev.t_ns, &field, c.x, c.y);
                                    }
                                }
                            }
                            InputKind::Key => push_key(ev.code as u32, ev.pressed, ev.t_ns),
                            InputKind::Button => {
                                // The device is read whatever window has the
                                // focus; a press only counts when ours has.
                                if (ev.code == BTN_TOUCH || ev.code == BTN_LEFT) && (focused || !ev.pressed) {
                                    buttons.set(true, ev.pressed);
                                } else if focused || !ev.pressed {
                                    // The other buttons of a pen can be bound like keys.
                                    push_key(ev.code as u32, ev.pressed, ev.t_ns);
                                }
                            }
                        }
                    }
                }
            };
        }

        // The pointer samples must be known before the window events they
        // caused are looked at: a window event is matched against the
        // samples recorded so far.
        drain_evdev!();
        if script_hold > 0 {
            script_hold -= 1;
            if script_hold == 0 {
                buttons.set(false, false);
            }
        }
        while let Some(&(_, what)) = script.get(script_at).filter(|(at, _)| at * 1_000_000 <= now_ns() - started) {
            match what {
                Scripted::Key(code) => {
                    let t = now_ns();
                    push_key(code, true, t);
                    push_key(code, false, t);
                    // A scripted key types what a US keyboard would; without
                    // window text the key loop below does that already.
                    if window.has_text() {
                        ui_text.extend(ui::key_char(code));
                    }
                }
                Scripted::Hold(code, down) => push_key(code, down, now_ns()),
                Scripted::Move(x, y) => script_pointer = Some((x, y)),
                Scripted::Click(x, y) => {
                    script_pointer = Some((x, y));
                    buttons.set(false, true);
                    script_hold = 3;
                }
                Scripted::Wheel(n) => scrolled += n,
            }
            script_at += 1;
        }

        // ---- window events (low rate; libwayland allocates, so this stays
        // outside the measured hot section).
        win_events.clear();
        window.pump(&mut win_events);
        let pump_ns = now_ns();
        feedback.clear();
        window.take_feedback(&mut feedback);
        for fb in &feedback {
            metrics.screen.answer(fb);
        }
        // While evdev drives the cursor, window pointer events are echoes
        // of samples the game already has, with worse timestamps.
        let window_pointer = !metrics.cursor_source.is_evdev();
        let window_text = window.has_text();
        for ev in &win_events {
            match *ev {
                WinEvent::CloseRequested => quit = true,
                WinEvent::Resized(w, h) => {
                    gfx.resize(w, h);
                    let devices = input.as_ref().map_or(&[][..], |i| i.devices.as_slice());
                    absmap.configure(devices, args.fullscreen.then_some((w, h)));
                }
                WinEvent::Focus(f) => {
                    lost_focus |= focused && !f;
                    focused = f;
                    if let Some(i) = &input {
                        i.focused.store(f, Ordering::Relaxed);
                    }
                    if !f {
                        buttons.set(true, false);
                        buttons.set(false, false);
                        alt_down = false;
                        ctrl_down = false;
                        shift_down = [false; 2];
                    }
                }
                WinEvent::PointerMotion { x, y, time_ms } => {
                    absmap.on_window_motion(x, y, time_ms, pump_ns);
                    if let (Screen::Play(play), true, true, false) = (&mut screen, window_pointer, clock.valid, paused)
                    {
                        play.queue_move(&clock, pump_ns, &field, x, y);
                    }
                }
                WinEvent::PointerLeave => {
                    if script_hold == 0 {
                        buttons.set(false, false);
                    }
                }
                WinEvent::PointerButton { pressed } => buttons.set(false, pressed),
                WinEvent::Scroll(notches) => scrolled += notches,
                WinEvent::Text(c) => ui_text.push(c),
                WinEvent::Key { code, pressed } => {
                    // With evdev keyboards the same key arrives there first
                    // and better timestamped; this path is the fallback.
                    if !evdev_keys {
                        push_key(code, pressed, pump_ns);
                    }
                }
            }
        }

        // ---- step 1: acquire. The only place a vsynced mode blocks.
        let t_start = now_ns();
        if !gfx.acquire()? {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        // A stable replay's trail is baked into the batch, unlike the live
        // late-latched pointer. Wait *before* sampling its map time so a slow
        // previous GPU frame cannot age the whole replay picture in flight.
        // Native replays and live play keep their existing late-latch order.
        if matches!(&screen, Screen::Play(play) if play.playback.as_ref().is_some_and(|pb| pb.visual.is_some())) {
            gfx.wait_previous()?;
        }
        let t_acquired = now_ns();
        // Before the hot section: libwayland allocates for the request.
        // It attaches to this frame's commit, made inside submit_present.
        let asked = window.request_feedback(frame_tag);
        let hot = HotSection::enter();

        // ---- step 2: logic. Drain what arrived while acquiring; judgement
        // uses the events' own timestamps, so time spent in the queue costs
        // nothing.
        drain_evdev!();
        let now = now_ns();
        let latest = input.as_ref().map(|i| i.latest.read());
        let devices = input.as_ref().map(|i| i.devices.as_slice()).unwrap_or(&[]);
        let pointer = absmap.cursor(latest, devices, fullscreen_size, now);
        let frame_dt = ((now - last_frame_ns) as f32 / 1e9).min(0.1);
        last_frame_ns = now;

        // What the options changed in the last frame takes effect.
        if tun != applied {
            if tun.master != applied.master || tun.music != applied.music {
                transport.set_music_gain(tun.master * tun.music);
            }
            settings.music_volume = tun.music;
            settings.effect_volume = tun.effects;
            settings.offset_ms = tun.offset_ms as f64;
            settings.beatmap_hitsounds = tun.beatmap_hitsounds;
            settings.background_dim = tun.dim;
            applied = tun;
        }
        let effect_gain = tun.effects * tun.master;

        let mut next: Option<Transition> = None;
        // The sound the menus make in this frame.
        let mut sound: Option<UiSound> = None;
        let ticks_before = browser.as_ref().map_or(0, |b| b.ticks);
        let switches_before = (tun.show_overlay, tun.beatmap_hitsounds, browser.as_ref().map(|b| (b.mods, b.auto)));
        let mut view_action: Option<ViewAction> = None;
        // The skip key was pressed, or a hit key went down this frame.
        let mut skip_key = false;
        let mut hit_down = false;
        // Hit-key presses of this frame, for the key -> present measurement.
        let mut pressed_at = [0u64; 16];
        let mut pressed_len = 0;
        let mut ui_len = 0;
        // A play someone is playing, as opposed to one being watched.
        let live = matches!(&screen, Screen::Play(play) if play.playback.is_none());
        let mut shift_at_f2 = None;
        for k in &keys[..keys_len] {
            if matches!(&screen, Screen::Select) && options.is_none() && modselect.is_none() && focused && !lost_focus {
                browser_repeat.key(k);
            }
            // Keys that mean the same on every screen.
            if k.code == KEY_LEFT_ALT || k.code == KEY_RIGHT_ALT {
                alt_down = k.pressed;
            }
            if k.code == KEY_LEFT_CTRL || k.code == KEY_RIGHT_CTRL {
                ctrl_down = k.pressed;
            }
            if k.code == KEY_LEFT_SHIFT {
                shift_down[0] = k.pressed;
            } else if k.code == KEY_RIGHT_SHIFT {
                shift_down[1] = k.pressed;
            } else if k.code == ui::KEY_F2 && k.pressed {
                shift_at_f2 = Some(shift_down[0] || shift_down[1]);
            }
            // Ctrl+O: the options, from wherever they can be reached (McOsu
            // `toggleOptionsMenu`).
            if k.pressed && ctrl_down && !alt_down && k.code == KEY_O && next.is_none() {
                let reachable = matches!(&screen, Screen::Menu | Screen::Select | Screen::Results(_));
                if options.is_some() {
                    next = Some(Transition::CloseOptions);
                } else if reachable {
                    next = Some(Transition::OpenOptions);
                }
            }
            if k.pressed {
                if ui_len < ui_keys.len() {
                    ui_keys[ui_len] = k.code;
                    ui_len += 1;
                }
                // Where the window cannot say what a key types, assume a US
                // layout.
                if !window_text && !ctrl_down {
                    ui_text.extend(ui::key_char(k.code));
                }
                match k.code {
                    KEY_F9 => tun.show_overlay = !tun.show_overlay,
                    KEY_F11 => metrics.reset(),
                    KEY_F10 => next = Some(Transition::NextPresentMode),
                    _ => {}
                }
            }
            let bound = |action| binds::is(&tun.binds, action, k.code);
            // Ctrl+R is a quick retry too, held as long.
            quick_retry_key(
                &mut retry_since,
                live,
                bound(binds::QUICK_RETRY) || (ctrl_down && !alt_down && k.code == KEY_R),
                k,
            );
            let hit_key = if bound(binds::LEFT_CLICK) || bound(binds::LEFT_CLICK_2) {
                Some(Button::K1)
            } else if bound(binds::RIGHT_CLICK) || bound(binds::RIGHT_CLICK_2) {
                Some(Button::K2)
            } else {
                None
            };
            match &mut screen {
                Screen::Pattern(_) => {
                    if k.pressed && hit_key.is_some() && pressed_len < pressed_at.len() {
                        pressed_at[pressed_len] = k.t_ns;
                        pressed_len += 1;
                    } else if k.pressed && k.code == KEY_ESC {
                        next = Some(Transition::Quit);
                    }
                }
                Screen::Play(play) if !paused => {
                    if live && bound(binds::SMOKE) {
                        smoke_held = k.pressed;
                    }
                    // Enter always skips; Space does when nothing else uses it.
                    skip_key |= k.pressed && (k.code == ui::KEY_ENTER || (live && k.code == KEY_SPACE));
                    hit_down |= k.pressed && hit_key.is_some();
                    if let Some(button) = hit_key {
                        if k.pressed && pressed_len < pressed_at.len() {
                            pressed_at[pressed_len] = k.t_ns;
                            pressed_len += 1;
                        }
                        if clock.valid {
                            play.queue_button(&clock, k.t_ns, button, k.pressed);
                        }
                    } else if k.pressed {
                        // The pause key does what Esc does.
                        let code = if bound(binds::GAME_PAUSE) { KEY_ESC } else { k.code };
                        match code {
                            c if bound(binds::INCREASE_OFFSET) && c == k.code => play.nudge_offset(1),
                            c if bound(binds::DECREASE_OFFSET) && c == k.code => play.nudge_offset(-1),
                            // A play given on the command line, and one that
                            // is only being watched, end at once.
                            KEY_ESC if oneshot || !live => next = Some(Transition::Leave),
                            KEY_ESC => next = Some(Transition::Pause),
                            // The offset, a millisecond at a time.
                            KEY_MINUS => play.nudge_offset(-1),
                            KEY_EQUAL => play.nudge_offset(1),
                            KEY_LEFT => view_action = Some(ViewAction::SeekBy(-5.0)),
                            KEY_RIGHT => view_action = Some(ViewAction::SeekBy(5.0)),
                            KEY_UP => view_action = Some(ViewAction::SpeedStep(1)),
                            KEY_DOWN => view_action = Some(ViewAction::SpeedStep(-1)),
                            KEY_SPACE => view_action = Some(ViewAction::TogglePause),
                            KEY_HOME => view_action = Some(ViewAction::Restart),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        // The key press behind this frame's clicked sounds, for measuring
        // how long the sound takes to be heard.
        let t_key = if pressed_len > 0 { pressed_at[pressed_len - 1] } else { 0 };
        // Holding the quick retry key long enough starts the map again.
        if !live || paused {
            smoke_held = false;
        }
        if quick_retry_ready(&mut retry_since, live, now_ns()) && next.is_none() {
            retry_since = None;
            next = Some(Transition::Retry);
        }

        let repeat_active = matches!(&screen, Screen::Select) && options.is_none() && modselect.is_none()
            && focused && !lost_focus && !alt_down && next.is_none();
        if let Some(code) = browser_repeat.update(repeat_active, now) {
            if ui_len < ui_keys.len() {
                ui_keys[ui_len] = code;
                ui_len += 1;
            }
        }

        // The frame as the menus see it.
        let (pressed, down) = buttons.frame();
        let (pointer_x, pointer_y) = script_pointer.unwrap_or((pointer.x, pointer.y));
        {
            // The trail is made of the skin's own image, a quarter of its
            // width apart.
            let unit = h as f32 / 768.0;
            let unit_trail = unit * tun.cursor_scale;
            let spacing = skin.cursor_smoke.as_ref().filter(|s| s.visible()).map_or(6.0, |s| s.w * unit * 0.25);
            smoke.update(now, pointer_x, pointer_y, smoke_held, spacing);
            // Both replay formats sample the recorded cursor in map time;
            // only live play uses the physical pointer's frame history.
            let replay_view = !paused && matches!(&screen, Screen::Play(play) if play.playback.is_some());
            match skin.cursor_trail.as_ref().filter(|s| s.visible()) {
                Some(s) => trail.update(now, pointer_x, pointer_y, !replay_view, s.w * unit_trail * 0.25),
                None => trail.update(now, pointer_x, pointer_y, false, 1.0),
            }
        }
        let mut ui_input = ui::Input {
            x: pointer_x,
            y: pointer_y,
            pressed,
            down,
            scroll: scrolled,
            keys: &ui_keys[..ui_len],
            text: &ui_text,
            dt: frame_dt,
            now_ns: now,
        };
        // The wheel is the volume where it has nothing to scroll, and
        // everywhere with Alt held.
        let covered = options.is_some() || modselect.is_some();
        let wheel_is_volume = match &screen {
            Screen::Play(_) => !paused,
            Screen::Menu => !covered,
            Screen::Select | Screen::Results(_) => alt_down,
            _ => false,
        };
        if wheel_is_volume && scrolled != 0.0 {
            tun.master = ((tun.master - scrolled * 0.05) * 100.0).round().clamp(0.0, 100.0) / 100.0;
            volume.poke(now);
            ui_input.scroll = 0.0;
        }
        let below = if covered { ui_input.covered() } else { ui_input };
        // The song the menus play, for the player in the main menu.
        let mut music_action: Option<MenuAction> = None;
        let music_view = match (&screen, jukebox.song(), jukebox.progress(&transport, &clock, now)) {
            (Screen::Menu, Some(song), Some(p)) => {
                Some(MusicView { title: &song.title, seconds: p.seconds, length: p.length, paused: jukebox.paused })
            }
            _ => None,
        };

        match &mut screen {
            Screen::Pattern(demo) => {
                for &t in &pressed_at[..pressed_len] {
                    if clock.valid {
                        let err = demo.error_ms(clock.pos_at(t) - demo.offset_frames);
                        metrics.hit_error.push(err);
                        demo.push_error(err as f32);
                        demo.last_hit_ns = t;
                    }
                    transport.play(&tap, effect_gain, t);
                }
                if clock.valid {
                    // Drawn on the clock the taps are judged on.
                    demo.set_phase(clock.pos_at(now + display_latency_ns) - demo.offset_frames);
                }
            }
            Screen::Menu => match menu.update(&below, &ui_metrics, music_view.as_ref()) {
                MenuAction::Play => next = Some(Transition::OpenBrowser),
                MenuAction::Options => next = Some(Transition::OpenOptions),
                MenuAction::Exit => next = Some(Transition::Quit),
                MenuAction::None => {}
                song => music_action = Some(song),
            },
            Screen::Select => {
                if let Some(list) = &mut browser {
                    list.ctrl = ctrl_down;
                    list.shift = shift_at_f2.unwrap_or(shift_down[0] || shift_down[1]);
                    match list.update(&below, &ui_metrics, &skin) {
                        BrowserAction::Start => next = Some(Transition::Start),
                        BrowserAction::Back => next = Some(Transition::ToMenu),
                        BrowserAction::Mods => next = Some(Transition::OpenMods),
                        BrowserAction::Options => next = Some(Transition::OpenOptions),
                        BrowserAction::ShowScore(index) => next = Some(Transition::ShowScore(index)),
                        BrowserAction::None => {}
                    }
                }
            }
            Screen::Play(play) => {
                if let Some(menu) = &mut pause {
                    match menu.update(&ui_input, &ui_metrics, &skin) {
                        PauseAction::Continue => next = Some(Transition::Resume),
                        PauseAction::Retry => next = Some(Transition::Retry),
                        PauseAction::Quit => next = Some(Transition::Leave),
                        PauseAction::None => {}
                    }
                } else if live && !oneshot && lost_focus && next.is_none() {
                    next = Some(Transition::Pause);
                }
                if matches!(next, Some(Transition::Pause)) && clock.valid {
                    // Whatever is held is let go: the keys come up unseen
                    // while the game is stopped.
                    play.queue_button(&clock, now, Button::K1, false);
                    play.queue_button(&clock, now, Button::K2, false);
                }
                if clock.valid && !paused {
                    // The skip button: a key, or a click on it.
                    let on_button = (pressed || hit_down)
                        && Play::skip_rect(&skin, w as f32, h as f32).contains(pointer_x, pointer_y);
                    if skip_key || on_button {
                        if let Some(target) = play.skip_target() {
                            transport.seek(play.timeline_frames(target), now);
                            sound = Some(UiSound::Hit);
                        }
                    }
                }
                if clock.valid {
                    // Viewer controls act on the transport; playback then
                    // simply follows the clock to wherever it went.
                    if play.playback.is_some() {
                        let limit = play.timeline_frames(play.end_time());
                        match view_action {
                            Some(ViewAction::SeekBy(seconds)) => {
                                let pos = clock.pos_at(now) + seconds * SAMPLE_RATE as f64 * transport.speed;
                                transport.seek(pos.clamp(0.0, limit), now);
                            }
                            Some(ViewAction::Restart) => transport.seek(0.0, now),
                            Some(ViewAction::SpeedStep(step)) => {
                                let at = SPEEDS.iter().position(|s| *s >= transport.speed).unwrap_or(3) as i32;
                                let speed = SPEEDS[(at + step).clamp(0, SPEEDS.len() as i32 - 1) as usize];
                                transport.set_speed(speed, now);
                            }
                            Some(ViewAction::TogglePause) => transport.set_paused(!transport.paused, now),
                            None => {}
                        }
                    }
                    play.view_speed = transport.speed as f32;
                    play.view_paused = transport.paused;
                    play.update(&clock, now);
                    for cue in play.cues() {
                        match cue.at {
                            Some(at) => transport.play_at(&cue.sound, effect_gain, at),
                            None => transport.play(&cue.sound, effect_gain, if cue.from_click { t_key } else { 0 }),
                        }
                    }
                    transport.set_loops(play.loops(), effect_gain);
                    metrics.hit_error = *play.hit_error();
                    next = play_transition(next, pause.is_some(), play.retry_requested, play.failed, play.done);
                }
            }
            Screen::Results(results) => {
                next = match results.update(&below, &ui_metrics, &skin) {
                    ResultsAction::Back => Some(Transition::Back),
                    ResultsAction::Retry => Some(Transition::Retry),
                    ResultsAction::Watch => Some(Transition::Watch),
                    ResultsAction::None => next,
                };
            }
            Screen::Done => {}
        }
        if update_modselect(&mut modselect, &mut browser, options.is_some(), &ui_input, &ui_metrics, &skin) {
            next = Some(Transition::CloseMods);
        }
        if browser.as_ref().map_or(0, |b| b.ticks) != ticks_before {
            sound = Some(UiSound::Click);
        }
        while let Some(ns) = latency_rx.pop() {
            metrics.key_to_sound.record(ns);
        }
        if clock.valid && clock.t0_ns != last_clock_t0 {
            last_clock_t0 = clock.t0_ns;
            metrics.clock_resid.record(clock.resid_ns.abs() as u64);
        }

        // ---- step 3: build the frame.
        let present_name = gfx.present_mode().name();
        {
            let batch = gfx.batch();
            let (wf, hf) = (w as f32, h as f32);
            match &mut screen {
                Screen::Pattern(demo) => demo.draw(batch, wf, hf, now),
                Screen::Menu => {
                    if let Some(background) = skin.menu.background.filter(|s| s.visible() && s.h > 0.0) {
                        let scale = (wf / background.w).max(hf / background.h);
                        background.draw(batch, wf * 0.5, hf * 0.5, scale, ui::WHITE);
                    }
                    // The square beats with the song that is playing.
                    let beat = jukebox.beat_phase(&clock, now);
                    menu.draw(batch, &ui_metrics, beat, music_view.as_ref(), now);
                }
                Screen::Select => {
                    if let Some(list) = &mut browser {
                        list.draw(batch, &ui_metrics, &skin, &backdrop, now, tun.songselect_top, tun.songselect_bottom);
                    }
                }
                Screen::Play(play) => {
                    let lit = ((1.0 - tun.dim).clamp(0.0, 1.0) * 255.0) as u8;
                    backdrop.draw(batch, &ui_metrics, [lit, lit, lit, 255]);
                    play.draw(batch, &skin, wf, hf);
                    if let Some(menu) = &pause {
                        menu.draw(batch, &ui_metrics, &skin, &ui_input);
                    }
                }
                Screen::Results(results) => results.draw(batch, &ui_metrics, &skin, &backdrop, &below),
                Screen::Done => {}
            }
            if let (Some(selection), Some(list)) = (&modselect, &browser) {
                draw_modselect(selection, list, options.is_some(), batch, &ui_metrics, &skin, &ui_input);
            }
            if let Some(panel) = &mut options {
                match panel.frame(batch, &ui_input, &ui_metrics, &mut tun) {
                    OptionsAction::Close => next = Some(Transition::CloseOptions),
                    OptionsAction::NextPresentMode => next = Some(Transition::NextPresentMode),
                    OptionsAction::Skin(index) => next = Some(Transition::SetSkin(index)),
                    OptionsAction::ApplySongs => next = Some(Transition::SetSongs),
                    OptionsAction::None => {}
                }
            }
            notice.draw(batch, &ui_metrics, now);
            volume.draw(batch, &ui_metrics, now, [tun.master, tun.music, tun.effects]);
            if !tun.show_overlay && !matches!(screen, Screen::Pattern(_)) {
                fps_meter.draw(batch, &ui_metrics, now);
                // The first seconds of a map: what the display runs at.
                if let (Screen::Play(play), Some(hz)) = (&screen, window.refresh_hz()) {
                    if !paused {
                        fps::draw_refresh(batch, &ui_metrics, hz, play.now as f32 / 1e6);
                    }
                }
            }
            if tun.show_overlay {
                let info = overlay::Info {
                    backend: window.backend_name(),
                    present: present_name,
                    fps_cap: tun.fps_cap,
                    source: pointer.source,
                    fit_rms: latest.and_then(|s| absmap.fit_rms(s.dev)),
                    input_rt: input.as_ref().is_some_and(|i| i.stats.realtime.load(Ordering::Relaxed)),
                    evdev_keys,
                    queue_overflows: input.as_ref().map_or(0, |i| i.stats.queue_overflows.load(Ordering::Relaxed)),
                    kernel_drops: input.as_ref().map_or(0, |i| i.stats.kernel_drops.load(Ordering::Relaxed)),
                    clock,
                    audio: pw.is_some(),
                    focused,
                };
                overlay::draw(batch, &mut metrics, &info, wf, hf, now);
            }
            // The cursor is two shapes positioned relative to a uniform the
            // CPU fills in at the last moment; nothing here knows where it is.
            // Both replay formats sample the displayed cursor's recorded
            // path, never the physical mouse. The same map time feeds the
            // objects and cursor, independent of seek, pause and render FPS.
            let cursor_opacity = match &screen {
                Screen::Play(play) => fading_cursor_opacity(play.practice.fading_cursor, paused || play.view_paused, play.game.combo()),
                _ => 1.0,
            };
            let replay = match &screen {
                Screen::Play(play) if !paused => play.playback.as_ref().map(|pb| (play, pb)),
                _ => None,
            };
            if let Some((play, pb)) = replay {
                let field = Field::new(wf, hf);
                let unit = hf / 768.0 * tun.cursor_scale;
                let sprite = skin.cursor_trail.as_ref();
                let mut stamp = |(x, y), alpha| {
                    let (x, y) = field.to_screen(x, y);
                    draw_replay_trail_stamp(batch, sprite, x, y, unit, alpha, cursor_opacity);
                };
                if tun.replay_smooth_trail {
                    pb.trail(play.now, &mut stamp);
                } else {
                    let joined = skin.cursor_middle.is_some();
                    let width = sprite.map_or(14.0, |s| s.w) * unit;
                    let spacing = width / (2.5 * tun.cursor_scale.max(1.0) * field.scale.max(0.001));
                    pb.skin_trail(play.now, joined, spacing, &mut stamp);
                }
            } else if !trail.is_empty() {
                trail.draw(batch, skin.cursor_trail.as_ref(), hf / 768.0 * tun.cursor_scale, now, cursor_opacity);
            }
            if !smoke.is_empty() {
                smoke.draw(batch, skin.cursor_smoke.as_ref(), hf / 768.0, now, 1.0);
            }
            draw_game_cursor(batch, &skin, hf / 768.0 * tun.cursor_scale, cursor_opacity);
        }
        let t_built = now_ns();
        // What the frame did, as a sound: a switch thrown, or the way on
        // or back that was taken.
        let switches = (tun.show_overlay, tun.beatmap_hitsounds, browser.as_ref().map(|b| (b.mods, b.auto)));
        if switches != switches_before {
            let more = match (switches.2, switches_before.2) {
                (Some(now), Some(before)) if now != before => now.0 != 0 && now.0 != before.0 || now.1 && !before.1,
                _ => (switches.0 && !switches_before.0) || (switches.1 && !switches_before.1),
            };
            sound = Some(if more { UiSound::CheckOn } else { UiSound::CheckOff });
        }
        sound = match &next {
            Some(Transition::OpenBrowser | Transition::Start | Transition::Retry | Transition::Watch) => {
                Some(UiSound::Hit)
            }
            Some(Transition::Resume) => Some(UiSound::Hit),
            Some(Transition::ToMenu | Transition::Back | Transition::CloseOptions | Transition::CloseMods) => {
                Some(UiSound::Back)
            }
            Some(Transition::Leave) if pause.is_some() => Some(UiSound::Back),
            Some(Transition::OpenOptions | Transition::OpenMods | Transition::ShowScore(_)) => Some(UiSound::Click),
            Some(Transition::NextPresentMode | Transition::SetSkin(_)) => Some(UiSound::Click),
            _ => sound,
        };
        if let Some(sound) = sound {
            transport.play(&sound.ids(), effect_gain, 0);
        }

        // ---- step 4: record commands.
        gfx.record([0.0, 0.0, 0.0, 1.0])?;
        let t_recorded = now_ns();

        // ---- step 5: let the previous frame leave the GPU, then latch the
        // newest cursor sample: after everything else, right before submit.
        gfx.wait_previous()?;
        let t_waited = now_ns();
        let latest = input.as_ref().map(|i| i.latest.read());
        let cursor = absmap.cursor(latest, devices, fullscreen_size, t_waited);
        let (x, y) = frame_cursor(&screen, &field, script_pointer, (cursor.x, cursor.y), paused);
        gfx.latch_cursor(x, y);

        // ---- step 6: submit and present.
        gfx.submit_present()?;
        let t_done = now_ns();
        drop(hot);

        // ---- measurements (after present, off the critical path).
        metrics.set_present_mode(gfx.present_mode());
        if last_present != 0 {
            fps_meter.frame(t_done - last_present);
            metrics.frame(t_done - last_present);
        }
        last_present = t_done;
        // Each new cursor sample is counted once: from its timestamp to the
        // return of the present call of the first frame that contains it.
        let mut sent = SentFrame { tag: frame_tag, present_ns: t_done, ..SentFrame::default() };
        frame_tag += 1;
        if cursor.t_ns != 0 && cursor.t_ns != last_latched_ns {
            sent.cursor_ns = cursor.t_ns;
            last_latched_ns = cursor.t_ns;
            if cursor.source.is_evdev() {
                metrics.cursor_to_present.record(t_done.saturating_sub(cursor.t_ns));
            } else {
                metrics.window_cursor_to_present.record(t_done.saturating_sub(cursor.t_ns));
            }
        }
        for &t in &pressed_at[..pressed_len] {
            metrics.key_to_present.record(t_done.saturating_sub(t));
            if sent.nkeys < sent.keys.len() {
                sent.keys[sent.nkeys] = t;
                sent.nkeys += 1;
            }
        }
        if asked {
            metrics.screen.sent(sent);
        }
        metrics.stages(
            t_acquired - t_start,
            t_built - t_acquired,
            t_recorded - t_built,
            t_waited - t_recorded,
            t_done - t_waited,
        );
        metrics.gpu_exec(gfx.last_gpu_ns());
        metrics.cursor_source = cursor.source;

        // ---- between frames: everything that may allocate, free or wait.
        transport.collect_retired();
        // Nothing may blank the screen during a play or a replay, which
        // can go on for minutes without input.
        let want_inhibit = matches!(screen, Screen::Play(_));
        if want_inhibit != idle_inhibited {
            window.inhibit_idle(want_inhibit);
            idle_inhibited = want_inhibit;
        }
        let in_menus = matches!(screen, Screen::Menu | Screen::Select);
        if let Some(list) = &mut browser {
            list.between_frames();
            // The selection changed: its background is wanted.
            if let Some(picture) = list.picture_due() {
                wanted_picture = picture.and_then(|(dir, name)| session::find_file(&dir, &name));
                match &wanted_picture {
                    Some(path) => backgrounds.request(path.clone()),
                    None => backdrop = Backdrop::default(),
                }
            }
        }
        if let Some(decoded) = backgrounds.poll() {
            if wanted_picture.as_ref() == Some(&decoded.path) {
                backdrop = match decoded.image {
                    Some(image) => {
                        gfx.upload(PAGE_BACKGROUND, image.w as u32, image.h as u32, &image.rgba)?;
                        Backdrop { w: image.w as f32, h: image.h as f32 }
                    }
                    None => Backdrop::default(),
                };
                if let Some(list) = &mut browser {
                    list.backdrop_age = 0.0;
                }
            }
        }
        // The music of the menus: the player's buttons, the selected song,
        // the speed of the chosen mods, and what happens at the end of a song.
        if let (true, Some(list)) = (in_menus, &mut browser) {
            let now = now_ns();
            match music_action {
                Some(MenuAction::PreviousSong) => list.previous_song(now),
                Some(MenuAction::NextSong) => list.next_song(now),
                Some(MenuAction::PlayPause) => jukebox.toggle_pause(&mut transport, now),
                Some(MenuAction::Seek(to)) => jukebox.seek_to(&mut transport, to as f64, now),
                _ => {}
            }
            if !menu_song_chosen && list.has_songs() {
                menu_song_chosen = true;
                if matches!(screen, Screen::Menu) {
                    list.next_song(now);
                }
            }
            let speed = mods::speed_with(list.mods, list.rate_pct);
            let beat = list.beat();
            if let (Some(m), true) = (list.preview_due(now), transport.has_audio()) {
                let dir = m.path.parent().unwrap_or(Path::new("."));
                // Where the mapper says, or a bit before the middle.
                let start_ms = if m.preview_ms >= 0 { m.preview_ms as f64 } else { m.length_ms as f64 * 0.4 };
                if let Some(audio) = session::find_file(dir, &m.audio) {
                    let song = Song {
                        audio,
                        title: format!("{} - {}", m.artist, m.title),
                        start: start_ms / 1000.0 * SAMPLE_RATE as f64,
                        beat,
                    };
                    jukebox.play(&mut transport, song, speed, now);
                }
            }
            jukebox.set_speed(&mut transport, speed, now);
            if jukebox.ended(&transport, &clock, now) {
                if matches!(screen, Screen::Menu) {
                    // The main menu goes on to another song; the browser
                    // plays the selected one over again.
                    jukebox.forget();
                    list.next_song(now);
                } else if !jukebox.failed(&transport) {
                    jukebox.restart(&mut transport, now);
                }
            }
        }

        match next {
            None => {}
            Some(Transition::Quit) => quit = true,
            Some(Transition::OpenBrowser) => screen = Screen::Select,
            Some(Transition::ToMenu) => {
                menu.open = true;
                screen = Screen::Menu;
            }
            Some(Transition::OpenMods) => modselect = Some(ModSelect::new()),
            Some(Transition::CloseMods) => modselect = None,
            Some(Transition::OpenOptions) => {
                // Rsu's own look first, then the installed skins by name.
                let mut skins = vec![("rsu (no skin)".to_string(), None)];
                for dir in skin::installed(settings.songs.as_deref()) {
                    let name = dir.file_name().map(|n| n.to_string_lossy().trim().to_string()).unwrap_or_default();
                    skins.push((name, Some(dir)));
                }
                // A skin from elsewhere is listed too.
                if let Some(dir) = settings.skin.as_ref().filter(|d| !skins.iter().any(|s| s.1.as_ref() == Some(d))) {
                    skins.push((skin.name.clone(), Some(dir.clone())));
                }
                let at = skins.iter().position(|s| s.1 == settings.skin).unwrap_or(0);
                let songs = settings.songs.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                options = Some(Options::new(skins, at, songs, gfx.present_mode().name()));
            }
            Some(Transition::CloseOptions) => {
                options = None;
                let sort = browser
                    .as_ref()
                    .map(|b| (b.sort, b.group))
                    .filter(|&(s, g)| s != sort_at_start || g != group_at_start);
                remember(&mut config, &tun, &tun_at_start, sort);
            }
            Some(Transition::SetSongs) => {
                if let Some(panel) = &mut options {
                    let entered = PathBuf::from(&panel.songs);
                    let path = if entered.join("Songs").is_dir() { entered.join("Songs") } else { entered };
                    if panel.songs.is_empty() || !path.is_dir() {
                        notice.show(now_ns(), ui::WHITE, format_args!("Folder not found"));
                    } else {
                        panel.songs = path.display().to_string();
                        settings.songs = Some(path.clone());
                        config.songs = Some(path.clone());
                        if let Err(e) = config.save() {
                            eprintln!("rsu: could not save settings: {e}");
                        }
                        if let Some(old) = &browser {
                            let mut b = Browser::new(Some(Scan::start(path)), old.sort);
                            b.set_group(old.group);
                            browser = Some(b);
                            menu_song_chosen = false;
                        }
                        notice.show(now_ns(), ui::WHITE, format_args!("Songs folder updated"));
                    }
                }
            }
            Some(Transition::NextPresentMode) => {
                let modes = gfx.available_modes();
                if let Some(i) = modes.iter().position(|m| *m == gfx.present_mode()) {
                    let mode = modes[(i + 1) % modes.len()];
                    gfx.set_present_mode(mode);
                    metrics.reset();
                    if let Some(panel) = &mut options {
                        panel.present = mode.name();
                    } else {
                        notice.show(now_ns(), ui::WHITE, format_args!("Present mode: {}", mode.name()));
                    }
                }
            }
            Some(Transition::SetSkin(index)) => {
                let chosen = options.as_ref().and_then(|o| o.skins.get(index)).map(|s| s.1.clone());
                if let Some(chosen) = chosen {
                    settings.skin = chosen;
                    atlas.rewind(font_end);
                    ui_atlas.rewind(ui_start);
                    skin = load_skin(settings.skin.as_deref(), &mut atlas, &mut ui_atlas);
                    gfx.upload_atlas(atlas.rows(), atlas.pixels())?;
                    gfx.upload(PAGE_UI, rsu_gfx::ATLAS_SIZE, ui_atlas.rows(), ui_atlas.pixels())?;
                    config.skin = settings.skin.as_ref().map(|p| p.display().to_string());
                    if let Err(e) = config.save() {
                        eprintln!("rsu: could not save settings: {e}");
                    }
                    // The menus sound like the new skin from here on.
                    let (table, tap_ids) = session::tap_sound(&settings);
                    transport.set_samples(table);
                    tap = tap_ids;
                    let name = if skin.name.is_empty() { "rsu (no skin)" } else { skin.name.as_str() };
                    notice.show(now_ns(), ui::WHITE, format_args!("Skin: {name}"));
                }
            }
            Some(Transition::Start) => {
                let chosen = browser.as_ref().and_then(|b| b.selected_map().map(|m| (m.path.clone(), b.mods, b.rate_pct, b.auto, b.practice)));
                let Some((path, game_mods, rate_pct, auto, practice)) = chosen else {
                    continue;
                };
                match load_map(&path, &settings, transport.has_audio()) {
                    Ok(map) => {
                        // The preview's stream if it is this song: its first
                        // seconds may be decoded already.
                        let music = map.audio_path.as_ref().and_then(|a| jukebox.stream_for(&transport, a));
                        let cur = current.insert(Current { map, music, stable_source: false, mods: game_mods, rate_pct, auto, practice });
                        jukebox.forget();
                        screen = Screen::Play(begin_play(cur, None, &settings, &mut transport, now_ns()));
                    }
                    Err(e) => {
                        if let Some(list) = &mut browser {
                            list.message = e.to_string();
                        }
                    }
                }
            }
            Some(Transition::ShowScore(index)) => {
                let found =
                    browser.as_ref().and_then(|b| Some((b.score(index)?.clone(), b.selected_map()?.path.clone())));
                let Some((score, path)) = found else {
                    continue;
                };
                let loaded = load_map(&path, &settings, transport.has_audio()).and_then(|map| {
                    if score.md5 != map.md5 {
                        return Err("that score was recorded on another version of the beatmap".into());
                    }
                    let replay = scores::read_for_score(&score);
                    if !score.stable && replay.is_err() {
                        return Err(replay.err().unwrap());
                    }
                    Ok((map, replay.ok()))
                });
                match loaded {
                    Ok((map, replay)) => {
                        let music = map.audio_path.as_ref().and_then(|a| jukebox.stream_for(&transport, a));
                        let watch_available = replay.is_some();
                        let (recorded, stable_frames, end_time) = match replay {
                            Some((replay, frames)) => (replay.inputs, frames, replay.end_time),
                            None => (Vec::new(), None, 0),
                        };
                        screen = Screen::Results(Box::new(Results {
                            outcome: Outcome {
                                summary: score.summary,
                                hit_mean_ms: 0.0,
                                hit_sd_ms: 0.0,
                                hit_count: 0,
                                verified: None,
                                replay_saved: watch_available,
                                replay_path: watch_available.then(|| score.path.clone()),
                                unix_time: score.unix_time,
                            },
                            title: map.title.clone(),
                            creator: map.creator.clone(),
                            player: score.player.clone(),
                            played: ranking::date_time(score.unix_time),
                            // Shown as set, also the ones rsu does not play.
                            mods: score.mods,
                            map_speed: mods::speed_with(score.mods, score.rate_pct),
                            recorded,
                            watch_available,
                            retry_drops_mods: score.stable && unsupported_retry_mods(score.mods) != 0,
                            stable_frames,
                            end_time: end_time.max(map.chart.objects.last().map_or(0, |o| o.end.max(o.t))),
                            shown: 0.0,
                            back_hover: 0.0,
                        }));
                        current = Some(Current { map, music, stable_source: score.stable, mods: score.mods, rate_pct: score.rate_pct, auto: false, practice: PracticeDifficulty::default() });
                    }
                    Err(e) => {
                        if let Some(list) = &mut browser {
                            list.message = e.to_string();
                        }
                    }
                }
            }
            Some(Transition::Pause) => {
                if matches!(screen, Screen::Play(_)) {
                    let now = now_ns();
                    transport.set_paused(true, now);
                    transport.set_loops(&[None; 2], 0.0);
                    pause = Some(PauseMenu::new(now));
                }
            }
            Some(Transition::Fail) => {
                if matches!(screen, Screen::Play(_)) {
                    let now = now_ns();
                    transport.set_paused(true, now);
                    transport.set_loops(&[None; 2], 0.0);
                    pause = Some(PauseMenu::failed(now));
                }
            }
            Some(Transition::Resume) => {
                transport.set_paused(false, now_ns());
                pause = None;
            }
            Some(Transition::Finished) | Some(Transition::Leave) => {
                let finished = matches!(next, Some(Transition::Finished));
                transport.set_loops(&[None; 2], 0.0);
                pause = None;
                match std::mem::replace(&mut screen, Screen::Done) {
                    Screen::Play(mut play) => {
                        // An offset adjusted during the play is kept.
                        let offset_ms = play.offset as f64 / 1000.0;
                        if offset_ms != settings.offset_ms {
                            settings.offset_ms = offset_ms;
                            config.offset_ms = offset_ms;
                            (tun.offset_ms, applied.offset_ms) = (offset_ms as f32, offset_ms as f32);
                            match config.save() {
                                Ok(()) => println!("  universal offset is now {offset_ms:+.0} ms (saved)"),
                                Err(e) => eprintln!("rsu: could not save settings: {e}"),
                            }
                        }
                        let stem = current.as_ref().map_or("replay", |c| c.map.file_stem.as_str());
                        if oneshot {
                            finish_play(&play, args.save_replay.as_deref(), stem);
                            quit = true;
                        } else if play.playback.is_some() || !finished {
                            // Watching ends where it began; an abandoned
                            // play just goes back to the list.
                            if play.playback.is_some() {
                                finish_play(&play, None, stem);
                            }
                            screen = match last_results.take() {
                                Some(results) if play.playback.is_some() => Screen::Results(results),
                                _ => Screen::Select,
                            };
                        } else {
                            let outcome = finish_play(&play, None, stem);
                            let player = std::env::var("USER").unwrap_or_default();
                            if let (Some(path), Some(list)) = (&outcome.replay_path, &mut browser) {
                                list.add_score(Score {
                                    md5: play.map_md5,
                                    summary: outcome.summary,
                                    mods: play.mods,
                                    rate_pct: play.rate_pct,
                                    unix_time: outcome.unix_time,
                                    player: player.clone(),
                                    path: path.clone(),
                                    stable: false,
                                });
                            }
                            screen = Screen::Results(Box::new(Results {
                                title: play.title.clone(),
                                creator: current.as_ref().map(|c| c.map.creator.clone()).unwrap_or_default(),
                                player,
                                played: ranking::date_time(outcome.unix_time),
                                mods: play.mods,
                                map_speed: play.speed(),
                                end_time: play.game.time(),
                                recorded: std::mem::take(&mut play.recorded),
                                watch_available: !play.practice.no_50s,
                                retry_drops_mods: false,
                                stable_frames: None,
                                outcome,
                                shown: 0.0,
                                back_hover: 0.0,
                            }));
                        }
                        if let (Screen::Select, Some(list)) = (&screen, &mut browser) {
                            list.restart_preview(now_ns());
                        }
                    }
                    other => screen = other,
                }
            }
            Some(Transition::Back) => {
                last_results = None;
                screen = if browser.is_some() { Screen::Select } else { Screen::Done };
                quit |= browser.is_none();
                if let Some(list) = &mut browser {
                    list.restart_preview(now_ns());
                }
            }
            Some(Transition::Retry) => {
                if let Some(cur) = &mut current {
                    last_results = None;
                    pause = None;
                    jukebox.forget();
                    screen = Screen::Play(retry_play(cur, &settings, &mut transport, now_ns()));
                }
            }
            Some(Transition::Watch) => {
                if !matches!(&screen, Screen::Results(results) if results.can_watch()) || current.is_none() {
                    continue;
                }
                if let (Screen::Results(results), Some(cur)) = (std::mem::replace(&mut screen, Screen::Done), &current)
                {
                    let playback = Playback::new(
                        &cur.chart(),
                        cur.mods,
                        results.recorded.clone(),
                        results.end_time,
                        // A stable score has no state hash to check against.
                        Some(results.outcome.summary).filter(|s| s.state_hash != 0),
                    );
                    let playback = match &results.stable_frames {
                        Some(frames) => playback.with_stable_visual(frames),
                        None => playback,
                    };
                    last_results = Some(results);
                    jukebox.forget();
                    screen = Screen::Play(begin_play(cur, Some(playback), &settings, &mut transport, now_ns()));
                }
            }
        }
    }

    match &mut screen {
        // Closed in the middle of a play: say how far it got.
        Screen::Play(play) => {
            let stem = current.as_ref().map_or("replay", |c| c.map.file_stem.as_str());
            finish_play(play, args.save_replay.as_deref().filter(|_| oneshot), stem);
        }
        Screen::Pattern(demo) if args.calibrate => match config::trimmed_mean(&mut demo.taps) {
            Some(mean) => {
                // The taps were judged with the current offset already
                // applied, so what is left over is added to it.
                config.offset_ms = ((settings.offset_ms + mean) * 10.0).round() / 10.0;
                println!(
                    "calibration: {} taps, {:+.1} ms on average; universal offset is now {:+.1} ms",
                    demo.taps.len(),
                    mean,
                    config.offset_ms
                );
                match config.save() {
                    Ok(()) => {
                        println!("  saved; it applies to every play unless --offset-ms is given")
                    }
                    Err(e) => println!("  could not save it ({e}); use --offset-ms {:.1}", config.offset_ms),
                }
            }
            None => println!("calibration: {} taps are too few to go on (20 needed); nothing changed", demo.taps.len()),
        },
        _ => {}
    }
    // What the options and the wheel changed is kept.
    let sort = browser.as_ref().map(|b| (b.sort, b.group)).filter(|&(s, g)| s != sort_at_start || g != group_at_start);
    let changed = Tunables { show_overlay: tun_at_start.show_overlay, ..tun } != tun_at_start;
    if changed || sort.is_some() {
        remember(&mut config, &tun, &tun_at_start, sort);
    }
    metrics.print_summary(&gfx, window.backend_name(), input.as_ref(), pw.is_some(), transport.clock());
    if let Some(music) = transport.music().filter(|m| m.underruns() > 0) {
        println!("  {:<28} {} audio cycles had to wait for the decoder", "music underruns", music.underruns());
    }
    for line in absmap.describe(input.as_ref().map_or(&[], |i| i.devices.as_slice())) {
        println!("  {:<28} {line}", "cursor mapping");
    }
    Ok(())
}
