//! Music played straight from the file: a decoding thread keeps a few
//! seconds of the track ready ahead of where the mixer reads, so a song
//! starts within milliseconds of being chosen whatever its length, and only
//! those seconds are ever in memory.
//!
//! The mixer and the decoding thread share a [`Stream`]: a ring of frames
//! (48 kHz stereo, like everything the mixer plays) and a handful of atomic
//! positions. Neither side waits for the other. The mixer says which track
//! frame it wants ([`Stream::read`] and the "want" position after a jump);
//! the decoder fills the ring from there and publishes how far it got. A
//! frame the decoder has not delivered yet is played as silence and
//! counted as an underrun.
//!
//! Every jump starts a new *generation*: the decoder begins a fresh segment
//! of the ring at the wanted frame, and the mixer only believes data of the
//! generation it asked for. The samples after a seek are the same as those
//! a decode of the whole file gives (`tests` and `rsu-sim stream-check`
//! check that), so timing does not depend on how a song was reached.

use crate::decode::{self, DecodeError};
use crate::resample::Resampler;
use crate::{CHANNELS, SAMPLE_RATE};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// Frames in the ring: about 11 seconds.
pub const CAPACITY: u64 = 1 << 19;
/// How far behind the mixer's read position the decoder keeps away from:
/// more than the mixer can read in one cycle at any speed.
const GUARD: u64 = CAPACITY / 4;
/// A wanted frame this far past the delivered data is a jump, not a wait.
const FAR_AHEAD: u64 = CAPACITY / 2;

const GEN_SHIFT: u32 = 40;
const POS_MASK: u64 = (1 << GEN_SHIFT) - 1;

fn pack(gen: u32, pos: u64) -> u64 {
    ((gen as u64) << GEN_SHIFT) | (pos & POS_MASK)
}

fn unpack(v: u64) -> (u32, u64) {
    ((v >> GEN_SHIFT) as u32, v & POS_MASK)
}

/// Not known (yet).
pub const UNKNOWN: u64 = u64::MAX;

/// One track being played from its file. Shared between the game (which
/// holds it), the mixer (which reads it) and the decoding thread.
pub struct Stream {
    path: PathBuf,
    /// Interleaved stereo f32, stored as bits; frame `p` lives in slot `p % CAPACITY`.
    ring: Box<[AtomicU32]>,
    /// The segment of the current generation: its first frame, and the
    /// frame after the last one delivered. Written by the decoder.
    seg_start: AtomicU64,
    seg_end: AtomicU64,
    /// The generation the mixer asked for and the frame it starts at.
    /// Written by the mixer.
    want: AtomicU64,
    /// The first frame the mixer reads in its current cycle.
    read: AtomicU64,
    /// Whether a mixer is playing it: only then is it worth decoding.
    attached: AtomicBool,
    /// Frames in the whole track, once the decoder has reached its end.
    length: AtomicU64,
    /// Frames in the whole track as the file's header says, if it says.
    estimate: AtomicU64,
    /// Cycles in which a frame the mixer needed was not there yet.
    underruns: AtomicU32,
    /// The file could not be opened or decoded.
    failed: AtomicBool,
    /// All of the track is in the ring and stays there (see [`Stream::preloaded`]).
    whole: bool,
    /// Jumps must land on exactly the frames a decode from the start gives,
    /// whatever it costs (see `decode::Source::seek`): set for plays, where
    /// timing matters; previews take the fast way.
    exact: AtomicBool,
}

impl Stream {
    fn new(path: PathBuf) -> Stream {
        let ring = (0..CAPACITY as usize * CHANNELS).map(|_| AtomicU32::new(0)).collect();
        Stream {
            path,
            ring,
            seg_start: AtomicU64::new(pack(0, 0)),
            seg_end: AtomicU64::new(pack(0, 0)),
            want: AtomicU64::new(pack(0, 0)),
            read: AtomicU64::new(0),
            attached: AtomicBool::new(false),
            length: AtomicU64::new(UNKNOWN),
            estimate: AtomicU64::new(UNKNOWN),
            underruns: AtomicU32::new(0),
            failed: AtomicBool::new(false),
            whole: false,
            exact: AtomicBool::new(false),
        }
    }

