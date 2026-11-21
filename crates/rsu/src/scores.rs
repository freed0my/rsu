//! Local scores: every replay in rsu's data folder is a score, and so is
//! every osu!standard score in osu! stable's `scores.db` next to the Songs
//! folder (its replays are in `Data/r`), the way McOsu shows them. The song
//! browser lists them for the selected beatmap.

use std::path::{Path, PathBuf};
use rsu_core::game::Summary;
use rsu_formats::osr::{self, Osr};
use rsu_formats::replay::Replay;

#[derive(Clone, Debug, PartialEq)]
pub struct Score {
    pub md5: [u8; 16],
    pub summary: Summary,
    /// osu!'s mod bits; for a stable score all of them, including mods
    /// rsu does not play.
    pub mods: u32,
    /// The player's own playback rate in percent; 0 when the mods decide.
    pub rate_pct: u16,
    pub unix_time: i64,
    pub player: String,
    /// The replay file: rsu's own `.tkr`, or stable's `.osr`.
    pub path: PathBuf,
    /// Set in osu! stable and imported from its `scores.db`.
    pub stable: bool,
}

impl Score {
    pub fn of(replay: &Replay, path: PathBuf) -> Score {
        Score {
            md5: replay.map_md5,
            summary: replay.summary,
            mods: replay.mods,
            rate_pct: replay.rate_pct,
            unix_time: replay.unix_time,
            player: replay.player.clone(),
            path,
            stable: replay.ruleset == osr::STABLE_RULESET,
        }
    }
}

/// The replay behind a score plus stable's raw visual cursor frames. The
/// converted `Replay.inputs` are unchanged and alone drive judgement.
pub fn read_replay(path: &Path) -> Result<(Replay, Option<Vec<osr::Frame>>), Box<dyn std::error::Error>> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("osr")) {
        let osr = Osr::read(path)?;
        if osr.header.mode != 0 {
            return Err("that replay is not osu!standard".into());
        }
        if osr.header.mods & rsu_core::mods::SPUN_OUT != 0 {
            return Err("Spun Out .osr playback is unsupported: spinner results cannot be reproduced".into());
        }
        let replay = osr.to_replay();
        Ok((replay, Some(osr.frames)))
    } else {
        Ok((Replay::read(path)?, None))
    }
}

/// Read the replay referenced by a score, validating its map and modifiers.
pub fn read_for_score(score: &Score) -> Result<(Replay, Option<Vec<osr::Frame>>), Box<dyn std::error::Error>> {
    let (replay, frames) = read_replay(&score.path)?;
    if replay.map_md5 != score.md5 {
        return Err("that replay was recorded on another version of the beatmap".into());
    }
    if replay.mods != score.mods || replay.rate_pct != score.rate_pct {
        return Err("that replay has different mods or rate than the score".into());
    }
    if replay.player != score.player || replay.summary != score.summary ||
        (!score.stable && replay.unix_time != score.unix_time) ||
        (replay.ruleset == osr::STABLE_RULESET) != score.stable {
        return Err("that replay does not belong to the selected score".into());
    }
    Ok((replay, frames))
}

/// The osu!standard scores of stable's `scores.db` in `osu_dir`.
pub fn load_stable(osu_dir: &Path) -> Vec<Score> {
    let Some(list) = std::fs::read(osu_dir.join("scores.db")).ok().and_then(|d| osr::scores_db(&d)) else {
        return Vec::new();
    };
    let replays = osu_dir.join("Data/r");
    list.into_iter()
        .filter(|s| s.header.mode == 0)
        .map(|s| Score {
            md5: s.header.map_md5,
            summary: s.header.summary(),
            mods: s.header.mods,
            rate_pct: 0,
            unix_time: s.header.unix_time(),
            player: s.header.player.clone(),
            path: replays.join(s.header.replay_file_name()),
            stable: true,
        })
        .collect()
}

/// Where replays are kept: `$XDG_DATA_HOME/takt/replays`.
pub fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("takt/replays"))
}

/// Every score there is, best first: rsu's own and, if `songs` is an
/// osu! installation's Songs folder, stable's.
pub fn load(songs: Option<&Path>) -> Vec<Score> {
    let files = dir().and_then(|d| std::fs::read_dir(d).ok()).into_iter().flatten().flatten();
    let mut scores: Vec<Score> = files
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tkr"))
        .filter_map(|p| Replay::read(&p).ok().map(|r| Score::of(&r, p)))
        .collect();
    if let Some(osu_dir) = songs.and_then(Path::parent) {
        let stable = load_stable(osu_dir);
        if !stable.is_empty() {
            eprintln!("rsu: {} scores imported from osu! stable", stable.len());
        }
        scores.extend(stable);
    }
    sort(&mut scores);
    scores
}

