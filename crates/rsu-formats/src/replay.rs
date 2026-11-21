//! rsu's own replay format (`.tkr`).
//!
//! A replay is nothing more than the exact input stream the core consumed,
//! plus enough context to check a playback against the original: which map,
//! which version of the rules, and the result it should arrive at.
//!
//! Layout (little endian):
//!
//! ```text
//! "TAKTRPL\x02"            magic and container version
//! u32 ruleset version      u32 mods      u16 rate in percent (0: by the mods)
//! [16] beatmap MD5         i64 unix time of the play
//! u16 + bytes player name
//! u64 score  u32 max combo  u32 n300 n100 n50 miss  u64 state hash
//! i64 end time             (map time the session was advanced to)
//! i64 start time           (map time of the first event)
//! u32 event count
//! events                   varint (dt << 2 | kind), then
//!                            kind 0 move:    zigzag varint dx, dy (1/256 px)
//!                            kind 1 press:   u8 button
//!                            kind 2 release: u8 button
//! u64 FNV-1a of everything above
//! ```
//!
//! Container version 1 had no rate field; it reads as rate 0.
//!
//! Times are deltas in microseconds and positions deltas in 1/256 osu!
//! pixel, so a five-minute play at tablet report rates is a few hundred KB
//! without any general-purpose compressor.

use std::path::Path;
use rsu_core::game::{Button, Input, InputKind, Summary};
use rsu_core::Time;

pub const MAGIC: &[u8; 8] = b"TAKTRPL\x02";
/// The container before the rate field existed.
const MAGIC_V1: &[u8; 8] = b"TAKTRPL\x01";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    pub ruleset: u32,
    pub mods: u32,
    /// A playback rate of the player's own in percent; 0 means the one the
    /// mods give (double time, half time, or 100).
    pub rate_pct: u16,
    pub map_md5: [u8; 16],
    pub unix_time: i64,
    pub player: String,
    pub summary: Summary,
    pub end_time: Time,
    pub inputs: Vec<Input>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayError {
    NotAReplay,
    Truncated,
    Corrupt(&'static str),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::NotAReplay => write!(f, "not a rsu replay"),
            ReplayError::Truncated => write!(f, "replay file is truncated"),
            ReplayError::Corrupt(what) => write!(f, "replay file is corrupt ({what})"),
        }
    }
}

impl std::error::Error for ReplayError {}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ReplayError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or(ReplayError::Truncated)?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ReplayError> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn u8(&mut self) -> Result<u8, ReplayError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ReplayError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, ReplayError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, ReplayError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64, ReplayError> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    fn varint(&mut self) -> Result<u64, ReplayError> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(ReplayError::Corrupt("varint too long"))
    }
}

