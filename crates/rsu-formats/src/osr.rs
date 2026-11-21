//! osu! stable's scores and replays: `.osr` files and the local score list
//! `scores.db`, read the way McOsu reads them (`OsuReplay`, `OsuDatabase::
//! loadScores`).
//!
//! Both start every score with the same header (mode, version, beatmap MD5,
//! player, counts, score, mods, time). A `.osr` then carries the HP graph
//! and an LZMA stream of cursor frames `w|x|y|z` separated by commas, where
//! `w` is milliseconds since the previous frame, `x`/`y` the cursor in osu!
//! pixels and `z` the held buttons. Local replays of `scores.db` live in
//! `Data/r/{beatmap md5}-{file time}.osr`.

use crate::lzma;
use crate::osudb::Reader;
use crate::replay::Replay;
use rsu_core::game::{Button, Input, InputKind, Summary};
use rsu_core::{to_fx, Time, MS};

/// .NET ticks (100 ns since year 1) at the Unix epoch.
const TICKS_AT_UNIX_EPOCH: i64 = 621_355_968_000_000_000;
/// .NET ticks at the Windows file time epoch (1601), which names the
/// replay files in `Data/r`.
const TICKS_AT_FILE_TIME_EPOCH: i64 = 504_911_232_000_000_000;
/// A frame with this delta carries the RNG seed, not a cursor position.
const SEED_FRAME: i64 = -12345;
/// The ruleset a converted stable replay claims: none of rsu's own.
pub const STABLE_RULESET: u32 = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// 0 is osu!standard; 1 taiko, 2 catch, 3 mania.
    pub mode: u8,
    pub version: i32,
    pub map_md5: [u8; 16],
    pub player: String,
    pub replay_md5: Option<[u8; 16]>,
    pub n300: u16,
    pub n100: u16,
    pub n50: u16,
    pub geki: u16,
    pub katu: u16,
    pub miss: u16,
    pub score: i32,
    pub max_combo: u16,
    pub perfect: bool,
    /// osu!'s mod bits, all of them, including the ones rsu lacks.
    pub mods: u32,
    /// .NET ticks, UTC.
    pub ticks: i64,
}

impl Header {
    fn read(r: &mut Reader) -> Option<Header> {
        let mode = r.u8()?;
        let version = r.i32()?;
        let map_md5 = r.md5()??;
        let player = String::from_utf8_lossy(r.string()?).into_owned();
        let replay_md5 = r.md5()?;
        let mut counts = [0u16; 6];
        for c in &mut counts {
            *c = r.u16()?;
        }
        let [n300, n100, n50, geki, katu, miss] = counts;
        let score = r.i32()?;
        let max_combo = r.u16()?;
        let perfect = r.u8()? != 0;
        let mods = r.u32()?;
        Some(Header {
            mode,
            version,
            map_md5,
            player,
            replay_md5,
            n300,
            n100,
            n50,
            geki,
            katu,
            miss,
            score,
            max_combo,
            perfect,
            mods,
            ticks: 0,
        })
    }

    pub fn unix_time(&self) -> i64 {
        (self.ticks - TICKS_AT_UNIX_EPOCH).div_euclid(10_000_000)
    }

    /// The name osu! gives this score's replay inside `Data/r`.
    pub fn replay_file_name(&self) -> String {
        format!("{}-{}.osr", crate::md5::hex(&self.map_md5), self.ticks - TICKS_AT_FILE_TIME_EPOCH)
    }

    /// The result as rsu keeps it. Stable has no state hash.
    pub fn summary(&self) -> Summary {
        Summary {
            score: self.score.max(0) as u64,
            max_combo: self.max_combo as u32,
            n300: self.n300 as u32,
            n100: self.n100 as u32,
            n50: self.n50 as u32,
            miss: self.miss as u32,
            state_hash: 0,
        }
    }
}

/// One cursor frame, with its time already summed up (map milliseconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub t: i64,
    pub x: f32,
    pub y: f32,
    /// M1 1, M2 2, K1 4 (always with M1), K2 8 (always with M2), smoke 16.
    pub keys: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Osr {
    pub header: Header,
    pub frames: Vec<Frame>,
    /// The RNG seed of the last frame, in replays from 2013 on.
    pub seed: Option<i32>,
    pub online_id: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OsrError {
    Truncated,
    Lzma(lzma::LzmaError),
    Frames(&'static str),
}

impl std::fmt::Display for OsrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OsrError::Truncated => write!(f, "osu! replay is truncated or not a replay"),
            OsrError::Lzma(e) => write!(f, "osu! replay frames: {e}"),
            OsrError::Frames(what) => write!(f, "osu! replay frames are malformed ({what})"),
        }
    }
}

