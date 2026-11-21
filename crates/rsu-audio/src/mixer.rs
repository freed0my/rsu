//! The real-time mix callback. Called by the device backend with an output
//! buffer; must not block, allocate or take locks.

use crate::clock::{ClockModel, Dll};
use crate::handoff::{Handoff, Retired};
use crate::stream::{Stream, StreamReader};
use crate::{CHANNELS, SAMPLE_RATE};
use std::sync::Arc;
use rsu_rt::{seqlock, spsc};

/// Commands from the game thread, delivered through an SPSC queue and
/// applied at the start of the next cycle.
#[derive(Clone, Copy, Debug)]
pub enum AudioCmd {
    /// Short tick, as soon as possible (hit feedback).
    Click {
        gain: f32,
    },
    /// A sample from the table, as soon as possible. `t_event_ns` is the
    /// CLOCK_MONOTONIC time of the input that caused it (0 = not measured);
    /// the mixer reports back how long after it the sound becomes audible.
    Play {
        sample: u16,
        gain: f32,
        /// -1.0 (left) ..= 1.0 (right): the far channel is turned down in
        /// proportion, the near one is left alone.
        pan: f32,
        t_event_ns: u64,
    },
    /// A sample that starts when the music reaches `at` (a timeline
    /// position in frames), to the exact output frame. A position already
    /// passed means now. Seeking, pausing and speed changes cancel it.
    PlayAt {
        sample: u16,
        gain: f32,
        pan: f32,
        at: f64,
    },
    /// A sample that repeats for as long as it is asked for (a slider
    /// being held, a spinner being spun). One per `slot`. Sending the same
    /// sample again only updates its gain, pan and rate; `None` stops it
    /// at once.
    Loop {
        slot: u8,
        sample: Option<u16>,
        gain: f32,
        pan: f32,
        /// Playback speed; pitch follows it.
        rate: f32,
    },
    MusicGain(f32),
    MetronomeGain(f32),
    /// Replace the track (or remove it). `start` is the timeline position
    /// of its first sample. The timeline itself is not moved; seek for that.
    SetMusic {
        music: Option<Handoff<Stream>>,
        start: u64,
    },
    /// Replace the hit sound table. Sounds still playing are cut.
    SetSamples(Handoff<[Arc<[f32]>]>),
    /// Jump to a timeline position, in frames.
    Seek(f64),
    /// Playback rate; 1.0 is normal. Pitch follows the rate.
    Speed(f64),
    Pause(bool),
}

/// What the device backend knows about the cycle it is asking us to fill.
#[derive(Clone, Copy, Debug)]
pub struct Cycle {
    /// CLOCK_MONOTONIC time the cycle refers to.
    pub now_ns: u64,
    /// Frames between writing a sample now and hearing it; negative if the
    /// device could not tell.
    pub delay_frames: f64,
}

/// A fixed beat grid in timeline frames: beat k sits at `offset + k * period`.
#[derive(Clone, Copy, Debug)]
pub struct BeatGrid {
    pub period_frames: f64,
    pub offset_frames: f64,
}

pub struct MixerConfig {
    /// The track, played from its file.
    pub music: Option<Arc<Stream>>,
    /// Timeline position (frames) at which the first music sample plays;
    /// the silence before it is the lead-in.
    pub music_start: u64,
    pub music_gain: f32,
    pub metronome: Option<BeatGrid>,
    pub metronome_gain: f32,
    /// Hit sound samples, addressed by [`AudioCmd::Play`].
    pub samples: Arc<[Arc<[f32]>]>,
}

#[derive(Clone, Copy)]
struct Voice {
    /// Frames since the voice started; negative while it waits for its
    /// sample-accurate start inside a later part of the buffer.
    age: i32,
    step: f32, // phase increment per frame, radians
    gain: f32,
    active: bool,
}

/// A sample being played back.
#[derive(Clone, Copy)]
struct SampleVoice {
    sample: u16,
    /// Next frame of the sample to play.
    at: u32,
    /// Left and right.
    gain: [f32; 2],
    /// Output frames still to wait before the first one sounds.
    wait: u32,
    active: bool,
}

/// A sample playing in a loop.
#[derive(Clone, Copy)]
struct LoopVoice {
    sample: u16,
    /// Position in the sample, in frames.
    at: f64,
    gain: [f32; 2],
    rate: f32,
    active: bool,
}

pub const LOOP_SLOTS: usize = 2;

