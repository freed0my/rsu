//! The game's hold on time and sound: commands to the audio thread, and
//! the clock that comes back. Without an audio device it keeps the same
//! kind of clock itself, on the system clock.

use crate::play::{LoopState, SoundIds};
use std::sync::Arc;
use rsu_audio::clock::ClockModel;
use rsu_audio::handoff::{Handoff, Retired};
use rsu_audio::mixer::AudioCmd;
use rsu_audio::stream::Stream;
use rsu_audio::SAMPLE_RATE;
use rsu_rt::{seqlock, spsc};

pub struct Transport {
    cmds: spsc::Producer<AudioCmd>,
    clock: seqlock::Reader<ClockModel>,
    retired: spsc::Consumer<Retired>,
    audio: bool,
    free: ClockModel,
    /// The clock epoch at the time of the last jump we asked for. Until the
    /// audio thread has carried the jump out (which starts a new epoch) the
    /// clock still describes the old position and must not be believed.
    stale_epoch: Option<u32>,
    /// The looping sounds as the audio thread was last told.
    loops: [Option<LoopState>; 2],
    /// The track the mixer was last given.
    music: Option<Arc<Stream>>,
    pub speed: f64,
    pub paused: bool,
}

impl Transport {
    pub fn new(
        cmds: spsc::Producer<AudioCmd>,
        clock: seqlock::Reader<ClockModel>,
        retired: spsc::Consumer<Retired>,
        audio: bool,
        now_ns: u64,
    ) -> Transport {
        Transport {
            cmds,
            clock,
            retired,
            audio,
            loops: [None; 2],
            music: None,
            free: ClockModel {
                t0_ns: now_ns,
                pos0: 0.0,
                rate: SAMPLE_RATE as f64 / 1e9,
                epoch: 1,
                valid: true,
                ..ClockModel::INVALID
            },
            stale_epoch: None,
            speed: 1.0,
            paused: false,
        }
    }

    pub fn has_audio(&self) -> bool {
        self.audio
    }

    /// The current clock. Invalid while a seek, speed change or pause is
    /// still on its way to the audio thread: a play that just started must
    /// not see the position the song preview was at.
    pub fn clock(&self) -> ClockModel {
        if !self.audio {
            return self.free;
        }
        let model = self.clock.read();
        if self.stale_epoch == Some(model.epoch) {
            return ClockModel { valid: false, ..model };
        }
        model
    }

    fn jump(&mut self, cmd: AudioCmd) {
        if self.audio && self.cmds.push(cmd).is_ok() {
            self.stale_epoch = Some(self.clock.read().epoch);
        }
    }

    fn refit_free(&mut self, now_ns: u64, pos: f64) {
        let rate = if self.paused { 0.0 } else { self.speed };
        self.free = ClockModel {
            t0_ns: now_ns,
            pos0: pos,
            rate: SAMPLE_RATE as f64 * rate / 1e9,
            epoch: self.free.epoch.wrapping_add(1),
            valid: true,
            ..ClockModel::INVALID
        };
    }

    /// Jump to a timeline position (frames). With audio the clock follows
    /// within one audio cycle; whoever plays back follows the clock.
    pub fn seek(&mut self, pos: f64, now_ns: u64) {
        self.jump(AudioCmd::Seek(pos));
        if !self.audio {
            self.refit_free(now_ns, pos);
        }
    }

    pub fn set_speed(&mut self, speed: f64, now_ns: u64) {
        let pos = self.free.pos_at(now_ns);
        self.speed = speed;
        self.jump(AudioCmd::Speed(speed));
        if !self.audio {
            self.refit_free(now_ns, pos);
        }
    }

    pub fn set_paused(&mut self, paused: bool, now_ns: u64) {
        let pos = self.free.pos_at(now_ns);
        self.paused = paused;
        self.jump(AudioCmd::Pause(paused));
        if !self.audio {
            self.refit_free(now_ns, pos);
        }
    }

