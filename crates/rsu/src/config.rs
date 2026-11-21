//! The settings file: `~/.config/takt/config`, plain `key = value` lines.
//! Everything in it can also be given on the command line; the file is for
//! what should be remembered between runs.

use crate::binds::{self, Binds};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The osu! Songs folder beatmaps are looked up in.
    pub songs: Option<PathBuf>,
    /// A skin folder, or the name of one next to the Songs folder.
    pub skin: Option<String>,
    pub songselect_top: bool,
    pub songselect_bottom: bool,
    /// Universal offset: positive judges inputs as earlier.
    pub offset_ms: f64,
    pub music_volume: f32,
    pub effect_volume: f32,
    /// Applies to both of the above.
    pub master_volume: f32,
    /// Play the samples and follow the per-object sound settings a beatmap
    /// brings, as osu! does. Off: only the skin's sounds, chosen the way
    /// McOsu chooses them.
    pub beatmap_hitsounds: bool,
    /// How much of a beatmap's background is taken away during play, 0..1.
    pub background_dim: f32,
    /// Cursor size as a multiple of the skin's image (McOsu `osu_cursor_scale`).
    pub cursor_scale: f32,
    /// Force a smooth cursor trail during replay playback.
    pub replay_smooth_trail: bool,
    /// The order of the song browser, by `browser::Sort::name`.
    pub sort: Option<String>,
    /// How the song browser divides the list, by `browser::Group::name`.
    pub group: Option<String>,
    /// Frame limit for the modes that do not wait for the display; 0 is
    /// none. Unset: what the command line says (1000 by default).
    pub fps_cap: Option<u32>,
    /// Keys and buttons, in the order of `binds::ACTIONS`.
    pub binds: Binds,
}

