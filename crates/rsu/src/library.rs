//! The song library: every osu!-standard beatmap in the Songs folder, with
//! what the selection screen shows about it.
//!
//! Reading and parsing thousands of files takes seconds, so the result is
//! cached in `~/.cache/takt/` and a file is parsed again only when its size
//! or modification time changes. Scanning runs on its own thread.

use crate::scores::{self, Score};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use rsu_formats::md5::{hex, md5};
use rsu_formats::{osu, osudb};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapInfo {
    pub path: PathBuf,
    pub artist: String,
    pub title: String,
    pub version: String,
    pub creator: String,
    pub cs: f32,
    pub ar: f32,
    pub od: f32,
    pub hp: f32,
    pub objects: u32,
    pub circles: u32,
    pub sliders: u32,
    pub spinners: u32,
    /// Time of the last object.
    pub length_ms: u32,
    /// Where the preview starts; negative if the map does not say.
    pub preview_ms: i32,
    pub audio: String,
    /// File name of the background image; may be empty.
    pub background: String,
    /// Slowest and fastest tempo, and the one most of the map is in.
    pub bpm: [f32; 3],
    /// Time of the first beat and length of a beat, in milliseconds, by
    /// the map's first timing point; (0, 0) if it has none.
    pub beat: (f64, f64),
    pub md5: [u8; 16],
    /// When the file was last written, in seconds since 1970: the closest
    /// thing to "date added" a folder of files has.
    pub added: u64,
    /// Star rating without mods, with double time and with half time, from
    /// osu!'s database; zero when it has none. Not part of the cache.
    pub stars: [f32; 9],
    /// Which beatmap set (folder) this belongs to; the maps of a set are
    /// neighbours in the library, easiest first.
    pub set: u32,
    /// Lower-cased text the search runs over.
    pub haystack: String,
}

/// What the cache remembers about one file. `info` is `None` for files
/// that are not playable (other game modes, broken files), so they are not
/// parsed again either.
struct Entry {
    mtime: u64,
    size: u64,
    info: Option<MapInfo>,
}

const HEADER: &str = "takt library v2";

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn encode(path: &Path, e: &Entry) -> String {
    let head = format!("{}\t{}\t{}", clean(&path.to_string_lossy()), e.mtime, e.size);
    match &e.info {
        None => format!("{head}\t-"),
        Some(i) => format!(
            "{head}\t+\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean(&i.artist),
            clean(&i.title),
            clean(&i.version),
            clean(&i.creator),
            i.cs,
            i.ar,
            i.od,
            i.hp,
            i.circles,
            i.sliders,
            i.spinners,
            i.length_ms,
            i.preview_ms,
            clean(&i.audio),
            clean(&i.background),
            i.bpm[0],
            i.bpm[1],
            i.bpm[2],
            hex(&i.md5),
            i.beat.0,
            i.beat.1,
            // Source and tags are only ever searched, so the cache keeps
            // the text the search runs over instead of the fields.
            clean(&i.haystack),
        ),
    }
}

fn unhex(text: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    for (o, at) in out.iter_mut().zip((0..32).step_by(2)) {
        *o = u8::from_str_radix(&text[at..at + 2], 16).ok()?;
    }
    Some(out)
}

fn decode(line: &str) -> Option<(PathBuf, Entry)> {
    let f: Vec<&str> = line.split('\t').collect();
    let path = PathBuf::from(f.first()?);
    let (mtime, size) = (f.get(1)?.parse().ok()?, f.get(2)?.parse().ok()?);
    let info = match *f.get(3)? {
        "-" => None,
        _ => {
            let (circles, sliders, spinners): (u32, u32, u32) =
                (f.get(12)?.parse().ok()?, f.get(13)?.parse().ok()?, f.get(14)?.parse().ok()?);
            Some(MapInfo {
                path: path.clone(),
                artist: f.get(4)?.to_string(),
                title: f.get(5)?.to_string(),
                version: f.get(6)?.to_string(),
                creator: f.get(7)?.to_string(),
                cs: f.get(8)?.parse().ok()?,
                ar: f.get(9)?.parse().ok()?,
                od: f.get(10)?.parse().ok()?,
                hp: f.get(11)?.parse().ok()?,
                objects: circles + sliders + spinners,
                circles,
                sliders,
                spinners,
                length_ms: f.get(15)?.parse().ok()?,
                preview_ms: f.get(16)?.parse().ok()?,
                audio: f.get(17)?.to_string(),
                background: f.get(18)?.to_string(),
                bpm: [f.get(19)?.parse().ok()?, f.get(20)?.parse().ok()?, f.get(21)?.parse().ok()?],
                md5: unhex(f.get(22)?)?,
                added: mtime,
                stars: [0.0; 9],
                set: 0,
                beat: (f.get(23)?.parse().ok()?, f.get(24)?.parse().ok()?),
                haystack: f.get(25)?.to_string(),
            })
        }
    };
    Some((path, Entry { mtime, size, info }))
}