impl std::error::Error for OsrError {}

impl Osr {
    pub fn decode(data: &[u8]) -> Result<Osr, OsrError> {
        let mut r = Reader::new(data);
        let mut header = Header::read(&mut r).ok_or(OsrError::Truncated)?;
        r.string().ok_or(OsrError::Truncated)?; // HP graph
        header.ticks = r.i64().ok_or(OsrError::Truncated)?;
        let len = r.i32().ok_or(OsrError::Truncated)?;
        let packed = if len > 0 { r.bytes(len as usize).ok_or(OsrError::Truncated)? } else { &[][..] };
        // Older replays wrote a 32-bit online id; it does not matter enough
        // to reject a file over.
        let online_id = r.i64().or_else(|| r.i32().map(i64::from)).unwrap_or(0);
        let text = if packed.is_empty() { Vec::new() } else { lzma::decompress(packed).map_err(OsrError::Lzma)? };
        let (frames, seed) = frames(&text)?;
        Ok(Osr { header, frames, seed, online_id })
    }

    pub fn read(path: &std::path::Path) -> Result<Osr, Box<dyn std::error::Error>> {
        Ok(Osr::decode(&std::fs::read(path)?)?)
    }

    /// The input stream rsu's core understands: a move for every frame,
    /// and presses and releases where the held buttons change. K1 and K2
    /// stand for themselves; a lone M1/M2 is a mouse button.
    pub fn inputs(&self) -> Vec<Input> {
        let mut out = Vec::with_capacity(self.frames.len() + self.frames.len() / 4);
        let mut held = 0u8;
        let mut last_t = Time::MIN;
        let mut last_pos = None;
        for f in &self.frames {
            // A few frames at the start step backwards (`w` = -1); the core
            // wants time to only go forward.
            let t = (f.t * MS).max(last_t);
            last_t = t;
            let pos = (to_fx(f.x), to_fx(f.y));
            if last_pos != Some(pos) {
                out.push(Input { t, kind: InputKind::Move { x: pos.0, y: pos.1 } });
                last_pos = Some(pos);
            }
            let k = f.keys;
            let mut now = 0u8;
            now |= if k & 4 != 0 { Button::K1 as u8 } else if k & 1 != 0 { Button::M1 as u8 } else { 0 };
            now |= if k & 8 != 0 { Button::K2 as u8 } else if k & 2 != 0 { Button::M2 as u8 } else { 0 };
            for b in [Button::M1, Button::M2, Button::K1, Button::K2] {
                let bit = b as u8;
                if held & bit != 0 && now & bit == 0 {
                    out.push(Input { t, kind: InputKind::Release(b) });
                }
            }
            for b in [Button::M1, Button::M2, Button::K1, Button::K2] {
                let bit = b as u8;
                if held & bit == 0 && now & bit != 0 {
                    out.push(Input { t, kind: InputKind::Press(b) });
                }
            }
            held = now;
        }
        out
    }

    /// As a rsu replay: the inputs, the stable result as the recorded one
    /// and only the mods rsu knows (the rest change rules it lacks).
    pub fn to_replay(&self) -> Replay {
        let h = &self.header;
        let inputs = self.inputs();
        Replay {
            ruleset: STABLE_RULESET,
            mods: h.mods & rsu_mods(),
            rate_pct: 0,
            map_md5: h.map_md5,
            unix_time: h.unix_time(),
            player: h.player.clone(),
            summary: h.summary(),
            end_time: inputs.last().map_or(0, |i| i.t),
            inputs,
        }
    }
}

fn rsu_mods() -> u32 {
    rsu_core::mods::KNOWN
}

/// The mods rsu does not play, for warnings: what a stable score has that
/// a replay of it here cannot reproduce.
pub fn foreign_mods(mods: u32) -> u32 {
    // No fail, perfect and sudden death only decide when a play ends;
    // touch device changes nothing the core judges. Score v2 is not
    // harmless: it judges slider heads by accuracy.
    const HARMLESS: u32 = 1 << 0 | 1 << 2 | 1 << 5 | 1 << 14;
    mods & !rsu_mods() & !HARMLESS
}