impl Default for Config {
    fn default() -> Self {
        // Volumes as McOsu has them out of the box.
        Config {
            songs: None,
            skin: None,
            songselect_top: false,
            songselect_bottom: false,
            offset_ms: 0.0,
            music_volume: 0.4,
            effect_volume: 1.0,
            master_volume: 1.0,
            beatmap_hitsounds: false,
            background_dim: 0.75,
            cursor_scale: 1.5,
            replay_smooth_trail: false,
            sort: None,
            group: None,
            fps_cap: None,
            binds: binds::defaults(),
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join("takt/config"))
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        let mut current = false;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key.starts_with('#') || value.is_empty() {
                continue;
            }
            match key {
                "songs" => c.songs = Some(value.into()),
                "skin" => c.skin = Some(value.into()),
                "songselect_top" => c.songselect_top = value == "1" || value == "true",
                "songselect_bottom" => c.songselect_bottom = value == "1" || value == "true",
                "offset_ms" => c.offset_ms = value.parse().unwrap_or(0.0),
                "music_volume" => c.music_volume = value.parse().unwrap_or(c.music_volume),
                "effect_volume" => c.effect_volume = value.parse().unwrap_or(c.effect_volume),
                "master_volume" => c.master_volume = value.parse().unwrap_or(c.master_volume).clamp(0.0, 1.0),
                "beatmap_hitsounds" => c.beatmap_hitsounds = value == "1" || value == "true",
                "background_dim" => c.background_dim = value.parse().unwrap_or(c.background_dim).clamp(0.0, 1.0),
                "cursor_scale" => c.cursor_scale = value.parse().unwrap_or(c.cursor_scale).clamp(0.1, 3.0),
                "replay_smooth_trail" => c.replay_smooth_trail = value == "1" || value == "true",
                "sort" => c.sort = Some(value.into()),
                "group" => c.group = Some(value.into()),
                "fps_cap" => c.fps_cap = value.parse().ok(),
                "format" => current = value == "2",
                other => {
                    if let Some(i) = binds::ACTIONS.iter().position(|a| a.key == other) {
                        c.binds[i] = value.parse().unwrap_or(c.binds[i]);
                    }
                }
            }
        }
        // Earlier versions wrote their defaults (0.5 and 0.5) into every
        // settings file without anyone having chosen them; in a file of
        // that time the pair is read as "not set".
        if !current && (c.music_volume, c.effect_volume) == (0.5, 0.5) {
            (c.music_volume, c.effect_volume) = (Config::default().music_volume, Config::default().effect_volume);
        }
        c
    }

    pub fn render(&self) -> String {
        let mut out = String::from("# rsu settings. Lines are `key = value`.\nformat = 2\n");
        if let Some(songs) = &self.songs {
            out += &format!("songs = {}\n", songs.display());
        }
        if let Some(skin) = &self.skin {
            out += &format!("skin = {skin}\n");
        }
        out += &format!("songselect_top = {}\n", self.songselect_top as u8);
        out += &format!("songselect_bottom = {}\n", self.songselect_bottom as u8);
        out += &format!("offset_ms = {:.1}\n", self.offset_ms);
        out += &format!("music_volume = {}\n", self.music_volume);
        out += &format!("effect_volume = {}\n", self.effect_volume);
        out += &format!("master_volume = {}\n", self.master_volume);
        out += &format!("beatmap_hitsounds = {}\n", self.beatmap_hitsounds as u8);
        out += &format!("background_dim = {}\n", self.background_dim);
        out += &format!("cursor_scale = {}\n", self.cursor_scale);
        out += &format!("replay_smooth_trail = {}\n", self.replay_smooth_trail as u8);
        if let Some(sort) = &self.sort {
            out += &format!("sort = {sort}\n");
        }
        if let Some(group) = &self.group {
            out += &format!("group = {group}\n");
        }
        if let Some(cap) = self.fps_cap {
            out += &format!("fps_cap = {cap}\n");
        }
        for (action, code) in binds::ACTIONS.iter().zip(self.binds) {
            out += &format!("{} = {code}\n", action.key);
        }
        out
    }

    pub fn load() -> Config {
        Config::path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Config::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Config::path().ok_or_else(|| std::io::Error::other("no home directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.render())
    }
}

/// Mean of the middle 80% of `values`: a few stray taps (a slip, a double
/// press) must not move the result. `None` if there is too little to go on.
pub fn trimmed_mean(values: &mut [f64]) -> Option<f64> {
    if values.len() < 20 {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let cut = values.len() / 10;
    let kept = &values[cut..values.len() - cut];
    Some(kept.iter().sum::<f64>() / kept.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_smooth_trail_defaults_off_and_round_trips() {
        assert!(!Config::default().replay_smooth_trail);
        assert!(!Config::parse("format = 2\n").replay_smooth_trail);
        for enabled in [false, true] {
            let config = Config { replay_smooth_trail: enabled, ..Config::default() };
            assert_eq!(Config::parse(&config.render()).replay_smooth_trail, enabled);
        }
        assert!(Config::parse("replay_smooth_trail = true\n").replay_smooth_trail);
    }

    #[test]
    fn songselect_panel_preferences_default_off_and_round_trip_independently() {
        for defaults in [Config::default(), Config::parse("format = 2\n")] {
            assert!(!defaults.songselect_top);
            assert!(!defaults.songselect_bottom);
        }
        for (top, bottom) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            let text = format!("songselect_top = {top}\nsongselect_bottom = {bottom}\n");
            let rendered = Config::parse(&text).render();
            assert!(rendered.contains(&format!("songselect_top = {top}\n")));
            assert!(rendered.contains(&format!("songselect_bottom = {bottom}\n")));
        }
    }

    #[test]
    fn config_round_trips_and_ignores_junk() {
        let c = Config {
            beatmap_hitsounds: true,
            songs: Some("/home/someone/osu/Songs".into()),
            skin: Some("My Skin - v2".into()),
            songselect_top: false,
            songselect_bottom: true,
            offset_ms: -7.5,
            music_volume: 0.25,
            effect_volume: 0.75,
            master_volume: 0.6,
            background_dim: 0.9,
            cursor_scale: 2.25,
            replay_smooth_trail: true,
            sort: Some("title".into()),
            group: Some("artist".into()),
            fps_cap: Some(0),
            binds: [30, 31, 32, 0, 33, 1, 14, 78, 74],
        };
        assert_eq!(Config::parse(&c.render()), c);
        // Volumes someone chose stay as chosen, whatever they are; the same
        // numbers in a file from before they could be chosen do not.
        let halves = Config { music_volume: 0.5, effect_volume: 0.5, ..Config::default() };
        assert_eq!(Config::parse(&halves.render()), halves);
        assert_eq!(Config::parse("music_volume = 0.5\neffect_volume = 0.5\n"), Config::default());
        let c = Config::parse("garbage\n# comment = 1\noffset_ms = 12\nunknown = x\nskin =\n");
        assert_eq!(c, Config { offset_ms: 12.0, ..Config::default() });
    }

    /// Deterministic noise, roughly normal (sum of uniforms), in -1..1.
    fn noise(seed: &mut u64) -> f64 {
        (0..6)
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (*seed >> 33) as f64 / (1u64 << 31) as f64
            })
            .sum::<f64>()
            / 3.0
            - 1.0
    }

    #[test]
    fn calibration_recovers_a_bias_despite_stray_taps() {
        // A player 12 ms late with 15 ms of scatter; one tap in twenty is a
        // slip 150 ms off. What matters is how close repeated calibrations
        // land to the truth.
        let mut worst = 0.0f64;
        for run in 0..50u64 {
            let mut seed = run * 977 + 5;
            let mut taps: Vec<f64> = (0..60)
                .map(|i| {
                    let slip = if i % 20 == 7 { 150.0 * noise(&mut seed).signum() } else { 0.0 };
                    12.0 + 15.0 * noise(&mut seed) * 1.7 + slip
                })
                .collect();
            let plain = taps.iter().sum::<f64>() / taps.len() as f64;
            let robust = trimmed_mean(&mut taps).unwrap();
            worst = worst.max((robust - 12.0).abs());
            // The slips alone can pull a plain mean by 7.5 ms.
            assert!((robust - 12.0).abs() <= (plain - 12.0).abs() + 1.5, "run {run}: {robust} vs {plain}");
        }
        // 60 taps with ~15 ms of scatter cannot do better than about
        // 15 / sqrt(60) = 2 ms; three times that is the bound here.
        assert!(worst < 6.0, "worst calibration error {worst} ms");
        assert_eq!(trimmed_mean(&mut [1.0; 19]), None);
    }
}