/// What a pan position does to the two channels.
fn pan_gains(gain: f32, pan: f32) -> [f32; 2] {
    let pan = pan.clamp(-1.0, 1.0);
    [gain * (1.0 - pan.max(0.0)), gain * (1.0 + pan.min(0.0))]
}

const SAMPLE_VOICES: usize = 32;
const VOICES: usize = 16;
const VOICE_LEN: i32 = (SAMPLE_RATE / 20) as i32; // 50 ms
const VOICE_DECAY: f32 = 1.0 / (SAMPLE_RATE as f32 * 0.008);

pub struct Mixer {
    cfg: MixerConfig,
    music: Option<StreamReader>,
    cmds: spsc::Consumer<AudioCmd>,
    clock_tx: seqlock::Writer<ClockModel>,
    /// Input-to-audible latencies of measured sounds, in nanoseconds.
    latency_tx: spsc::Producer<u64>,
    /// Replaced buffers, returned for someone else to free.
    retired_tx: spsc::Producer<Retired>,
    sample_voices: [SampleVoice; SAMPLE_VOICES],
    loops: [LoopVoice; LOOP_SLOTS],
    dll: Dll,
    /// Timeline position of the next output frame. The timeline is what the
    /// game clock measures: it runs at `speed`, stops when paused and jumps
    /// on a seek. At speed 1 it advances one frame per output frame.
    pos: f64,
    speed: f64,
    paused: bool,
    next_beat: f64,
    beat_index: u64,
    voices: [Voice; VOICES],
}

impl Mixer {
    pub fn new(
        cfg: MixerConfig,
        cmds: spsc::Consumer<AudioCmd>,
        clock_tx: seqlock::Writer<ClockModel>,
        latency_tx: spsc::Producer<u64>,
        retired_tx: spsc::Producer<Retired>,
    ) -> Self {
        let next_beat = cfg.metronome.map(|g| g.offset_frames).unwrap_or(f64::INFINITY);
        let mut cfg = cfg;
        let music = cfg.music.take().map(StreamReader::attach);
        Mixer {
            cfg,
            music,
            cmds,
            clock_tx,
            latency_tx,
            retired_tx,
            sample_voices: [SampleVoice { sample: 0, at: 0, gain: [0.0; 2], wait: 0, active: false }; SAMPLE_VOICES],
            loops: [LoopVoice { sample: 0, at: 0.0, gain: [0.0; 2], rate: 1.0, active: false }; LOOP_SLOTS],
            dll: Dll::new(SAMPLE_RATE),
            pos: 0.0,
            speed: 1.0,
            paused: false,
            next_beat,
            beat_index: 0,
            voices: [Voice { age: 0, step: 0.0, gain: 0.0, active: false }; VOICES],
        }
    }