pub fn sort(scores: &mut [Score]) {
    scores.sort_by(|a, b| b.summary.score.cmp(&a.summary.score).then(b.unix_time.cmp(&a.unix_time)));
}

/// "3 days ago" and the like, as McOsu words it.
pub fn ago(unix_time: i64, now: i64, out: &mut impl std::fmt::Write) {
    let delta = (now - unix_time).max(0);
    let (n, unit) = match delta {
        0..=59 => (delta, "second"),
        60..=3599 => (delta / 60, "minute"),
        3600..=86_399 => (delta / 3600, "hour"),
        86_400..=2_591_999 => (delta / 86_400, "day"),
        2_592_000..=31_535_999 => (delta / 2_592_000, "month"),
        _ => (delta / 31_536_000, "year"),
    };
    let _ = write!(out, "{n} {unit}{} ago", if n == 1 { "" } else { "s" });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_score_rejects_another_players_replay() {
        let path = std::env::temp_dir().join(format!("rsu-score-owner-{}.tkr", std::process::id()));
        let replay = Replay { ruleset: 1, mods: 0, rate_pct: 0, map_md5: [1; 16], unix_time: 10,
            player: "Alice".into(), summary: Summary::default(), end_time: 0, inputs: vec![] };
        replay.write(&path).unwrap();
        let mut score = Score::of(&replay, path.clone());
        assert!(read_for_score(&score).is_ok());
        score.player = "Bob".into();
        assert!(read_for_score(&score).is_err(), "another player's replay must not be watchable");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn converted_stable_scores_keep_classic_provenance() {
        let mut replay = Replay {
            ruleset: osr::STABLE_RULESET, mods: 0, rate_pct: 0, map_md5: [0; 16],
            unix_time: 0, player: String::new(), summary: Summary::default(),
            end_time: 0, inputs: Vec::new(),
        };
        assert!(Score::of(&replay, PathBuf::new()).stable);
        replay.ruleset = osr::STABLE_RULESET + 1;
        assert!(!Score::of(&replay, PathBuf::new()).stable);
    }

    #[test]
    #[ignore = "set RSU_TEST_TKR to a real .tkr and run with --ignored"]
    fn real_native_score_loads_its_own_replay() {
        let path = PathBuf::from(std::env::var_os("RSU_TEST_TKR").unwrap());
        let replay = Replay::read(&path).unwrap();
        let score = Score::of(&replay, path);
        assert!(read_for_score(&score).is_ok());
    }

    #[test]
    #[ignore = "set RSU_TEST_OSR to a real .osr and run with --ignored"]
    fn real_stable_osr_keeps_its_original_frames_beside_unchanged_inputs() {
        let path = std::env::var("RSU_TEST_OSR").expect("path to a real .osr");
        let path = Path::new(&path);
        let original = Osr::read(path).unwrap();
        assert!(!original.frames.is_empty());
        let (converted, frames) = read_replay(path).unwrap();
        assert_eq!(converted.inputs, original.inputs());
        assert_eq!(converted.summary, original.header.summary());
        assert_eq!(frames.as_deref(), Some(original.frames.as_slice()));
        let mut score = Score::of(&converted, path.to_path_buf());
        score.unix_time += 3600; // scores.db and .osr timestamps need not use the same representation.
        assert!(read_for_score(&score).is_ok());
        score.summary.score += 1;
        assert!(read_for_score(&score).is_err());
        let visual = crate::play::VisualTrack::from_stable_frames(&frames.unwrap());
        let at = original.frames.last().unwrap().t.saturating_mul(rsu_core::MS);
        assert!(visual.position(at).is_some());
    }

    #[test]
    fn ages_are_worded_in_the_largest_whole_unit() {
        let say = |delta: i64| {
            let mut s = String::new();
            ago(1_000_000 - delta, 1_000_000, &mut s);
            s
        };
        assert_eq!(say(1), "1 second ago");
        assert_eq!(say(59), "59 seconds ago");
        assert_eq!(say(60), "1 minute ago");
        assert_eq!(say(7200), "2 hours ago");
        assert_eq!(say(86_400 * 3 + 5), "3 days ago");
        assert_eq!(say(86_400 * 45), "1 month ago");
        assert_eq!(say(86_400 * 800), "2 years ago");
        assert_eq!(say(-5), "0 seconds ago");
    }
}