fn read_cache(path: &Path) -> HashMap<PathBuf, Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return HashMap::new();
    }
    lines.filter_map(decode).collect()
}

/// Slowest, fastest and most used tempo of a map, the way McOsu works
/// them out: every timing section counts for as long as it lasts.
fn tempo(map: &osu::Beatmap, last_ms: f64) -> [f32; 3] {
    let points: Vec<(f64, f64)> = map
        .timing_points
        .iter()
        .filter(|p| p.uninherited && p.beat_length.is_finite() && p.beat_length > 0.0)
        .map(|p| (p.time_ms, 60_000.0 / p.beat_length))
        .collect();
    let (mut lo, mut hi) = (f64::MAX, 0.0f64);
    let mut spans: Vec<(f64, f64)> = Vec::new();
    for (n, &(t, bpm)) in points.iter().enumerate() {
        (lo, hi) = (lo.min(bpm), hi.max(bpm));
        // The first section also covers what comes before it.
        let from = if n == 0 { f64::MIN } else { t };
        let until = points.get(n + 1).map_or(last_ms.max(t), |next| next.0);
        let lasted = (until.min(last_ms.max(t)) - from.max(0.0)).max(0.0);
        match spans.iter_mut().find(|(b, _)| (*b - bpm).abs() < 0.01) {
            Some(span) => span.1 += lasted,
            None => spans.push((bpm, lasted)),
        }
    }
    if points.is_empty() {
        return [0.0; 3];
    }
    let most = spans.iter().fold((0.0, -1.0), |best, &s| if s.1 > best.1 { s } else { best }).0;
    [lo as f32, hi as f32, most as f32]
}

fn inspect(path: &Path) -> Option<MapInfo> {
    let bytes = std::fs::read(path).ok()?;
    let map = osu::parse(&bytes).ok()?;
    if map.objects.is_empty() {
        return None;
    }
    let last = map.objects.iter().map(|o| o.time_ms).fold(0.0, f64::max);
    let count = |want: fn(&osu::RawKind) -> bool| map.objects.iter().filter(|o| want(&o.kind)).count() as u32;
    let artist = map.artist.trim().to_string();
    let title = map.title.trim().to_string();
    let version = map.version.trim().to_string();
    let creator = map.creator.trim().to_string();
    let haystack =
        format!("{artist} {title} {version} {creator} {} {}", map.source.trim(), map.tags.trim()).to_lowercase();
    Some(MapInfo {
        path: path.to_path_buf(),
        cs: map.difficulty.cs,
        ar: map.difficulty.ar,
        od: map.difficulty.od,
        hp: map.difficulty.hp,
        objects: map.objects.len() as u32,
        circles: count(|k| matches!(k, osu::RawKind::Circle)),
        sliders: count(|k| matches!(k, osu::RawKind::Slider { .. })),
        spinners: count(|k| matches!(k, osu::RawKind::Spinner { .. })),
        length_ms: last.max(0.0) as u32,
        preview_ms: map.preview_ms.clamp(-1, i32::MAX as i64) as i32,
        bpm: tempo(&map, last),
        beat: map
            .timing_points
            .iter()
            .find(|p| p.uninherited && p.beat_length.is_finite() && p.beat_length > 0.0)
            .map_or((0.0, 0.0), |p| (p.time_ms, p.beat_length)),
        md5: md5(&bytes),
        audio: map.audio_filename,
        background: map.background,
        added: 0,
        stars: [0.0; 9],
        set: 0,
        artist,
        title,
        version,
        creator,
        haystack,
    })
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("osu")) {
            out.push(path);
        }
    }
}