/// osu!'s short names for the mods set in `mods`, in its usual order:
/// "HDDT", "HRNC", "" for none.
pub fn mod_names(mods: u32) -> String {
    const NAMES: &[(u32, &str)] = &[
        (1 << 0, "NF"),
        (1 << 1, "EZ"),
        (1 << 2, "TD"),
        (1 << 3, "HD"),
        (1 << 4, "HR"),
        (1 << 5, "SD"),
        (1 << 14, "PF"),
        (1 << 6, "DT"),
        (1 << 9, "NC"),
        (1 << 8, "HT"),
        (1 << 10, "FL"),
        (1 << 7, "RX"),
        (1 << 13, "AP"),
        (1 << 12, "SO"),
        (1 << 11, "AT"),
        (1 << 29, "V2"),
    ];
    let mut mods = mods;
    // As osu! shows them: NC instead of NC+DT, PF instead of PF+SD.
    if mods & (1 << 9) != 0 {
        mods &= !(1 << 6);
    }
    if mods & (1 << 14) != 0 {
        mods &= !(1 << 5);
    }
    NAMES.iter().filter(|(bit, _)| mods & bit != 0).map(|(_, n)| *n).collect()
}

fn frames(text: &[u8]) -> Result<(Vec<Frame>, Option<i32>), OsrError> {
    let text = std::str::from_utf8(text).map_err(|_| OsrError::Frames("not text"))?;
    let mut out = Vec::with_capacity(text.len() / 16);
    let mut seed = None;
    let mut t = 0i64;
    for part in text.split(',').filter(|p| !p.trim().is_empty()) {
        let mut f = part.split('|');
        let mut next = || f.next().ok_or(OsrError::Frames("field missing"));
        let w: i64 = next()?.trim().parse().map_err(|_| OsrError::Frames("time"))?;
        let x: f32 = next()?.trim().parse().map_err(|_| OsrError::Frames("x"))?;
        let y: f32 = next()?.trim().parse().map_err(|_| OsrError::Frames("y"))?;
        // Keys are written as an integer, in very old replays as a float.
        let z_text = next()?.trim();
        let z = z_text
            .parse::<i64>()
            .ok()
            .or_else(|| z_text.parse::<f64>().ok().map(|v| v as i64))
            .ok_or(OsrError::Frames("keys"))?;
        if w == SEED_FRAME {
            seed = Some(z as i32);
            continue;
        }
        t = t.checked_add(w).ok_or(OsrError::Frames("time overflow"))?;
        if !x.is_finite() || !y.is_finite() {
            return Err(OsrError::Frames("position"));
        }
        out.push(Frame { t, x, y, keys: z as u32 });
    }
    Ok((out, seed))
}

/// One score of `scores.db`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StableScore {
    pub header: Header,
    pub online_id: i64,
}

