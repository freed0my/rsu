//! The little that rsu wants from osu!'s own beatmap database (`osu!.db`):
//! the timing offsets attached to individual beatmaps and their star
//! ratings. The local offset is what the player set for a map in osu!; the
//! online offset is a correction published for it. Both are in milliseconds
//! and both delay the map.
//!
//! The layout follows McOsu's reader (`OsuDatabase::loadDB`).

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offsets {
    pub local_ms: i16,
    pub online_ms: i16,
}

/// What the database says about one beatmap.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Entry {
    pub offsets: Offsets,
    /// Star rating without mods, with double time and with half time;
    /// zero where the database has none.
    /// Star ratings by [`star_slot`]: no speed mod, double time, half
    /// time, each without, with hard rock and with easy.
    pub stars: [f32; 9],
}

/// Reads the little-endian .NET `BinaryWriter` layout every osu! file
/// shares; shared with the score and replay readers.
pub(crate) struct Reader<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }
    pub(crate) fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    pub(crate) fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    /// A string holding an MD5 in hex, as osu! keeps them.
    pub(crate) fn md5(&mut self) -> Option<Option<[u8; 16]>> {
        Some(md5_text(std::str::from_utf8(self.string()?).ok()?))
    }
    pub(crate) fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.data.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(out)
    }
    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        self.bytes(n).map(|_| ())
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    pub(crate) fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    pub(crate) fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    pub(crate) fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
    /// A string: nothing, or a marker, a length in LEB128 and the bytes.
    pub(crate) fn string(&mut self) -> Option<&'a [u8]> {
        match self.u8()? {
            0 => Some(&[]),
            0x0b => {
                let (mut len, mut shift) = (0usize, 0);
                loop {
                    let b = self.u8()?;
                    len |= ((b & 0x7f) as usize) << shift;
                    shift += 7;
                    if b & 0x80 == 0 || shift > 35 {
                        break;
                    }
                }
                self.bytes(len)
            }
            _ => None,
        }
    }
}

/// 32 hex digits to 16 bytes; `None` for anything else.
/// Where the star rating for these mods is in [`Entry::stars`]. osu!
/// rates every combination of a speed mod (none, double time, half time)
/// with none, hard rock or easy; nightcore counts as double time.
pub fn star_slot(mods: u32) -> usize {
    let speed = if mods & (64 | 512) != 0 {
        1
    } else if mods & 256 != 0 {
        2
    } else {
        0
    };
    let difficulty = if mods & 16 != 0 {
        1
    } else if mods & 2 != 0 {
        2
    } else {
        0
    };
    speed * 3 + difficulty
}

/// The star rating at any playback speed, from the nine osu! stores:
/// straight lines through the ratings at 0.75x, 1x and 1.5x, carried on
/// past them. Exact at those three speeds and only an estimate between and
/// beyond (rsu has no star calculator of its own).
pub fn stars_at(stars: &[f32; 9], mods: u32, speed: f64) -> f32 {
    let column = star_slot(mods) % 3;
    let (nm, dt, ht) = (stars[column], stars[3 + column], stars[6 + column]);
    let (x0, y0, x1, y1) = if speed < 1.0 {
        (0.75, ht, 1.0, nm)
    } else {
        (1.0, nm, 1.5, dt)
    };
    if ht <= 0.0 || nm <= 0.0 || dt <= 0.0 {
        return 0.0;
    }
    let slope = (y1 - y0) as f64 / (x1 - x0);
    (y0 as f64 + (speed - x0) * slope).max(0.0) as f32
}

pub fn md5_text(text: &str) -> Option<[u8; 16]> {
    let mut md5 = [0u8; 16];
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    for (o, at) in md5.iter_mut().zip((0..32).step_by(2)) {
        *o = u8::from_str_radix(&text[at..at + 2], 16).ok()?;
    }
    Some(md5)
}

/// The player's collections from `collection.db`: a name and the MD5 of
/// every beatmap in it. `None` if the data is not understood.
pub fn collections(data: &[u8]) -> Option<Vec<(String, Vec<[u8; 16]>)>> {
    let mut r = Reader::new(data);
    r.skip(4)?; // version
    let count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let name = String::from_utf8_lossy(r.string()?).into_owned();
        let n = r.u32()?;
        let mut maps = Vec::with_capacity(n.min(1 << 16) as usize);
        for _ in 0..n {
            if let Some(md5) = r.md5()? {
                maps.push(md5);
            }
        }
        out.push((name, maps));
    }
    Some(out)
}

/// Offsets by beatmap MD5, for the maps that have any. `None` if the data
/// is not a database this reader understands.
pub fn offsets(data: &[u8]) -> Option<HashMap<[u8; 16], Offsets>> {
    let all = read(data)?;
    Some(all.into_iter().filter(|(_, e)| e.offsets != Offsets::default()).map(|(k, e)| (k, e.offsets)).collect())
}