/// Scan `songs`, using and refreshing the cache file. `done` counts files
/// as they are handled, out of `total`. Returns the maps sorted for display
/// and how many files had to be parsed.
pub fn scan(songs: &Path, cache_file: &Path, done: &AtomicUsize, total: &AtomicUsize) -> (Vec<MapInfo>, usize) {
    let mut files = Vec::new();
    collect(songs, &mut files);
    total.store(files.len(), Ordering::Relaxed);
    let mut cache = read_cache(cache_file);
    let mut parsed = 0;
    let mut lines = vec![HEADER.to_string()];
    let mut maps = Vec::new();
    for path in files {
        let meta = std::fs::metadata(&path).ok();
        let size = meta.as_ref().map_or(0, |m| m.len());
        let mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let entry = match cache.remove(&path) {
            Some(e) if e.mtime == mtime && e.size == size => e,
            _ => {
                parsed += 1;
                Entry { mtime, size, info: inspect(&path) }
            }
        };
        lines.push(encode(&path, &entry));
        maps.extend(entry.info.map(|info| MapInfo { added: mtime, ..info }));
        done.fetch_add(1, Ordering::Relaxed);
    }
    if parsed > 0 || !cache.is_empty() {
        // Write beside the target and rename, so a crash cannot leave half a file.
        let tmp = cache_file.with_extension("tmp");
        let written = cache_file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&tmp, lines.join("\n")))
            .and_then(|_| std::fs::rename(&tmp, cache_file));
        if let Err(e) = written {
            eprintln!("rsu: could not write the library cache: {e}");
        }
    }
    group(&mut maps);
    (maps, parsed)
}

/// Put the maps of a folder next to each other, easiest first, and number
/// the folders.
pub fn group(maps: &mut [MapInfo]) {
    maps.sort_by(|a, b| {
        (a.path.parent().cmp(&b.path.parent()))
            .then(a.stars[0].total_cmp(&b.stars[0]))
            .then(a.objects.cmp(&b.objects))
            .then(a.version.cmp(&b.version))
    });
    let mut set = 0;
    for i in 0..maps.len() {
        if i > 0 && maps[i].path.parent() != maps[i - 1].path.parent() {
            set += 1;
        }
        maps[i].set = set;
    }
}

/// Star ratings from osu!'s own database next to the Songs folder, where
/// there is one: rsu has no difficulty calculator of its own yet.
fn rate(maps: &mut [MapInfo], songs: &Path) {
    let Some(data) = songs.parent().and_then(|p| std::fs::read(p.join("osu!.db")).ok()) else {
        return;
    };
    let Some(db) = osudb::read(&data) else {
        return;
    };
    for m in maps.iter_mut() {
        if let Some(e) = db.get(&m.md5) {
            m.stars = e.stars;
        }
    }
}

pub fn cache_file() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("takt/library"))
}

/// Everything the song browser shows.
pub struct Library {
    pub maps: Vec<MapInfo>,
    pub scores: Vec<Score>,
    /// The player's collections in osu!: a name and the MD5 of its maps.
    pub collections: Vec<(String, Vec<[u8; 16]>)>,
}

/// The collections osu! keeps next to the Songs folder, if there are any.
fn collections(songs: &Path) -> Vec<(String, Vec<[u8; 16]>)> {
    let data = songs.parent().and_then(|p| std::fs::read(p.join("collection.db")).ok());
    data.and_then(|d| osudb::collections(&d)).unwrap_or_default()
}

/// A scan in progress on another thread.
pub struct Scan {
    pub done: Arc<AtomicUsize>,
    pub total: Arc<AtomicUsize>,
    result: Receiver<Library>,
}