    /// Make jumps exact (for plays) or fast (for previews).
    pub fn set_exact(&self, exact: bool) {
        self.exact.store(exact, Ordering::Release);
    }

    /// A track given whole (interleaved stereo at [`SAMPLE_RATE`], at most
    /// [`CAPACITY`] frames): nothing to decode, every jump lands at once.
    pub fn preloaded(samples: &[f32]) -> Arc<Stream> {
        let frames = (samples.len() / CHANNELS).min(CAPACITY as usize) as u64;
        let mut stream = Stream::new(PathBuf::new());
        for (p, f) in samples.chunks_exact(CHANNELS).take(frames as usize).enumerate() {
            stream.put(p as u64, f[0], f[1]);
        }
        stream.seg_end = AtomicU64::new(pack(0, frames));
        stream.length = AtomicU64::new(frames);
        stream.whole = true;
        Arc::new(stream)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Frames in the track: exact once decoded to the end, otherwise what
    /// the file's header claims. [`UNKNOWN`] if neither is known.
    pub fn length(&self) -> u64 {
        match self.length.load(Ordering::Acquire) {
            UNKNOWN => self.estimate.load(Ordering::Acquire),
            n => n,
        }
    }

    /// Whether the end of the track is known exactly.
    pub fn length_exact(&self) -> bool {
        self.length.load(Ordering::Acquire) != UNKNOWN
    }

    pub fn underruns(&self) -> u32 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Whether frames from `pos` on are ready for the mixer (up to `frames`
    /// of them, or to the end of the track).
    pub fn ready_at(&self, pos: u64, frames: u64) -> bool {
        let (gen, _) = unpack(self.want.load(Ordering::Acquire));
        let (eg, end) = unpack(self.seg_end.load(Ordering::Acquire));
        let (sg, start) = unpack(self.seg_start.load(Ordering::Acquire));
        let length = self.length.load(Ordering::Acquire);
        self.failed() || (eg == gen && sg == gen && start <= pos && (end >= pos + frames || end >= length))
    }

    fn slot(&self, pos: u64) -> usize {
        (pos % CAPACITY) as usize * CHANNELS
    }

    fn get(&self, pos: u64) -> [f32; 2] {
        let i = self.slot(pos);
        [f32::from_bits(self.ring[i].load(Ordering::Relaxed)), f32::from_bits(self.ring[i + 1].load(Ordering::Relaxed))]
    }

    fn put(&self, pos: u64, l: f32, r: f32) {
        let i = self.slot(pos);
        self.ring[i].store(l.to_bits(), Ordering::Relaxed);
        self.ring[i + 1].store(r.to_bits(), Ordering::Relaxed);
    }
}

/// The mixer's side of a stream: what it last asked for. Real-time safe:
/// nothing here allocates, locks or waits.
pub struct StreamReader {
    stream: Arc<Stream>,
    gen: u32,
    want: u64,
    last_read: u64,
}

/// A view of what is ready for one mixer cycle.
pub struct Window<'a> {
    stream: &'a Stream,
    /// Frames `lo..hi` are there to be read.
    lo: u64,
    hi: u64,
    /// The end of the track, if known.
    length: u64,
    pub missing: bool,
}

impl Window<'_> {
    /// The frame at `pos`, or silence if it is not there (or past the end).
    #[inline]
    pub fn frame(&mut self, pos: u64) -> [f32; 2] {
        if pos >= self.lo && pos < self.hi {
            self.stream.get(pos)
        } else {
            if pos < self.length {
                self.missing = true;
            }
            [0.0; 2]
        }
    }
}

impl StreamReader {
    /// Take over a stream. Call on the audio thread when it starts playing it.
    pub fn attach(stream: Arc<Stream>) -> StreamReader {
        let (gen, want) = unpack(stream.want.load(Ordering::Acquire));
        stream.attached.store(true, Ordering::Release);
        StreamReader { stream, gen, want, last_read: want }
    }

    /// Give the stream back; it is no longer decoded.
    pub fn detach(self) -> Arc<Stream> {
        self.stream.attached.store(false, Ordering::Release);
        self.stream
    }