impl Replay {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96 + self.inputs.len() * 8);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.ruleset.to_le_bytes());
        out.extend_from_slice(&self.mods.to_le_bytes());
        out.extend_from_slice(&self.rate_pct.to_le_bytes());
        out.extend_from_slice(&self.map_md5);
        out.extend_from_slice(&self.unix_time.to_le_bytes());
        let name = &self.player.as_bytes()[..self.player.len().min(u16::MAX as usize)];
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        let s = &self.summary;
        out.extend_from_slice(&s.score.to_le_bytes());
        for v in [s.max_combo, s.n300, s.n100, s.n50, s.miss] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&s.state_hash.to_le_bytes());
        out.extend_from_slice(&self.end_time.to_le_bytes());
        let start = self.inputs.first().map_or(0, |i| i.t);
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&(self.inputs.len() as u32).to_le_bytes());
        let (mut t, mut x, mut y) = (start, 0i32, 0i32);
        for input in &self.inputs {
            // The core never emits decreasing times; a hand-built stream
            // that does is flattened rather than encoded as garbage.
            let dt = (input.t - t).max(0) as u64;
            t += dt as i64;
            match input.kind {
                InputKind::Move { x: nx, y: ny } => {
                    put_varint(&mut out, dt << 2);
                    put_varint(&mut out, zigzag(nx as i64 - x as i64));
                    put_varint(&mut out, zigzag(ny as i64 - y as i64));
                    (x, y) = (nx, ny);
                }
                InputKind::Press(b) => {
                    put_varint(&mut out, dt << 2 | 1);
                    out.push(b as u8);
                }
                InputKind::Release(b) => {
                    put_varint(&mut out, dt << 2 | 2);
                    out.push(b as u8);
                }
            }
        }
        let sum = fnv(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    pub fn decode(data: &[u8]) -> Result<Replay, ReplayError> {
        let version = match data.get(..MAGIC.len()) {
            Some(m) if m == MAGIC => 2,
            Some(m) if m == MAGIC_V1 => 1,
            _ => return Err(ReplayError::NotAReplay),
        };
        if data.len() < MAGIC.len() + 8 {
            return Err(ReplayError::Truncated);
        }
        let (body, sum) = data.split_at(data.len() - 8);
        if fnv(body) != u64::from_le_bytes(sum.try_into().unwrap()) {
            return Err(ReplayError::Corrupt("checksum mismatch"));
        }
        let mut r = Reader { data: body, pos: MAGIC.len() };
        let ruleset = r.u32()?;
        let mods = r.u32()?;
        let rate_pct = if version >= 2 { r.u16()? } else { 0 };
        let map_md5 = r.array()?;
        let unix_time = r.i64()?;
        let name_len = r.u16()? as usize;
        let player = String::from_utf8_lossy(r.take(name_len)?).into_owned();
        let summary = Summary {
            score: r.u64()?,
            max_combo: r.u32()?,
            n300: r.u32()?,
            n100: r.u32()?,
            n50: r.u32()?,
            miss: r.u32()?,
            state_hash: r.u64()?,
        };
        let end_time = r.i64()?;
        let start = r.i64()?;
        let count = r.u32()? as usize;
        // Every event is at least two bytes; do not trust a count beyond that.
        if count > body.len() / 2 {
            return Err(ReplayError::Corrupt("event count"));
        }
        let mut inputs = Vec::with_capacity(count);
        let (mut t, mut x, mut y) = (start, 0i32, 0i32);
        for _ in 0..count {
            let head = r.varint()?;
            t = t.checked_add((head >> 2) as i64).ok_or(ReplayError::Corrupt("time overflow"))?;
            let button = |b: u8| Button::from_bits(b).ok_or(ReplayError::Corrupt("button"));
            let kind = match head & 3 {
                0 => {
                    let dx = unzigzag(r.varint()?);
                    let dy = unzigzag(r.varint()?);
                    x = (x as i64).wrapping_add(dx) as i32;
                    y = (y as i64).wrapping_add(dy) as i32;
                    InputKind::Move { x, y }
                }
                1 => InputKind::Press(button(r.u8()?)?),
                2 => InputKind::Release(button(r.u8()?)?),
                _ => return Err(ReplayError::Corrupt("event kind")),
            };
            inputs.push(Input { t, kind });
        }
        if r.pos != body.len() {
            return Err(ReplayError::Corrupt("trailing data"));
        }
        Ok(Replay { ruleset, mods, rate_pct, map_md5, unix_time, player, summary, end_time, inputs })
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.encode())
    }

    pub fn read(path: &Path) -> Result<Replay, Box<dyn std::error::Error>> {
        Ok(Replay::decode(&std::fs::read(path)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Replay {
        let mut inputs = Vec::new();
        let mut t = -1_500_000; // lead-in: map time starts negative
        for i in 0..2000i64 {
            t += 7_519;
            inputs.push(Input {
                t,
                kind: InputKind::Move { x: (i * 37 % 131_072) as i32 - 5_000, y: (i * 91 % 98_304) as i32 - 3_000 },
            });
            if i % 50 == 0 {
                let b = [Button::K1, Button::K2, Button::M1, Button::M2][(i / 50 % 4) as usize];
                inputs.push(Input { t: t + 3, kind: InputKind::Press(b) });
                inputs.push(Input { t: t + 3, kind: InputKind::Release(b) }); // zero-length dt
            }
        }
        Replay {
            ruleset: 1,
            mods: 0,
            rate_pct: 125,
            map_md5: [7; 16],
            unix_time: 1_794_000_000,
            player: "игрок".into(),
            summary: Summary {
                score: 12_345_678,
                max_combo: 999,
                n300: 900,
                n100: 50,
                n50: 3,
                miss: 1,
                state_hash: 0xdead_beef_cafe_f00d,
            },
            end_time: t + 2_000_000,
            inputs,
        }
    }

    #[test]
    fn round_trips_exactly() {
        let r = sample();
        let bytes = r.encode();
        assert_eq!(Replay::decode(&bytes).unwrap(), r);
        // Compactness is a goal: well under the 16 bytes an Input takes in memory.
        assert!(bytes.len() < r.inputs.len() * 8, "{} bytes for {} events", bytes.len(), r.inputs.len());
    }

    #[test]
    fn version_one_replays_still_read() {
        // Build a v1 file by hand: the same body without the rate field.
        let r = Replay { rate_pct: 0, ..sample() };
        let v2 = r.encode();
        let mut body = v2[..v2.len() - 8].to_vec();
        body[..8].copy_from_slice(MAGIC_V1);
        body.drain(16..18); // magic 8 + ruleset 4 + mods 4, then the rate
        let sum = fnv(&body);
        body.extend_from_slice(&sum.to_le_bytes());
        assert_eq!(Replay::decode(&body).unwrap(), r);
    }

    #[test]
    fn empty_replay_round_trips() {
        let r = Replay { inputs: Vec::new(), ..sample() };
        assert_eq!(Replay::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn damage_is_detected_not_misread() {
        let bytes = sample().encode();
        assert_eq!(Replay::decode(b"not a replay at all"), Err(ReplayError::NotAReplay));
        assert_eq!(Replay::decode(&bytes[..10]), Err(ReplayError::Truncated));
        assert!(matches!(Replay::decode(&bytes[..bytes.len() / 2]), Err(ReplayError::Corrupt(_))));
        for at in [9, 40, bytes.len() / 2, bytes.len() - 9] {
            let mut bad = bytes.clone();
            bad[at] ^= 0x10;
            assert!(matches!(Replay::decode(&bad), Err(ReplayError::Corrupt(_))), "flip at {at}");
        }
    }

    #[test]
    fn zigzag_covers_the_range() {
        for v in [0, 1, -1, 2, -2, i32::MAX as i64, i32::MIN as i64, i64::MAX, i64::MIN] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }
}