    /// Replace what the mixer plays: the track (starting at timeline frame
    /// `music_start`) and, if given, the hit sound table. Follow with a
    /// seek to say where on the timeline to continue.
    pub fn load(&mut self, music: Option<Arc<Stream>>, music_start: u64, samples: Option<Arc<[Arc<[f32]>]>>) {
        if !self.audio {
            return; // nobody would ever take the buffers back out of the queue
        }
        self.music = music.clone();
        if let Some(table) = samples {
            self.loops = [None; 2]; // a new table silences them
            let handoff = Handoff::new(table);
            if self.cmds.push(AudioCmd::SetSamples(handoff)).is_err() {
                // SAFETY: the command was not sent, so the reference is still ours.
                drop(unsafe { handoff.into_arc() });
            }
        }
        let handoff = music.map(Handoff::new);
        if self.cmds.push(AudioCmd::SetMusic { music: handoff, start: music_start }).is_err() {
            if let Some(h) = handoff {
                // SAFETY: as above.
                drop(unsafe { h.into_arc() });
            }
        }
    }

    /// The track the mixer plays (or was last asked to).
    pub fn music(&self) -> Option<&Arc<Stream>> {
        self.music.as_ref()
    }

    /// Replace the table of samples, leaving the track alone.
    pub fn set_samples(&mut self, table: Arc<[Arc<[f32]>]>) {
        if !self.audio {
            return;
        }
        self.loops = [None; 2];
        let handoff = Handoff::new(table);
        if self.cmds.push(AudioCmd::SetSamples(handoff)).is_err() {
            // SAFETY: the command was not sent, so the reference is still ours.
            drop(unsafe { handoff.into_arc() });
        }
    }

    /// How loud the track is, 0..1.
    pub fn set_music_gain(&mut self, gain: f32) {
        let _ = self.cmds.push(AudioCmd::MusicGain(gain));
    }

    /// Play a hit sound: every layer at the sound's own volume times
    /// `gain`. `t_event_ns` is the key press that caused it, if one did;
    /// the audio thread reports back when that sound becomes audible.
    pub fn play(&mut self, sound: &SoundIds, gain: f32, t_event_ns: u64) {
        for (n, &sample) in sound.ids[..sound.count as usize].iter().enumerate() {
            let _ = self.cmds.push(AudioCmd::Play {
                sample,
                gain: gain * sound.volume as f32 / 100.0 * sound.levels[n] as f32 / 100.0,
                pan: sound.pan as f32 / 100.0,
                t_event_ns: if n == 0 { t_event_ns } else { 0 },
            });
        }
    }

    /// Like [`Transport::play`], but the sound starts when the music
    /// reaches the timeline position `at` (in frames), to the exact output
    /// frame, however late in the frame this was called.
    pub fn play_at(&mut self, sound: &SoundIds, gain: f32, at: f64) {
        for (n, &sample) in sound.ids[..sound.count as usize].iter().enumerate() {
            let _ = self.cmds.push(AudioCmd::PlayAt {
                sample,
                gain: gain * sound.volume as f32 / 100.0 * sound.levels[n] as f32 / 100.0,
                pan: sound.pan as f32 / 100.0,
                at,
            });
        }
    }