    fn ask(&mut self, pos: u64) {
        self.gen = self.gen.wrapping_add(1) & 0x00ff_ffff;
        self.want = pos;
        self.stream.want.store(pack(self.gen, pos), Ordering::Release);
    }

    /// The cycle will read frames `first..=last` of the track. Says so to
    /// the decoder, asks for a jump if they are elsewhere, and returns what
    /// can be read now.
    pub fn window(&mut self, first: u64, last: u64) -> Window<'_> {
        let s = &*self.stream;
        s.read.store(first, Ordering::Release);
        if s.whole {
            let length = s.length.load(Ordering::Relaxed);
            return Window { stream: &self.stream, lo: 0, hi: length, length, missing: false };
        }
        let (eg, end) = unpack(s.seg_end.load(Ordering::Acquire));
        let (sg, start) = unpack(s.seg_start.load(Ordering::Acquire));
        let length = s.length.load(Ordering::Acquire);
        let current = eg == self.gen && sg == self.gen;
        // Where the data of this generation is still intact.
        let lo = if current { start.max(end.saturating_sub(CAPACITY - GUARD)) } else { self.want };
        let hi = if current { end } else { self.want };
        // Going back always starts over: the decoder may still be filling
        // the ring as far ahead as the old position allowed.
        let jump = first < self.last_read
            || first < self.want
            || (current && first < lo)
            || first > hi.max(self.want) + FAR_AHEAD;
        self.last_read = first;
        let _ = last;
        if jump && first < length {
            self.ask(first);
            // Nothing is there yet; frames read from it count as missing.
            return Window { stream: &self.stream, lo: 0, hi: 0, length, missing: false };
        }
        let (lo, hi) = if current { (lo, hi) } else { (0, 0) };
        Window { stream: &self.stream, lo, hi, length, missing: false }
    }

    /// Count a cycle that had to play silence for missing frames.
    pub fn underrun(&self) {
        self.stream.underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stream(&self) -> &Arc<Stream> {
        &self.stream
    }
}

// ---- the decoding thread ------------------------------------------------

/// Opens streams and keeps the ones being played filled.
pub struct Streamer {
    new: Sender<Weak<Stream>>,
    thread: std::thread::Thread,
}

impl Streamer {
    pub fn start() -> std::io::Result<Streamer> {
        let (tx, rx) = channel();
        let handle = std::thread::Builder::new().name("rsu-stream".into()).spawn(move || serve(rx))?;
        Ok(Streamer { new: tx, thread: handle.thread().clone() })
    }

    /// A stream of the file at `path`. Nothing is read until a mixer plays it.
    pub fn open(&self, path: &Path) -> Arc<Stream> {
        let stream = Arc::new(Stream::new(path.to_path_buf()));
        let _ = self.new.send(Arc::downgrade(&stream));
        self.thread.unpark();
        stream
    }

    /// Have the thread look at its streams now rather than at its next
    /// round (after a jump, say).
    pub fn poke(&self) {
        self.thread.unpark();
    }
}

struct Served {
    stream: Weak<Stream>,
    decoder: Option<Decoder>,
    gen: u32,
    /// The next frame to deliver.
    end: u64,
    done: bool,
}

