//! The music of the menus: the selected song plays from its preview point
//! (at the speed of the chosen mods, as in McOsu), can be paused and moved
//! through, and gives way to the next song when it ends in the main menu.
//! Tracks are played straight from their files (`rsu_audio::stream`), so
//! a song starts within milliseconds of being chosen.

use crate::transport::Transport;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use rsu_audio::clock::ClockModel;
use rsu_audio::stream::{Stream, Streamer, UNKNOWN};
use rsu_audio::SAMPLE_RATE;

/// A song the menus play.
pub struct Song {
    pub audio: PathBuf,
    /// "Artist - Title", for the player in the main menu.
    pub title: String,
    /// Where it starts, in frames.
    pub start: f64,
    /// First beat and length of a beat, in milliseconds.
    pub beat: Option<(f64, f64)>,
}

pub struct Jukebox {
    streamer: Option<Streamer>,
    playing: Option<Song>,
    /// The player paused it.
    pub paused: bool,
}

/// Where the song is, for the player to show.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub seconds: f64,
    /// `None` while the length of the track is not known.
    pub length: Option<f64>,
}

impl Jukebox {
    pub fn new(streamer: Option<Streamer>) -> Jukebox {
        Jukebox { streamer, playing: None, paused: false }
    }

    /// A new stream of the file at `path`; `None` without audio.
    pub fn open(&self, path: &Path) -> Option<Arc<Stream>> {
        self.streamer.as_ref().map(|s| s.open(path))
    }

    /// Whether the song's file could not be played.
    pub fn failed(&self, transport: &Transport) -> bool {
        self.playing.is_some() && transport.music().is_some_and(|s| s.failed())
    }

    /// The track at `path`, ready to be given to the mixer: what is
    /// playing already if it is that file, so a play picks up the decoded
    /// seconds of its preview. `None` without audio.
    pub fn stream_for(&self, transport: &Transport, path: &Path) -> Option<Arc<Stream>> {
        if let Some(s) = transport.music().filter(|s| s.path() == path) {
            return Some(s.clone());
        }
        self.streamer.as_ref().map(|s| s.open(path))
    }

    pub fn song(&self) -> Option<&Song> {
        self.playing.as_ref()
    }

    /// Play `song` from its start at `speed`.
    pub fn play(&mut self, transport: &mut Transport, song: Song, speed: f64, now: u64) {
        let Some(stream) = self.stream_for(transport, &song.audio) else { return };
        // A preview may start a few milliseconds off in some Ogg files; it
        // starts at once instead of decoding everything before it.
        stream.set_exact(false);
        transport.load(Some(stream), 0, None);
        transport.set_paused(false, now);
        transport.set_speed(speed, now);
        transport.seek(song.start, now);
        self.paused = false;
        self.playing = Some(song);
    }

    /// The menus are left for a play: what plays from now on is its business.
    pub fn forget(&mut self) {
        self.playing = None;
        self.paused = false;
    }

    pub fn toggle_pause(&mut self, transport: &mut Transport, now: u64) {
        if self.playing.is_some() {
            self.paused = !self.paused;
            transport.set_paused(self.paused, now);
        }
    }

    /// Go to `fraction` (0..1) of the track.
    pub fn seek_to(&mut self, transport: &mut Transport, fraction: f64, now: u64) {
        let Some(length) = self.length_frames(transport) else { return };
        transport.seek((fraction.clamp(0.0, 1.0) * length).floor(), now);
    }

    /// Follow the speed of the mods chosen in the song browser.
    pub fn set_speed(&mut self, transport: &mut Transport, speed: f64, now: u64) {
        if self.playing.is_some() && transport.speed != speed {
            transport.set_speed(speed, now);
        }
    }

    fn length_frames(&self, transport: &Transport) -> Option<f64> {
        self.playing.as_ref()?;
        let n = transport.music()?.length();
        (n != UNKNOWN && n > 0).then_some(n as f64)
    }

    pub fn progress(&self, transport: &Transport, clock: &ClockModel, now: u64) -> Option<Progress> {
        self.playing.as_ref()?;
        let pos = if clock.valid { clock.pos_at(now).max(0.0) } else { 0.0 };
        let length = self.length_frames(transport).map(|n| n / SAMPLE_RATE as f64);
        let seconds = pos / SAMPLE_RATE as f64;
        Some(Progress { seconds: length.map_or(seconds, |l| seconds.min(l)), length })
    }

    /// How far into the current beat the song is, 0 to 1.
    pub fn beat_phase(&self, clock: &ClockModel, now: u64) -> Option<f32> {
        let (first, length) = self.playing.as_ref()?.beat?;
        if !clock.valid || self.paused {
            return None;
        }
        let ms = clock.pos_at(now) / SAMPLE_RATE as f64 * 1000.0;
        Some(((ms - first) / length).rem_euclid(1.0) as f32)
    }

    /// Whether the song has played to the end of its track.
    pub fn ended(&self, transport: &Transport, clock: &ClockModel, now: u64) -> bool {
        let exact = transport.music().is_some_and(|s| s.length_exact());
        match (self.length_frames(transport), exact && clock.valid && !self.paused) {
            (Some(length), true) => clock.pos_at(now) >= length,
            _ => transport.music().is_some_and(|s| s.failed()) && self.playing.is_some(),
        }
    }

    /// Start the song over from its preview point.
    pub fn restart(&mut self, transport: &mut Transport, now: u64) {
        if let Some(song) = &self.playing {
            transport.seek(song.start, now);
        }
    }
}
