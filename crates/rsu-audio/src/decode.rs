//! Decoding audio files: [`Source`] reads a file's frames at its own rate
//! from any frame on, [`load`] decodes a whole track into the engine format
//! at once.

use crate::resample::resample_stereo;
use crate::SAMPLE_RATE;
use std::fs::File;
use std::path::{Path, PathBuf};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecType, Decoder, DecoderOptions, CODEC_TYPE_MP3, CODEC_TYPE_NULL, CODEC_TYPE_VORBIS};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Debug)]
pub enum DecodeError {
    Io(std::io::Error),
    Format(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "{e}"),
            DecodeError::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn fmt_err(e: SymError) -> DecodeError {
    DecodeError::Format(e.to_string())
}

/// Samples an MP3 decoder emits before the first real one.
const MP3_DECODER_DELAY: u32 = 529;

/// Frames decoded and thrown away before the frame a seek is for: a
/// decoder's output depends on the packets before it (MP3's overlapping
/// transforms and filter bank, Vorbis's overlapping blocks), so it needs
/// a run-up to produce exactly what a decode from the start does.
const PREROLL: u64 = 1152 * 10;

/// Packets with sound decoded and dropped after a seek (see `Source::resync`).
const RESYNC_PACKETS: u8 = 2;

/// A file's audio as interleaved stereo at the file's own rate, read
/// forward from any frame.
pub struct Source {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    codec: CodecType,
    /// Silent frames put in front of the decoded ones (see `open`).
    keep: u64,
    pub rate: u32,
    /// Frames in the file, if its header says.
    pub frames: Option<u64>,
    /// The frame the next decoded packet starts with.
    pos: u64,
    /// Frames before this one are decoded but not handed out.
    skip_to: u64,
    /// Just after a seek: how many packets with sound are still to be
    /// thrown away before the next one is placed by its own timestamp. A
    /// decoder that has just been reset does not know the packets before:
    /// Vorbis gives nothing for the first one and a block of the wrong
    /// length for the second (a quarter of a short or long block off), so
    /// counting frames from where the seek landed would put everything
    /// that much early or late.
    resync: Option<u8>,
    buf: Option<SampleBuffer<f32>>,
    stereo: Vec<f32>,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source, DecodeError> {
        let file = File::open(path).map_err(DecodeError::Io)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        // "Gapless": drop the samples an encoder declares as its own delay
        // (for MP3 with a LAME header, about 25 ms of nothing at the start).
        // osu! plays such files without that delay and beatmaps are timed
        // to it, so leaving it in would play the music that much late on
        // those maps only. (Measured on a library of beatmaps: audio onsets
        // against object times agree between Vorbis, untagged MP3 and
        // LAME-tagged MP3 only this way.)
        let options = FormatOptions { enable_gapless: true, ..Default::default() };
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &options, &MetadataOptions::default())
            .map_err(fmt_err)?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| DecodeError::Format("no audio track".into()))?;
        let track_id = track.id;
        let codec = track.codec_params.codec;
        let rate = track.codec_params.sample_rate.ok_or_else(|| DecodeError::Format("no sample rate".into()))?;
        // An MP3 header that declares no encoder delay still makes the
        // reader drop the decoder's own 529 samples. Beatmaps with such
        // files are timed with those samples in place, so they are put back.
        let keep = match (track.codec_params.codec, track.codec_params.delay) {
            (CODEC_TYPE_MP3, Some(delay)) if delay <= MP3_DECODER_DELAY => delay as u64,
            _ => 0,
        };
        let frames = track.codec_params.n_frames.map(|n| n + keep);
        let decoder =
            symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(fmt_err)?;
        Ok(Source {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            codec,
            keep,
            rate,
            frames,
            pos: 0,
            skip_to: 0,
            resync: None,
            buf: None,
            stereo: Vec::new(),
        })
    }

    /// Go on from frame `target`: the next frames handed out start there.
    ///
    /// Frames are placed by the timestamps the file gives its packets. In
    /// MP3s those agree with counting the frames from the start (checked on
    /// hundreds of beatmap songs), but some Ogg Vorbis files carry granule
    /// positions that are off by up to a few hundred frames in stretches
    /// (ffmpeg agrees with counting, not with the timestamps). With `exact`
    /// such files are decoded from the start up to `target` instead: slower,
    /// but frame for frame what a decode of the whole file gives.
    pub fn seek(&mut self, target: u64, exact: bool) -> Result<(), DecodeError> {
        let ts = target.saturating_sub(self.keep + PREROLL);
        let trust_timestamps = !(exact && self.codec == CODEC_TYPE_VORBIS);
        let seeked = if ts == 0 || !trust_timestamps {
            None
        } else {
            self.format.seek(SeekMode::Accurate, SeekTo::TimeStamp { ts, track_id: self.track_id }).ok()
        };
        match seeked {
            Some(at) => {
                self.decoder.reset();
                self.pos = at.actual_ts + self.keep;
                self.resync = Some(RESYNC_PACKETS);
            }
            // From the start, or where the format cannot seek: open afresh
            // and decode up to the frame.
            None => *self = Source::open(&self.path)?,
        }
        self.skip_to = target;
        Ok(())
    }

    /// The next frames, or `None` at the end of the track.
    pub fn next_frames(&mut self) -> Result<Option<&[f32]>, DecodeError> {
        loop {
            let mut start = self.pos;
            if start < self.keep {
                self.stereo.clear();
                self.stereo.resize((self.keep - start) as usize * 2, 0.0);
                self.pos = self.keep;
            } else {
                let packet = match self.format.next_packet() {
                    Ok(p) => p,
                    Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                    Err(SymError::ResetRequired) => return Ok(None),
                    Err(e) => return Err(fmt_err(e)),
                };
                if packet.track_id() != self.track_id {
                    continue;
                }
                let decoded = match self.decoder.decode(&packet) {
                    Ok(d) => d,
                    // A corrupt packet is played as silence of its length, as
                    // players do, so that what follows keeps its time.
                    Err(SymError::DecodeError(_)) if self.resync.is_none() => {
                        start = self.pos;
                        self.stereo.clear();
                        self.stereo.resize(packet.dur() as usize * 2, 0.0);
                        self.pos += packet.dur();
                        if self.pos <= self.skip_to {
                            continue;
                        }
                        let cut = (self.skip_to.saturating_sub(start) as usize * 2).min(self.stereo.len());
                        return Ok(Some(&self.stereo[cut..]));
                    }
                    Err(SymError::DecodeError(_)) => continue,
                    Err(e) => return Err(fmt_err(e)),
                };
                match self.resync {
                    None => {}
                    Some(_) if decoded.frames() == 0 => continue,
                    Some(0) => {
                        self.resync = None;
                        self.pos = packet.ts() + self.keep;
                    }
                    Some(n) => {
                        self.resync = Some(n - 1);
                        continue;
                    }
                }
                start = self.pos;
                let spec = *decoded.spec();
                let channels = spec.channels.count();
                let sb = self.buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
                if sb.capacity() < decoded.capacity() {
                    *sb = SampleBuffer::new(decoded.capacity() as u64, spec);
                }
                sb.copy_interleaved_ref(decoded);
                let s = sb.samples();
                self.stereo.clear();
                match channels {
                    0 => {}
                    1 => self.stereo.extend(s.iter().flat_map(|&x| [x, x])),
                    2 => self.stereo.extend_from_slice(s),
                    n => self.stereo.extend(s.chunks_exact(n).flat_map(|f| [f[0], f[1]])),
                }
                self.pos += (self.stereo.len() / 2) as u64;
            }
            if self.pos <= self.skip_to {
                continue;
            }
            let cut = (self.skip_to.saturating_sub(start) as usize * 2).min(self.stereo.len());
            return Ok(Some(&self.stereo[cut..]));
        }
    }
}

/// Decode `path` to interleaved stereo f32 at [`SAMPLE_RATE`].
pub fn load(path: &Path) -> Result<Vec<f32>, DecodeError> {
    let mut source = Source::open(path)?;
    let mut stereo: Vec<f32> = Vec::new();
    while let Some(frames) = source.next_frames()? {
        stereo.extend_from_slice(frames);
    }
    if stereo.is_empty() {
        return Err(DecodeError::Format("no audio decoded".into()));
    }
    Ok(resample_stereo(&stereo, source.rate, SAMPLE_RATE))
}