fn serve(rx: Receiver<Weak<Stream>>) {
    let mut served: Vec<Served> = Vec::new();
    loop {
        loop {
            match rx.try_recv() {
                Ok(stream) => served.push(Served { stream, decoder: None, gen: 0, end: 0, done: false }),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        served.retain(|s| s.stream.strong_count() > 0);
        let mut busy = false;
        for s in &mut served {
            let Some(stream) = s.stream.upgrade() else { continue };
            if stream.attached.load(Ordering::Acquire) && !stream.failed() {
                busy |= fill(s, &stream);
            }
        }
        if !busy {
            std::thread::park_timeout(Duration::from_millis(2));
        }
    }
}

/// Decode some of `stream` if it needs it; whether anything was done.
fn fill(s: &mut Served, stream: &Stream) -> bool {
    let (gen, want) = unpack(stream.want.load(Ordering::Acquire));
    if s.decoder.is_none() || gen != s.gen {
        let exact = stream.exact.load(Ordering::Acquire);
        let decoder = match s.decoder.take() {
            Some(mut d) => d.seek(want, exact).map(|_| d),
            None => Decoder::open(&stream.path, want, exact),
        };
        match decoder {
            Ok(d) => {
                if let Some(n) = d.estimate {
                    stream.estimate.store(n, Ordering::Release);
                }
                s.decoder = Some(d);
            }
            Err(e) => {
                eprintln!("rsu: cannot play {}: {e}", stream.path.display());
                stream.failed.store(true, Ordering::Release);
                return false;
            }
        }
        s.gen = gen;
        s.end = want;
        s.done = false;
        stream.seg_start.store(pack(gen, want), Ordering::Release);
        stream.seg_end.store(pack(gen, want), Ordering::Release);
    }
    if s.done {
        return false;
    }
    let Some(decoder) = &mut s.decoder else { return false };
    let mut worked = false;
    // A few packets at a time, then look at the want again.
    for _ in 0..8 {
        let read = stream.read.load(Ordering::Acquire).max(want);
        if s.end >= read + CAPACITY - GUARD {
            break;
        }
        let room = (read + CAPACITY - GUARD - s.end) as usize;
        let before = s.end;
        let out = match decoder.next(room) {
            Ok(Some(out)) => out,
            Ok(None) => {
                stream.length.store(s.end, Ordering::Release);
                s.done = true;
                return true;
            }
            Err(e) => {
                eprintln!("rsu: {}: {e}", stream.path.display());
                stream.length.store(s.end, Ordering::Release);
                s.done = true;
                return true;
            }
        };
        for f in out.chunks_exact(CHANNELS) {
            stream.put(s.end, f[0], f[1]);
            s.end += 1;
        }
        stream.seg_end.store(pack(s.gen, s.end), Ordering::Release);
        worked |= s.end > before;
        // A new jump is waiting: go and serve it.
        if unpack(stream.want.load(Ordering::Acquire)).0 != s.gen {
            return true;
        }
    }
    worked
}

/// Decodes a file into 48 kHz stereo from any frame on.
pub struct Decoder {
    source: decode::Source,
    resampler: Resampler,
    /// The output frame the next call delivers first.
    next_out: u64,
    pub estimate: Option<u64>,
    out: Vec<f32>,
}

impl Decoder {
    /// Open `path` positioned at output frame `pos` (exactly: see `Source::seek`).
    pub fn open(path: &Path, pos: u64, exact: bool) -> Result<Decoder, DecodeError> {
        let source = decode::Source::open(path)?;
        let rate = source.rate;
        let estimate = source.frames.map(|n| Resampler::out_len(n, rate, SAMPLE_RATE));
        let mut d =
            Decoder { source, resampler: Resampler::new(rate, SAMPLE_RATE), next_out: 0, estimate, out: Vec::new() };
        d.seek(pos, exact)?;
        Ok(d)
    }

    /// Continue from output frame `pos`.
    pub fn seek(&mut self, pos: u64, exact: bool) -> Result<(), DecodeError> {
        let first_in = self.resampler.first_input(pos);
        self.source.seek(first_in, exact)?;
        self.resampler.restart(pos, first_in);
        self.next_out = pos;
        Ok(())
    }

    /// The next output frames, at most `max` of them; `None` at the end.
    pub fn next(&mut self, max: usize) -> Result<Option<&[f32]>, DecodeError> {
        self.out.clear();
        // What the resampler holds goes out first; the file is read only
        // when that is used up, so nothing is left behind at the end.
        loop {
            self.resampler.drain(&mut self.out, max);
            if !self.out.is_empty() {
                break;
            }
            if self.resampler.finished() {
                return Ok(None);
            }
            match self.source.next_frames()? {
                Some(input) => self.resampler.push(input),
                None => {
                    self.resampler.finish();
                }
            }
        }
        self.next_out += (self.out.len() / CHANNELS) as u64;
        Ok(Some(&self.out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAV file whose samples count up, at `rate`.
    fn wav(dir: &Path, name: &str, rate: u32, frames: usize) -> PathBuf {
        let path = dir.join(name);
        let mut data = Vec::new();
        for i in 0..frames {
            let l = ((i as f32 * 0.013).sin() * 12000.0) as i16;
            let r = ((i as f32 * 0.007).cos() * 9000.0) as i16;
            data.extend_from_slice(&l.to_le_bytes());
            data.extend_from_slice(&r.to_le_bytes());
        }
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * 4).to_le_bytes());
        f.extend_from_slice(&4u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&data);
        std::fs::write(&path, f).unwrap();
        path
    }

    fn tempdir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rsu-stream-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn decode_from(path: &Path, pos: u64, frames: usize) -> Vec<f32> {
        decode_in_pieces(path, pos, frames, 4096)
    }

    fn decode_in_pieces(path: &Path, pos: u64, frames: usize, piece: usize) -> Vec<f32> {
        let mut d = Decoder::open(path, pos, true).unwrap();
        let mut out = Vec::new();
        while out.len() < frames * CHANNELS {
            match d.next(piece).unwrap() {
                Some(chunk) => out.extend_from_slice(chunk),
                None => break,
            }
        }
        out.truncate(frames * CHANNELS);
        out
    }

    #[test]
    fn a_seek_gives_the_samples_of_a_whole_decode() {
        let dir = tempdir();
        for rate in [44_100, 48_000, 22_050] {
            let path = wav(&dir, &format!("t{rate}.wav"), rate, rate as usize * 3);
            let whole = decode::load(&path).unwrap();
            let all = decode_from(&path, 0, usize::MAX / 4);
            assert_eq!(all.len(), whole.len(), "{rate}: same length");
            assert!(all == whole, "{rate}: from the start");
            // Taken a few frames at a time (as the ring's free room allows),
            // nothing is lost at the end.
            for piece in [1, 77, 1000] {
                let small = decode_in_pieces(&path, 1000, usize::MAX / 4, piece);
                assert!(small == whole[2000..], "{rate} Hz in pieces of {piece}");
            }
            for pos in [1u64, 1000, 47_999, 70_001, (whole.len() / 2 - 10) as u64] {
                let part = decode_from(&path, pos, 5000);
                let from = pos as usize * 2;
                let expect = &whole[from..(from + 10_000).min(whole.len())];
                assert!(part == expect, "{rate} Hz from frame {pos}");
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    /// Plays the mixer's part by hand: asks, waits for the decoder, reads.
    #[test]
    fn the_thread_fills_what_the_reader_asks_for_and_follows_jumps() {
        let dir = tempdir();
        let path = wav(&dir, "s.wav", 44_100, 44_100 * 30);
        let whole = decode::load(&path).unwrap();
        let streamer = Streamer::start().unwrap();
        let stream = streamer.open(&path);
        let mut reader = StreamReader::attach(stream.clone());
        let check = |reader: &mut StreamReader, at: u64| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let mut w = reader.window(at, at + 63);
                if !w.missing && (at..at + 64).all(|p| p < w.hi) {
                    for p in at..at + 64 {
                        let f = w.frame(p);
                        assert_eq!(f, [whole[p as usize * 2], whole[p as usize * 2 + 1]], "frame {p}");
                    }
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "frame {at} never came");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        check(&mut reader, 0);
        check(&mut reader, 64);
        // Far forward, then back.
        check(&mut reader, 1_000_000);
        check(&mut reader, 1_000_064);
        check(&mut reader, 5_000);
        // Play on for longer than the ring holds.
        let mut at = 5_000;
        while at < 5_000 + CAPACITY * 2 {
            check(&mut reader, at);
            at += 4096;
        }
        // Past the end: silence, and the length is known.
        let end = (whole.len() / 2) as u64;
        check(&mut reader, end - 64);
        assert_eq!(stream.length(), end);
        let mut w = reader.window(end, end + 63);
        assert_eq!(w.frame(end), [0.0; 2]);
        assert!(!w.missing, "past the end is not an underrun");
        std::fs::remove_dir_all(dir).ok();
    }
}