    /// Make the looping sounds what `wanted` says, telling the audio thread
    /// only about what changed enough to be heard.
    pub fn set_loops(&mut self, wanted: &[Option<LoopState>; 2], gain: f32) {
        for (slot, (want, sent)) in wanted.iter().zip(&mut self.loops).enumerate() {
            let same = match (want, &*sent) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.sample == b.sample
                        && a.volume == b.volume
                        && (a.pan as i32 - b.pan as i32).abs() < 3
                        && (a.rate - b.rate).abs() < 0.01
                }
                _ => false,
            };
            if same {
                continue;
            }
            let cmd = match want {
                Some(l) => AudioCmd::Loop {
                    slot: slot as u8,
                    sample: Some(l.sample),
                    gain: gain * l.volume as f32 / 100.0,
                    pan: l.pan as f32 / 100.0,
                    rate: l.rate,
                },
                None => AudioCmd::Loop { slot: slot as u8, sample: None, gain: 0.0, pan: 0.0, rate: 1.0 },
            };
            if self.cmds.push(cmd).is_ok() {
                *sent = *want;
            }
        }
    }

    /// Free the buffers the mixer has stopped using. This releases memory,
    /// so it belongs outside the frame's hot section.
    pub fn collect_retired(&mut self) {
        while let Some(buffer) = self.retired.pop() {
            buffer.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsu_audio::mixer::{Cycle, Mixer, MixerConfig};

    /// A transport wired to a real mixer that the test drives by hand.
    fn rig() -> (Transport, Mixer) {
        let (cmd_tx, cmd_rx) = spsc::channel(64);
        let (clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
        let (retired_tx, retired_rx) = spsc::channel(16);
        let mixer = Mixer::new(
            MixerConfig {
                music: None,
                music_start: 0,
                music_gain: 1.0,
                metronome: None,
                metronome_gain: 0.0,
                samples: Arc::new([]),
            },
            cmd_rx,
            clock_tx,
            spsc::channel(16).0,
            retired_tx,
        );
        (Transport::new(cmd_tx, clock_rx, retired_rx, true, 0), mixer)
    }

    fn cycles(mixer: &mut Mixer, t: &mut u64, n: usize) {
        let mut buf = [0.0f32; 128];
        for _ in 0..n {
            mixer.process(&mut buf, Cycle { now_ns: *t, delay_frames: 0.0 });
            *t += 1_333_333;
        }
    }

    #[test]
    fn the_clock_is_not_believed_until_a_jump_has_happened() {
        let (mut transport, mut mixer) = rig();
        let mut t = 1_000_000_000u64;
        assert!(!transport.clock().valid, "no audio cycle yet");
        // A song preview has been playing for a while at 60 s.
        transport.seek(60.0 * 48_000.0, t);
        cycles(&mut mixer, &mut t, 100);
        let preview = transport.clock();
        assert!(preview.valid && preview.pos_at(t) > 60.0 * 48_000.0);

        // A play starts: three commands, then the frame loop reads the clock
        // before the audio thread has run again.
        transport.set_paused(false, t);
        transport.set_speed(1.5, t);
        transport.seek(0.0, t);
        assert!(!transport.clock().valid, "still the preview's position: must not be used");
        cycles(&mut mixer, &mut t, 1);
        let fresh = transport.clock();
        assert!(fresh.valid);
        assert!(fresh.pos_at(t) < 200.0, "the play starts at the beginning, got {}", fresh.pos_at(t));
        assert!((fresh.rate * 1e9 - 72_000.0).abs() < 1.0, "and at the new speed");
    }

    #[test]
    fn a_jump_split_across_two_audio_cycles_is_still_safe() {
        let (mut transport, mut mixer) = rig();
        let mut t = 1_000_000_000u64;
        transport.seek(60.0 * 48_000.0, t);
        cycles(&mut mixer, &mut t, 50);
        // The audio thread runs between two of the commands.
        transport.set_speed(1.0, t);
        cycles(&mut mixer, &mut t, 1);
        transport.seek(0.0, t);
        assert!(!transport.clock().valid, "the seek itself has not happened yet");
        cycles(&mut mixer, &mut t, 1);
        assert!(transport.clock().valid && transport.clock().pos_at(t) < 200.0);
    }

    #[test]
    fn without_audio_the_clock_follows_at_once() {
        let (cmd_tx, _cmd_rx) = spsc::channel(8);
        let (_clock_tx, clock_rx) = seqlock::channel(ClockModel::INVALID);
        let (_retired_tx, retired_rx) = spsc::channel(8);
        let mut transport = Transport::new(cmd_tx, clock_rx, retired_rx, false, 1_000);
        transport.seek(48_000.0, 2_000);
        assert_eq!(transport.clock().pos_at(2_000), 48_000.0);
        transport.set_speed(2.0, 2_000);
        assert_eq!(transport.clock().pos_at(1_000_002_000), 48_000.0 + 96_000.0);
        transport.set_paused(true, 1_000_002_000);
        assert_eq!(transport.clock().pos_at(9_000_000_000), 144_000.0);
        // Buffers are not handed to a mixer that does not exist.
        transport.load(Some(Stream::preloaded(&[0.0; 8])), 0, None);
    }
}