impl Scan {
    pub fn start(songs: PathBuf) -> Scan {
        let (done, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (tx, result) = channel();
        let (d, t) = (done.clone(), total.clone());
        let spawned = std::thread::Builder::new().name("rsu-library".into()).spawn(move || {
            let started = std::time::Instant::now();
            let cache = cache_file().unwrap_or_else(|| PathBuf::from("/nonexistent/rsu-library"));
            let (mut maps, parsed) = scan(&songs, &cache, &d, &t);
            rate(&mut maps, &songs);
            group(&mut maps);
            eprintln!(
                "rsu: library: {} beatmaps in {:.2} s ({} files read, the rest from the cache)",
                maps.len(),
                started.elapsed().as_secs_f64(),
                parsed
            );
            let scores = scores::load(Some(&songs));
            let collections = collections(&songs);
            let _ = tx.send(Library { maps, scores, collections });
        });
        if let Err(e) = spawned {
            eprintln!("rsu: could not start the library scan: {e}");
        }
        Scan { done, total, result }
    }

    /// The finished library, once.
    pub fn poll(&self) -> Option<Library> {
        self.result.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsu-library-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn map(title: &str, mode: u8, objects: usize) -> String {
        let mut s = format!(
            "osu file format v14\n[General]\nAudioFilename: a.mp3\nPreviewTime: 1234\nMode: {mode}\n[Metadata]\n\
Title:{title}\nArtist:Some Artist\nCreator:Mapper\nVersion:Hard\nSource:A Game\nTags:some tags\n[Difficulty]\n\
CircleSize:4\nOverallDifficulty:8\nApproachRate:9\nHPDrainRate:5\n[Events]\n0,0,\"bg.jpg\",0,0\n[TimingPoints]\n\
0,500,4,1,0,100,1,0\n2000,250,4,1,0,100,1,0\n2200,500,4,1,0,100,1,0\n[HitObjects]\n"
        );
        for i in 0..objects {
            s += &format!("100,100,{},1,0\n", 1000 + i * 500);
        }
        s
    }

    #[test]
    fn scan_lists_playable_maps_and_the_cache_spares_a_second_parse() {
        let dir = temp("scan");
        let songs = dir.join("Songs");
        std::fs::create_dir_all(songs.join("1 set")).unwrap();
        std::fs::create_dir_all(songs.join("2 set/nested")).unwrap();
        std::fs::write(songs.join("1 set/b.osu"), map("Zebra", 0, 10)).unwrap();
        std::fs::write(songs.join("1 set/taiko.osu"), map("Drums", 1, 10)).unwrap();
        std::fs::write(songs.join("2 set/nested/a.OSU"), map("Apple", 0, 3)).unwrap();
        std::fs::write(songs.join("2 set/empty.osu"), b"").unwrap();
        std::fs::write(songs.join("2 set/notes.txt"), b"not a map").unwrap();
        let cache = dir.join("cache/library");
        let (done, total) = (AtomicUsize::new(0), AtomicUsize::new(0));

        let (maps, parsed) = scan(&songs, &cache, &done, &total);
        assert_eq!((total.load(Ordering::Relaxed), done.load(Ordering::Relaxed), parsed), (4, 4, 4));
        let titles: Vec<_> = maps.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, ["Zebra", "Apple"], "only osu! standard maps with objects, folder by folder");
        assert_eq!((maps[0].objects, maps[0].length_ms, maps[0].preview_ms), (10, 5500, 1234));
        assert_eq!((maps[0].circles, maps[0].set, maps[1].set), (10, 0, 1));
        assert_eq!(maps[1].haystack, "some artist apple hard mapper a game some tags");
        assert_eq!((maps[1].background.as_str(), maps[1].bpm), ("bg.jpg", [120.0, 240.0, 120.0]));
        assert_eq!(maps[1].beat, (0.0, 500.0));
        assert_eq!(maps[1].md5, md5(&std::fs::read(songs.join("2 set/nested/a.OSU")).unwrap()));

        // Nothing changed: everything comes from the cache, unplayable files included.
        let (again, parsed) = scan(&songs, &cache, &AtomicUsize::new(0), &AtomicUsize::new(0));
        assert_eq!(parsed, 0);
        assert_eq!(again, maps);

        // A changed file is read again; a deleted one disappears.
        std::fs::write(songs.join("1 set/b.osu"), map("Zebra Remix", 0, 12)).unwrap();
        std::fs::remove_file(songs.join("2 set/nested/a.OSU")).unwrap();
        let (third, parsed) = scan(&songs, &cache, &AtomicUsize::new(0), &AtomicUsize::new(0));
        assert_eq!(parsed, 1);
        assert_eq!(third.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), ["Zebra Remix"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_cache_is_ignored_not_trusted() {
        let dir = temp("damaged");
        let songs = dir.join("Songs");
        std::fs::create_dir_all(&songs).unwrap();
        std::fs::write(songs.join("a.osu"), map("Tabs\tand\ttabs", 0, 2)).unwrap();
        let cache = dir.join("library");
        std::fs::write(&cache, "some other file\nwith\tlines").unwrap();
        let (maps, parsed) = scan(&songs, &cache, &AtomicUsize::new(0), &AtomicUsize::new(0));
        assert_eq!((maps.len(), parsed), (1, 1));
        // The rewritten cache round-trips, tabs in titles and all.
        let (again, parsed) = scan(&songs, &cache, &AtomicUsize::new(0), &AtomicUsize::new(0));
        assert_eq!(parsed, 0);
        assert_eq!(again[0].title, "Tabs and tabs");
        let _ = std::fs::remove_dir_all(dir);
    }
}