/// Every beatmap of the database by its MD5. `None` if the data is not a
/// database this reader understands.
pub fn read(data: &[u8]) -> Option<HashMap<[u8; 16], Entry>> {
    let mut r = Reader::new(data);
    let version = r.u32()?;
    // Older layouts differ in more than this reader cares to know.
    if version < 20140609 {
        return None;
    }
    r.skip(4 + 1 + 8)?; // folder count, account unlocked, unlock date
    r.string()?; // player name
    let count = r.u32()?;
    let mut out = HashMap::with_capacity(count.min(1 << 20) as usize);
    for _ in 0..count {
        if version < 20191107 {
            r.skip(4)?; // entry size
        }
        for _ in 0..7 {
            r.string()?; // artist, title (each twice), creator, difficulty, audio file
        }
        let md5 = r.string()?;
        r.string()?; // .osu file name
        r.skip(1 + 2 * 3 + 8 + 4 * 4 + 8)?; // status, counts, modified, AR CS HP OD, slider velocity
        let mut stars = [0.0f32; 9];
        for mode in 0..4 {
            // Star ratings per mod combination, for each of the four modes:
            // a marker, the mods, a marker, the rating.
            let pairs = r.u32()? as usize;
            let floats = version >= 20250108;
            if mode != 0 {
                r.skip(pairs.checked_mul(if floats { 10 } else { 14 })?)?;
                continue;
            }
            for _ in 0..pairs {
                r.skip(1)?;
                let mods = r.u32()?;
                r.skip(1)?;
                let rating = if floats { r.f32()? } else { r.f64()? as f32 };
                if mods & !(64 | 256 | 16 | 2) != 0 {
                    continue; // flashlight and the like
                }
                let slot = star_slot(mods);
                if rating.is_finite() && rating > 0.0 {
                    stars[slot] = rating;
                }
            }
        }
        r.skip(4 * 3)?; // drain time, length, preview time
        let timing_points = r.u32()? as usize;
        r.skip(timing_points.checked_mul(17)?)?;
        r.skip(4 * 3 + 4)?; // ids, grades
        let local_ms = r.i16()?;
        r.skip(4 + 1)?; // stack leniency, mode
        r.string()?; // source
        r.string()?; // tags
        let online_ms = r.i16()?;
        r.string()?; // title font
        r.skip(1 + 8 + 1)?; // unplayed, last played, osz2
        r.string()?; // folder
        r.skip(8 + 5 + 4 + 1)?; // last checked, five switches, last edit, mania speed
        if let Some(key) = parse_md5(md5) {
            out.insert(key, Entry { offsets: Offsets { local_ms, online_ms }, stars });
        }
    }
    Some(out)
}

fn parse_md5(hex: &[u8]) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    if hex.len() != 32 {
        return None;
    }
    for (o, pair) in out.iter_mut().zip(hex.chunks_exact(2)) {
        *o = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
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
            out.extend([0x0b, s.len() as u8]);
            out.extend(s.as_bytes());
        }
    }

    fn entry(out: &mut Vec<u8>, version: u32, md5: &str, local: i16, online: i16) {
        for s in ["Artist", "", "Title", "", "Mapper", "Hard", "audio.mp3", md5, "map.osu"] {
            string(out, s);
        }
        out.extend([0u8; 1 + 6 + 8 + 16 + 8]);
        // One star rating (5.25 without mods) for the first mode, none for
        // the others.
        out.extend(1u32.to_le_bytes());
        out.push(0x08);
        out.extend(0u32.to_le_bytes());
        if version >= 20250108 {
            out.push(0x0c);
            out.extend(5.25f32.to_le_bytes());
        } else {
            out.push(0x0d);
            out.extend(5.25f64.to_le_bytes());
        }
        out.extend([0u8; 12]);
        out.extend([0u8; 12]);
        out.extend(2u32.to_le_bytes()); // two timing points
        out.extend([0u8; 34]);
        out.extend([0u8; 16]);
        out.extend(local.to_le_bytes());
        out.extend([0u8; 5]);
        string(out, "source");
        string(out, "");
        out.extend(online.to_le_bytes());
        string(out, "");
        out.extend([0u8; 10]);
        string(out, "123 Artist - Title");
        out.extend([0u8; 18]);
    }

    fn database(version: u32) -> Vec<u8> {
        let mut db = Vec::new();
        db.extend(version.to_le_bytes());
        db.extend([0u8; 13]);
        string(&mut db, "player");
        db.extend(3u32.to_le_bytes());
        entry(&mut db, version, "00112233445566778899aabbccddeeff", 20, 0);
        entry(&mut db, version, "ffffffffffffffffffffffffffffffff", 0, 0);
        entry(&mut db, version, "0123456789abcdef0123456789abcdef", -5, 12);
        db
    }

    #[test]
    fn stars_follow_the_speed() {
        // none, DT, HT without HR/EZ columns filled.
        let s = [5.0, 0.0, 0.0, 7.5, 0.0, 0.0, 4.0, 0.0, 0.0];
        assert_eq!(stars_at(&s, 0, 1.0), 5.0);
        assert_eq!(stars_at(&s, 0, 1.5), 7.5);
        assert_eq!(stars_at(&s, 0, 0.75), 4.0);
        assert!((stars_at(&s, 0, 1.25) - 6.25).abs() < 1e-5);
        assert!((stars_at(&s, 0, 0.875) - 4.5).abs() < 1e-5);
        assert_eq!(stars_at(&s, 16, 1.0), 0.0, "a column the database lacks stays unknown");
    }

    #[test]
    fn offsets_are_found_in_both_star_rating_layouts() {
        for version in [20240101, 20260924] {
            let found = offsets(&database(version)).unwrap();
            assert_eq!(found.len(), 2, "maps without an offset are left out");
            let first =
                [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
            assert_eq!(found[&first], Offsets { local_ms: 20, online_ms: 0 });
            let third =
                [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
            assert_eq!(found[&third], Offsets { local_ms: -5, online_ms: 12 });
            let all = read(&database(version)).unwrap();
            assert_eq!(all.len(), 3);
            assert_eq!(all[&[0xff; 16]], Entry { offsets: Offsets::default(), stars: [5.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0] });
        }
    }

    #[test]
    fn damaged_or_ancient_data_is_refused_not_misread() {
        let db = database(20260924);
        assert!(offsets(&db[..db.len() - 7]).is_none());
        assert!(offsets(&database(20130101)).is_none());
        assert!(offsets(&[]).is_none());
    }
}