    fn start_voice(&mut self, delay_frames: i32, freq: f32, gain: f32) {
        // Steal the oldest voice if all are busy; a dropped tail of a 50 ms
        // tick is inaudible, a missing attack is not.
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| {
            let mut oldest = 0;
            for (i, v) in self.voices.iter().enumerate() {
                if v.age > self.voices[oldest].age {
                    oldest = i;
                }
            }
            oldest
        });
        self.voices[slot] =
            Voice { age: -delay_frames, step: std::f32::consts::TAU * freq / SAMPLE_RATE as f32, gain, active: true };
    }

    /// Fill `out` (interleaved stereo) and publish the clock for this cycle.
    pub fn process(&mut self, out: &mut [f32], cycle: Cycle) {
        let frames = out.len() / CHANNELS;

        // Commands first, so the clock published for this cycle already
        // reflects a seek or a speed change.
        let mut jumped = false;
        // When what is written in this cycle reaches the listener.
        let audible_ns = cycle.now_ns + (cycle.delay_frames.max(0.0) * 1e9 / SAMPLE_RATE as f64) as u64;
        while let Some(cmd) = self.cmds.pop() {
            match cmd {
                AudioCmd::Click { gain } => self.start_voice(0, 1900.0, gain),
                AudioCmd::Play { sample, gain, pan, t_event_ns } => {
                    if (sample as usize) < self.cfg.samples.len() {
                        // Take a free voice, or the one closest to its end.
                        let slot = self.sample_voices.iter().position(|v| !v.active).unwrap_or_else(|| {
                            (0..SAMPLE_VOICES).max_by_key(|&i| self.sample_voices[i].at).unwrap_or(0)
                        });
                        self.sample_voices[slot] =
                            SampleVoice { sample, at: 0, gain: pan_gains(gain, pan), wait: 0, active: true };
                    }
                    if t_event_ns != 0 {
                        let _ = self.latency_tx.push(audible_ns.saturating_sub(t_event_ns));
                    }
                }
                AudioCmd::PlayAt { sample, gain, pan, at } => {
                    if (sample as usize) < self.cfg.samples.len() {
                        let slot = self.sample_voices.iter().position(|v| !v.active).unwrap_or_else(|| {
                            (0..SAMPLE_VOICES).max_by_key(|&i| self.sample_voices[i].at).unwrap_or(0)
                        });
                        // Output frames from the start of this cycle; the
                        // music moves `speed` timeline frames per output frame.
                        let speed = if self.paused { 0.0 } else { self.speed };
                        let wait = if speed > 0.0 { ((at - self.pos) / speed).round().max(0.0) } else { 0.0 };
                        self.sample_voices[slot] = SampleVoice {
                            sample,
                            at: 0,
                            gain: pan_gains(gain, pan),
                            wait: wait.min(u32::MAX as f64) as u32,
                            active: true,
                        };
                    }
                }
                AudioCmd::Loop { slot, sample, gain, pan, rate } => {
                    if let Some(voice) = self.loops.get_mut(slot as usize) {
                        match sample.filter(|s| (*s as usize) < self.cfg.samples.len()) {
                            Some(sample) => {
                                if !voice.active || voice.sample != sample {
                                    voice.at = 0.0;
                                }
                                voice.sample = sample;
                                voice.gain = pan_gains(gain, pan);
                                voice.rate = rate.clamp(0.05, 8.0);
                                voice.active = true;
                            }
                            None => voice.active = false,
                        }
                    }
                }
                AudioCmd::SetMusic { music, start } => {
                    // SAFETY: the sender gave up this reference for us to own.
                    // The old one first: the new one may be the same stream.
                    if let Some(old) = self.music.take() {
                        // If the way back is full the stream is leaked:
                        // better than freeing memory on this thread.
                        let _ = self.retired_tx.push(Retired::Music(Handoff::new(old.detach())));
                    }
                    self.music = music.map(|h| StreamReader::attach(unsafe { h.into_arc() }));
                    self.cfg.music_start = start;
                }
                AudioCmd::SetSamples(table) => {
                    // SAFETY: as above.
                    let old = std::mem::replace(&mut self.cfg.samples, unsafe { table.into_arc() });
                    let _ = self.retired_tx.push(Retired::Samples(Handoff::new(old)));
                    for v in &mut self.sample_voices {
                        v.active = false; // their indices meant the old table
                    }
                    for v in &mut self.loops {
                        v.active = false;
                    }
                }
                AudioCmd::MusicGain(g) => self.cfg.music_gain = g,
                AudioCmd::MetronomeGain(g) => self.cfg.metronome_gain = g,
                AudioCmd::Seek(pos) => {
                    self.pos = pos.max(0.0).round();
                    jumped = true;
                }
                AudioCmd::Speed(speed) => {
                    self.speed = speed.clamp(0.05, 8.0);
                    if self.speed == 1.0 {
                        // Back on whole frames, so music is copied, not interpolated.
                        self.pos = self.pos.round();
                    }
                    jumped = true;
                }
                AudioCmd::Pause(paused) => {
                    self.paused = paused;
                    jumped = true;
                }
            }
        }
        let rate = if self.paused { 0.0 } else { self.speed };
        if jumped {
            // What waits for a moment of the music loses its moment.
            for v in self.sample_voices.iter_mut().filter(|v| v.wait > 0) {
                v.active = false;
            }
            self.dll.restart(SAMPLE_RATE as f64 * rate);
            if let Some(grid) = self.cfg.metronome {
                self.beat_index = ((self.pos - grid.offset_frames) / grid.period_frames).ceil().max(0.0) as u64;
                self.next_beat = grid.offset_frames + self.beat_index as f64 * grid.period_frames;
            }
        }

        // The first sample written in this cycle becomes audible
        // `delay_frames` from `now_ns`, so at `now_ns` the listener hears
        // what was written `delay` output frames ago.
        let delay = if cycle.delay_frames >= 0.0 { cycle.delay_frames } else { frames as f64 };
        let model = self.dll.update(cycle.now_ns, self.pos - delay * rate, delay, frames as u32);
        self.clock_tx.write(model);

        out.fill(0.0);
        if let Some(music) = &mut self.music {
            // The track frames this cycle reads (the timeline runs ahead of
            // the track by the lead-in). Asked for even while paused, so the
            // decoder gets ready for wherever a seek went.
            let start = self.cfg.music_start as f64;
            let first = (self.pos - start).max(0.0).floor() as u64;
            let last = (self.pos + frames as f64 * rate - start).max(0.0).ceil() as u64 + 1;
            let mut w = music.window(first, last);
            let g = self.cfg.music_gain;
            if rate == 1.0 && self.pos.fract() == 0.0 {
                // Normal playback: a straight copy.
                let pos = self.pos as i64 - self.cfg.music_start as i64;
                for (i, frame) in out.chunks_exact_mut(CHANNELS).enumerate() {
                    let p = pos + i as i64;
                    if p >= 0 {
                        let [l, r] = w.frame(p as u64);
                        frame[0] = l * g;
                        frame[1] = r * g;
                    }
                }
            } else if rate > 0.0 {
                // Another speed: read the track at a fractional step.
                // Linear interpolation is rough but cheap; pitch follows.
                for (i, frame) in out.chunks_exact_mut(CHANNELS).enumerate() {
                    let p = self.pos + i as f64 * rate - start;
                    if p < 0.0 {
                        continue;
                    }
                    let (k, f) = (p as u64, p.fract() as f32);
                    let (a, b) = (w.frame(k), w.frame(k + 1));
                    frame[0] = (a[0] + (b[0] - a[0]) * f) * g;
                    frame[1] = (a[1] + (b[1] - a[1]) * f) * g;
                }
            }
            if w.missing && rate > 0.0 {
                music.underrun();
            }
        }

        // Metronome ticks that fall inside this buffer start on their exact frame.
        if let (Some(grid), true) = (self.cfg.metronome, rate > 0.0) {
            let end = self.pos + frames as f64 * rate;
            while self.next_beat < end {
                let at = ((self.next_beat - self.pos) / rate).max(0.0) as i32;
                let accent = self.beat_index % 4 == 0;
                let (freq, gain) = if accent { (1320.0, 1.0) } else { (880.0, 0.7) };
                self.start_voice(at, freq, gain * self.cfg.metronome_gain);
                self.beat_index += 1;
                self.next_beat = grid.offset_frames + self.beat_index as f64 * grid.period_frames;
            }
        }

        // Hit sounds.
        for v in self.sample_voices.iter_mut().filter(|v| v.active) {
            let data = &self.cfg.samples[v.sample as usize];
            // A voice that waits for its frame starts part-way into the
            // buffer, or in a later one.
            let skip = (v.wait as usize).min(frames);
            v.wait -= skip as u32;
            if v.wait > 0 {
                continue;
            }
            let room = out.len() - skip * CHANNELS;
            let from = v.at as usize * CHANNELS;
            let n = data.len().saturating_sub(from).min(room);
            let out = &mut out[skip * CHANNELS..];
            for (o, s) in out[..n].chunks_exact_mut(CHANNELS).zip(data[from..from + n].chunks_exact(CHANNELS)) {
                o[0] += s[0] * v.gain[0];
                o[1] += s[1] * v.gain[1];
            }
            v.at += (n / CHANNELS) as u32;
            v.active = from + n < data.len();
        }

        // Looping sounds, read at their own rate and wrapped at the end.
        for v in self.loops.iter_mut().filter(|v| v.active) {
            let data = &self.cfg.samples[v.sample as usize];
            let len = data.len() / CHANNELS;
            if len < 2 {
                v.active = false;
                continue;
            }
            for o in out.chunks_exact_mut(CHANNELS) {
                let (k, f) = (v.at as usize % len, v.at.fract() as f32);
                let next = (k + 1) % len;
                for (c, o) in o.iter_mut().enumerate() {
                    let (a, b) = (data[k * CHANNELS + c], data[next * CHANNELS + c]);
                    *o += (a + (b - a) * f) * v.gain[c];
                }
                v.at += v.rate as f64;
                if v.at >= len as f64 {
                    v.at -= len as f64;
                }
            }
        }

        // Voices: decaying sine ticks.
        for v in self.voices.iter_mut().filter(|v| v.active) {
            for f in 0..frames {
                if v.age >= 0 {
                    let a = v.age as f32;
                    let s = (a * v.step).sin() * (-a * VOICE_DECAY).exp() * v.gain;
                    out[f * CHANNELS] += s;
                    out[f * CHANNELS + 1] += s;
                }
                v.age += 1;
                if v.age >= VOICE_LEN {
                    v.active = false;
                    break;
                }
            }
        }

        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        self.pos += frames as f64 * rate;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(samples: Vec<f32>) -> Arc<Stream> {
        Stream::preloaded(&samples)
    }

    fn mixer(cfg: MixerConfig) -> (Mixer, spsc::Producer<AudioCmd>, seqlock::Reader<ClockModel>) {
        let (tx, rx) = spsc::channel(64);
        let (cw, cr) = seqlock::channel(ClockModel::INVALID);
        let (latency_tx, _) = spsc::channel(64);
        let (retired_tx, _) = spsc::channel(64);
        (Mixer::new(cfg, rx, cw, latency_tx, retired_tx), tx, cr)
    }

    #[test]
    fn metronome_ticks_start_on_the_exact_frame() {
        let grid = BeatGrid { period_frames: 100.0, offset_frames: 150.0 };
        let (mut m, _tx, _clock) = mixer(MixerConfig {
            music: None,
            music_start: 0,
            music_gain: 1.0,
            metronome: Some(grid),
            metronome_gain: 1.0,
            samples: Arc::new([]),
        });
        let mut all = Vec::new();
        let mut buf = [0.0f32; 64 * CHANNELS];
        for i in 0..6 {
            m.process(&mut buf, Cycle { now_ns: 1_000_000 + i * 1_333_333, delay_frames: 0.0 });
            all.extend(buf.iter().step_by(CHANNELS).copied());
        }
        // sin(0) == 0 on the start frame itself, so the first non-zero
        // sample is the frame after each beat.
        let first = all.iter().position(|s| *s != 0.0).unwrap();
        assert_eq!(first, 151);
        assert!(all[..151].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn music_starts_exactly_after_the_lead_in() {
        // A ramp makes every sample identify its own index.
        let music = track((0..400).flat_map(|i| [i as f32 / 1000.0; 2]).collect());
        let (mut m, _tx, _clock) = mixer(MixerConfig {
            music: Some(music),
            music_start: 100, // inside the second 64-frame buffer
            music_gain: 1.0,
            metronome: None,
            metronome_gain: 0.0,
            samples: Arc::new([]),
        });
        let mut all = Vec::new();
        let mut buf = [0.0f32; 64 * CHANNELS];
        for i in 0..10 {
            m.process(&mut buf, Cycle { now_ns: 1_000_000 + i * 1_333_333, delay_frames: 0.0 });
            all.extend(buf.iter().step_by(CHANNELS).copied());
        }
        assert!(all[..101].iter().all(|s| *s == 0.0), "silence through the lead-in (sample 0 is 0.0)");
        for (i, s) in all[100..500].iter().enumerate() {
            assert_eq!(*s, i as f32 / 1000.0, "music frame {i}");
        }
        assert!(all[500..].iter().all(|s| *s == 0.0), "silence after the track ends");
    }

    /// A ramp track: every sample's value is its own index in millionths
    /// (small enough to stay clear of the output limiter).
    fn ramp_mixer(start: u64) -> (Mixer, spsc::Producer<AudioCmd>, seqlock::Reader<ClockModel>) {
        let music = track((0..100_000).flat_map(|i| [i as f32 / 1e6; 2]).collect());
        mixer(MixerConfig {
            music: Some(music),
            music_start: start,
            music_gain: 1.0,
            metronome: None,
            metronome_gain: 0.0,
            samples: Arc::new([]),
        })
    }

    fn run(m: &mut Mixer, t: &mut u64, cycles: usize) -> Vec<f32> {
        let mut all = Vec::new();
        let mut buf = [0.0f32; 64 * CHANNELS];
        for _ in 0..cycles {
            m.process(&mut buf, Cycle { now_ns: *t, delay_frames: 0.0 });
            *t += 1_333_333;
            all.extend(buf.iter().step_by(CHANNELS).copied());
        }
        all
    }

    #[test]
    fn seek_jumps_the_track_and_the_clock_together() {
        let (mut m, mut tx, clock) = ramp_mixer(1000);
        let mut t = 1_000_000_000;
        run(&mut m, &mut t, 10);
        let before = clock.read().epoch;
        tx.push(AudioCmd::Seek(50_000.0)).unwrap();
        let out = run(&mut m, &mut t, 1);
        // Timeline 50_000 is track frame 49_000.
        assert_eq!(out[0], 49_000.0 / 1e6);
        assert_eq!(out[63], 49_063.0 / 1e6);
        let c = clock.read();
        assert_eq!(c.epoch, before + 1, "a seek starts a new clock epoch");
        assert_eq!(c.pos_at(t - 1_333_333), 50_000.0);
        // Seeking back into the lead-in gives silence again.
        tx.push(AudioCmd::Seek(0.0)).unwrap();
        assert!(run(&mut m, &mut t, 1).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn speed_changes_how_fast_the_track_and_the_clock_run() {
        let (mut m, mut tx, clock) = ramp_mixer(0);
        let mut t = 1_000_000_000;
        tx.push(AudioCmd::Speed(0.5)).unwrap();
        let out = run(&mut m, &mut t, 100);
        // Half speed: output frame i plays track position i / 2.
        assert_eq!(out[0], 0.0);
        assert!((out[1] - 0.5e-6).abs() < 1e-9, "interpolated between samples");
        assert!((out[1000] - 500e-6).abs() < 1e-9);
        let c = clock.read();
        assert!((c.rate * 1e9 - 24_000.0).abs() < 50.0, "clock runs at {} frames/s", c.rate * 1e9);
        assert!((c.pos_at(t - 1_333_333) - 99.0 * 32.0).abs() < 2.0);
        // Back to normal: whole frames again, copied exactly.
        tx.push(AudioCmd::Speed(1.0)).unwrap();
        let out = run(&mut m, &mut t, 1);
        assert_eq!(out[0], 3200.0 / 1e6, "exactly a track sample");
        assert_eq!(out[1], 3201.0 / 1e6);
    }

    #[test]
    fn pause_stops_the_timeline() {
        let (mut m, mut tx, clock) = ramp_mixer(0);
        let mut t = 1_000_000_000;
        run(&mut m, &mut t, 10);
        tx.push(AudioCmd::Pause(true)).unwrap();
        let out = run(&mut m, &mut t, 20);
        assert!(out.iter().all(|s| *s == 0.0));
        let c = clock.read();
        assert_eq!(c.pos_at(t), 640.0);
        assert_eq!(c.pos_at(t + 1_000_000_000), 640.0, "time passes, the position does not");
        tx.push(AudioCmd::Pause(false)).unwrap();
        let out = run(&mut m, &mut t, 1);
        assert_eq!(out[0], 640.0 / 1e6, "resumes exactly where it stopped");
    }

    #[test]
    fn samples_play_from_the_next_cycle_and_report_their_latency() {
        // Two samples: a 100-frame ramp and a 10-frame block.
        let ramp: Arc<[f32]> = (0..100).flat_map(|i| [i as f32 / 1000.0; 2]).collect();
        let block: Arc<[f32]> = vec![0.25f32; 20].into();
        let (tx, rx) = spsc::channel(64);
        let (cw, _cr) = seqlock::channel(ClockModel::INVALID);
        let (latency_tx, mut latency_rx) = spsc::channel(64);
        let mut tx: spsc::Producer<AudioCmd> = tx;
        let mut m = Mixer::new(
            MixerConfig {
                music: None,
                music_start: 0,
                music_gain: 1.0,
                metronome: None,
                metronome_gain: 0.0,
                samples: Arc::new([ramp, block]),
            },
            rx,
            cw,
            latency_tx,
            spsc::channel(8).0,
        );
        let mut buf = [0.0f32; 64 * CHANNELS];
        let now = 5_000_000_000u64;
        // A key pressed 400 us before the cycle; the device adds 96 frames (2 ms).
        tx.push(AudioCmd::Play { sample: 0, gain: 0.5, pan: 0.0, t_event_ns: now - 400_000 }).unwrap();
        tx.push(AudioCmd::Play { sample: 1, gain: 1.0, pan: 0.0, t_event_ns: 0 }).unwrap();
        tx.push(AudioCmd::Play { sample: 9, gain: 1.0, pan: 0.0, t_event_ns: 0 }).unwrap(); // not in the table
        m.process(&mut buf, Cycle { now_ns: now, delay_frames: 96.0 });
        // Frame 5: ramp value 0.005 at half gain plus the block.
        assert!((buf[5 * CHANNELS] - (0.0025 + 0.25)).abs() < 1e-6);
        assert!((buf[20 * CHANNELS] - 0.010).abs() < 1e-6, "the block has ended");
        // The ramp continues across the buffer boundary and then stops.
        m.process(&mut buf, Cycle { now_ns: now + 1_333_333, delay_frames: 96.0 });
        assert!((buf[0] - 0.032).abs() < 1e-6);
        assert!((buf[35 * CHANNELS] - 0.0495).abs() < 1e-6);
        assert_eq!(buf[36 * CHANNELS], 0.0);
        // Only the measured sound reports: 0.4 ms of waiting + 2 ms of device.
        assert_eq!(latency_rx.pop(), Some(2_400_000));
        assert_eq!(latency_rx.pop(), None);
    }

    #[test]
    fn a_sample_placed_on_the_music_clock_starts_on_its_exact_frame() {
        let block: Arc<[f32]> = vec![0.5f32; 2 * 8].into(); // 8 frames
        let (tx, rx) = spsc::channel(64);
        let (cw, _cr) = seqlock::channel(ClockModel::INVALID);
        let mut tx: spsc::Producer<AudioCmd> = tx;
        let mut m = Mixer::new(
            MixerConfig {
                music: None,
                music_start: 0,
                music_gain: 1.0,
                metronome: None,
                metronome_gain: 0.0,
                samples: Arc::new([block]),
            },
            rx,
            cw,
            spsc::channel(8).0,
            spsc::channel(8).0,
        );
        let mut buf = [0.0f32; 64 * CHANNELS];
        let cycle = |n: u64| Cycle { now_ns: 1_000_000_000 + n * 1_333_333, delay_frames: 96.0 };
        // The timeline is at 0; the sample is for frame 70: in the next cycle, at frame 6.
        tx.push(AudioCmd::PlayAt { sample: 0, gain: 1.0, pan: 0.0, at: 70.0 }).unwrap();
        m.process(&mut buf, cycle(0));
        assert!(buf.iter().all(|s| *s == 0.0), "not yet");
        m.process(&mut buf, cycle(1));
        assert_eq!(buf[5 * CHANNELS], 0.0);
        assert_eq!(buf[6 * CHANNELS], 0.5, "on the exact frame");
        assert_eq!(buf[13 * CHANNELS], 0.5);
        assert_eq!(buf[14 * CHANNELS], 0.0, "and for the sample's length only");
        // A moment already past starts at once.
        tx.push(AudioCmd::PlayAt { sample: 0, gain: 1.0, pan: 0.0, at: 5.0 }).unwrap();
        m.process(&mut buf, cycle(2));
        assert_eq!(buf[0], 0.5);
        // A seek takes back what was waiting for a moment of the old music.
        tx.push(AudioCmd::PlayAt { sample: 0, gain: 1.0, pan: 0.0, at: 400.0 }).unwrap();
        tx.push(AudioCmd::Seek(1000.0)).unwrap();
        m.process(&mut buf, cycle(3));
        m.process(&mut buf, cycle(4));
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn pan_turns_down_the_far_channel_and_loops_wrap_until_stopped() {
        let block: Arc<[f32]> = vec![0.5f32; 8].into();
        // Left channel counts 0, 1, 2; right stays at 1.
        let cycle: Arc<[f32]> = vec![0.0, 1.0, 0.1, 1.0, 0.2, 1.0].into();
        let (mut tx, rx) = spsc::channel::<AudioCmd>(64);
        let (cw, _cr) = seqlock::channel(ClockModel::INVALID);
        let mut m = Mixer::new(
            MixerConfig {
                music: None,
                music_start: 0,
                music_gain: 1.0,
                metronome: None,
                metronome_gain: 0.0,
                samples: Arc::new([block, cycle]),
            },
            rx,
            cw,
            spsc::channel(8).0,
            spsc::channel(8).0,
        );
        let mut buf = [0.0f32; 8 * CHANNELS];
        let at = Cycle { now_ns: 1_000_000_000, delay_frames: 0.0 };
        tx.push(AudioCmd::Play { sample: 0, gain: 1.0, pan: 0.25, t_event_ns: 0 }).unwrap();
        m.process(&mut buf, at);
        assert_eq!((buf[0], buf[1]), (0.375, 0.5), "panned right: the left channel gives way");
        assert_eq!((buf[8], buf[9]), (0.0, 0.0));

        tx.push(AudioCmd::Loop { slot: 0, sample: Some(1), gain: 0.5, pan: -1.0, rate: 1.0 }).unwrap();
        m.process(&mut buf, at);
        let left: Vec<f32> = buf.chunks_exact(2).map(|f| f[0]).collect();
        assert_eq!(left, [0.0, 0.05, 0.1, 0.0, 0.05, 0.1, 0.0, 0.05], "it wraps");
        assert!(buf.chunks_exact(2).all(|f| f[1] == 0.0), "hard left");
        // The same sample again keeps its place and takes the new settings.
        tx.push(AudioCmd::Loop { slot: 0, sample: Some(1), gain: 1.0, pan: 0.0, rate: 0.5 }).unwrap();
        m.process(&mut buf, at);
        assert_eq!((buf[0], buf[1]), (0.2, 1.0), "continues where it was");
        assert!((buf[2] - 0.1).abs() < 1e-6, "half speed: halfway from 0.2 back to 0.0");
        tx.push(AudioCmd::Loop { slot: 0, sample: None, gain: 0.0, pan: 0.0, rate: 1.0 }).unwrap();
        m.process(&mut buf, at);
        assert!(buf.iter().all(|s| *s == 0.0), "stopped at once");
    }

    #[test]
    fn music_and_samples_can_be_replaced_while_running_without_freeing_on_this_thread() {
        let first = track(vec![0.1f32; 2000]);
        let second = track(vec![0.2f32; 2000]);
        let table: Arc<[Arc<[f32]>]> = Arc::new([vec![0.5f32; 400].into()]);
        let (mut tx, rx) = spsc::channel::<AudioCmd>(64);
        let (cw, _cr) = seqlock::channel(ClockModel::INVALID);
        let (retired_tx, mut retired_rx) = spsc::channel(8);
        let mut m = Mixer::new(
            MixerConfig {
                music: Some(first.clone()),
                music_start: 0,
                music_gain: 1.0,
                metronome: None,
                metronome_gain: 0.0,
                samples: Arc::new([]),
            },
            rx,
            cw,
            spsc::channel(8).0,
            retired_tx,
        );
        let mut buf = [0.0f32; 64 * CHANNELS];
        let mut t = 1_000_000_000u64;
        let mut cycle = |m: &mut Mixer, buf: &mut [f32]| {
            m.process(buf, Cycle { now_ns: t, delay_frames: 0.0 });
            t += 1_333_333;
        };
        cycle(&mut m, &mut buf);
        assert_eq!(buf[0], 0.1);

        tx.push(AudioCmd::SetMusic { music: Some(Handoff::new(second.clone())), start: 0 }).unwrap();
        tx.push(AudioCmd::SetSamples(Handoff::new(table.clone()))).unwrap();
        tx.push(AudioCmd::Seek(0.0)).unwrap();
        tx.push(AudioCmd::Play { sample: 0, gain: 1.0, pan: 0.0, t_event_ns: 0 }).unwrap();
        cycle(&mut m, &mut buf);
        assert!((buf[0] - 0.7).abs() < 1e-6, "new track plus the new table's sample");
        // The old track is still alive: the mixer's reference came back
        // instead of being dropped.
        assert_eq!(Arc::strong_count(&first), 2);
        let mut returned = 0;
        while let Some(r) = retired_rx.pop() {
            r.release();
            returned += 1;
        }
        assert_eq!(returned, 2, "old track and old (empty) table");
        assert_eq!(Arc::strong_count(&first), 1);

        // Removing the track gives silence and returns it too.
        tx.push(AudioCmd::SetMusic { music: None, start: 0 }).unwrap();
        cycle(&mut m, &mut buf);
        cycle(&mut m, &mut buf);
        assert_eq!(Arc::strong_count(&second), 2);
        retired_rx.pop().unwrap().release();
        assert_eq!(Arc::strong_count(&second), 1);
        assert_eq!(Arc::strong_count(&table), 2, "the table in use is held once by the mixer");
    }

    #[test]
    fn clock_reports_position_minus_delay() {
        let (mut m, _tx, clock) = mixer(MixerConfig {
            music: None,
            music_start: 0,
            music_gain: 1.0,
            metronome: None,
            metronome_gain: 0.0,
            samples: Arc::new([]),
        });
        let mut buf = [0.0f32; 64 * CHANNELS];
        let mut t = 5_000_000_000u64;
        for _ in 0..3000 {
            m.process(&mut buf, Cycle { now_ns: t, delay_frames: 192.0 });
            t += 1_333_333;
        }
        let c = clock.read();
        // After the last call `written` is 3000*64 and that call's cycle
        // started 64 frames earlier, 192 frames ahead of the listener.
        let expect = (3000.0 - 1.0) * 64.0 - 192.0;
        assert!((c.pos_at(t - 1_333_333) - expect).abs() < 0.5, "{}", c.pos_at(t - 1_333_333));
        assert_eq!(c.quantum, 64);
    }
}