/// Every score of a `scores.db`, in file order. `None` if the data is not
/// a database this reader understands.
pub fn scores_db(data: &[u8]) -> Option<Vec<StableScore>> {
    let mut r = Reader::new(data);
    let _version = r.i32()?;
    let maps = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..maps {
        let _map = r.md5()?;
        let n = r.u32()?;
        for _ in 0..n {
            let mut header = Header::read(&mut r)?;
            r.string()?; // the HP graph, always empty here
            header.ticks = r.i64()?;
            let _replay_len = r.i32()?; // always -1: the replay is elsewhere
            let online_id = if header.version >= 20121008 { r.i64()? } else { r.i32()? as i64 };
            if header.mods & (1 << 23) != 0 {
                r.f64()?; // target practice accuracy
            }
            out.push(StableScore { header, online_id });
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(out: &mut Vec<u8>, s: &str) {
        if s.is_empty() {
            out.push(0);
        } else {
            out.push(0x0b);
            out.push(s.len() as u8);
            out.extend_from_slice(s.as_bytes());
        }
    }

    fn header_bytes(out: &mut Vec<u8>) {
        out.push(0);
        out.extend_from_slice(&20240101i32.to_le_bytes());
        string(out, "0123456789abcdef0123456789abcdef");
        string(out, "player");
        string(out, "ffffffffffffffffffffffffffffffff");
        for c in [300u16, 20, 3, 40, 5, 1] {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&1_234_567i32.to_le_bytes());
        out.extend_from_slice(&456u16.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&(64u32 | 8).to_le_bytes()); // HDDT
    }

    /// The text "0|256|-500|0,-1|256|-500|0,20|100.5|50|5,10|101|50|15,
    /// 10|102|51|0,-12345|0|0|777," packed by `xz --format=lzma`.
    const FRAMES: &[u8] = &[
        0x5d, 0x00, 0x00, 0x00, 0x04, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x18, 0x1f, 0x02, 0x43,
        0x51, 0x03, 0xb4, 0x00, 0x55, 0x57, 0xd8, 0x53, 0xab, 0x04, 0x8d, 0x68, 0x02, 0x8a, 0x95, 0x7f, 0xad, 0xd2,
        0xc8, 0x97, 0xd0, 0x64, 0x6d, 0x1c, 0x3d, 0x62, 0xfa, 0x67, 0x05, 0xcd, 0xc3, 0x72, 0xb8, 0x6c, 0x87, 0xf8,
        0x85, 0xb9, 0x84, 0x11, 0x38, 0x02, 0xb1, 0x5b, 0x24, 0xfa, 0xc3, 0xe1, 0xc8, 0x15, 0xf9, 0x6e, 0x7f, 0xfe,
        0xec, 0x62, 0x00,
    ];

    fn osr() -> Vec<u8> {
        let mut out = Vec::new();
        header_bytes(&mut out);
        string(&mut out, "");
        let ticks = TICKS_AT_UNIX_EPOCH + 1_700_000_000 * 10_000_000;
        out.extend_from_slice(&ticks.to_le_bytes());
        out.extend_from_slice(&(FRAMES.len() as i32).to_le_bytes());
        out.extend_from_slice(FRAMES);
        out.extend_from_slice(&99i64.to_le_bytes());
        out
    }

    #[test]
    fn reads_header_frames_and_seed() {
        let r = Osr::decode(&osr()).unwrap();
        let h = &r.header;
        assert_eq!((h.player.as_str(), h.score, h.max_combo, h.mods), ("player", 1_234_567, 456, 72));
        assert_eq!((h.n300, h.n100, h.n50, h.miss), (300, 20, 3, 1));
        assert_eq!(h.map_md5[..3], [0x01, 0x23, 0x45]);
        assert_eq!(h.unix_time(), 1_700_000_000);
        assert_eq!((r.seed, r.online_id), (Some(777), 99));
        let times: Vec<i64> = r.frames.iter().map(|f| f.t).collect();
        assert_eq!(times, [0, -1, 19, 29, 39]);
        assert_eq!(r.frames[2].x, 100.5);
        assert_eq!(mod_names(h.mods), "HDDT");
        assert_eq!(foreign_mods(h.mods), 0);
        assert_eq!(foreign_mods(1024 | 16 | 8 | 64), 1024);
    }

    #[test]
    fn frames_become_moves_presses_and_releases() {
        let r = Osr::decode(&osr()).unwrap();
        let inputs = r.inputs();
        use InputKind::*;
        let kinds: Vec<(Time, InputKind)> = inputs.iter().map(|i| (i.t, i.kind)).collect();
        assert_eq!(
            kinds,
            [
                (0, Move { x: 256 * 256, y: -500 * 256 }),
                (19_000, Move { x: to_fx(100.5), y: 50 * 256 }),
                (19_000, Press(Button::K1)),
                (29_000, Move { x: 101 * 256, y: 50 * 256 }),
                // 5 -> 15: K1 still held, the smoke bit is not a button, K2 pressed
                (29_000, Press(Button::K2)),
                (39_000, Move { x: 102 * 256, y: 51 * 256 }),
                (39_000, Release(Button::K1)),
                (39_000, Release(Button::K2)),
            ]
        );
        // The frame stepping back to -1 ms is held at 0.
        assert!(inputs.windows(2).all(|w| w[0].t <= w[1].t));
        let replay = r.to_replay();
        assert_eq!(replay.mods, 64 | 8, "double time and hidden");
        assert_eq!(replay.summary.score, 1_234_567);
        assert_eq!(replay.ruleset, STABLE_RULESET);
    }

    #[test]
    fn reads_a_scores_db() {
        let mut db = Vec::new();
        db.extend_from_slice(&20240101i32.to_le_bytes());
        db.extend_from_slice(&1u32.to_le_bytes());
        string(&mut db, "0123456789abcdef0123456789abcdef");
        db.extend_from_slice(&2u32.to_le_bytes());
        for ticks in [637_000_000_000_000_000i64, 638_000_000_000_000_000] {
            header_bytes(&mut db);
            string(&mut db, "");
            db.extend_from_slice(&ticks.to_le_bytes());
            db.extend_from_slice(&(-1i32).to_le_bytes());
            db.extend_from_slice(&5i64.to_le_bytes());
        }
        let scores = scores_db(&db).unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[1].header.ticks, 638_000_000_000_000_000);
        assert_eq!(scores[0].online_id, 5);
        assert_eq!(
            scores[0].header.replay_file_name(),
            format!("0123456789abcdef0123456789abcdef-{}.osr", 637_000_000_000_000_000i64 - TICKS_AT_FILE_TIME_EPOCH)
        );
        assert!(scores_db(&db[..db.len() - 3]).is_none());
    }

    #[test]
    fn damage_is_an_error_not_a_panic() {
        let bytes = osr();
        for cut in 0..bytes.len() - 8 {
            assert!(Osr::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }
}
