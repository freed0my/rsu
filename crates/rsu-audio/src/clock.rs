//! Master clock.
//!
//! The device reports "what is audible now" only once per cycle (every
//! 1.3 ms at a 64-frame quantum) and with some jitter. The game needs a
//! position for arbitrary instants: frame times and input timestamps. So the
//! audio thread fits a line `pos(t) = pos0 + rate * (t - t0)` to the reported
//! points with a delay-locked loop and publishes it. Audio stays the master
//! (the line follows the device, including its drift against the system
//! clock); CLOCK_MONOTONIC only provides resolution between cycles.

/// A published snapshot of the fitted line. `Copy`, read through a seqlock.
#[derive(Clone, Copy, Debug)]
pub struct ClockModel {
    /// CLOCK_MONOTONIC time of the cycle this model was updated at.
    pub t0_ns: u64,
    /// Stream position (frames) audible at `t0_ns`.
    pub pos0: f64,
    /// Frames per nanosecond.
    pub rate: f64,
    /// Bumped on every discontinuity (start, seek, xrun-sized jump).
    pub epoch: u32,
    /// Last prediction error in nanoseconds, before correction.
    pub resid_ns: f32,
    /// Output delay the device reported for this cycle, in frames.
    pub delay_frames: f32,
    /// Frames in the last cycle (the quantum actually in use).
    pub quantum: u32,
    pub valid: bool,
}

impl ClockModel {
    pub const INVALID: ClockModel = ClockModel {
        t0_ns: 0,
        pos0: 0.0,
        rate: 0.0,
        epoch: 0,
        resid_ns: 0.0,
        delay_frames: 0.0,
        quantum: 0,
        valid: false,
    };

    /// Audible stream position, in frames, at CLOCK_MONOTONIC time `t_ns`.
    /// Works for times slightly in the past (input events) and future
    /// (the expected display time of the frame being built).
    #[inline]
    pub fn pos_at(&self, t_ns: u64) -> f64 {
        self.pos0 + (t_ns as i64 - self.t0_ns as i64) as f64 * self.rate
    }
}

/// Second-order DLL. Wide loop while acquiring, narrow once locked.
pub struct Dll {
    model: ClockModel,
    nominal_rate: f64,
    cycles: u64,
}

impl Dll {
    /// A jump larger than this is a discontinuity, not jitter.
    const RESET_NS: f64 = 20_000_000.0;
    const ACQUIRE_CYCLES: u64 = 256;

    pub fn new(sample_rate: u32) -> Self {
        Dll { model: ClockModel::INVALID, nominal_rate: sample_rate as f64 / 1e9, cycles: 0 }
    }

    /// The position is about to jump or change speed (seek, pause, playback
    /// rate): start over at `frames_per_second` instead of slewing towards
    /// the new line. The next update begins a new epoch.
    pub fn restart(&mut self, frames_per_second: f64) {
        self.nominal_rate = frames_per_second / 1e9;
        self.model.valid = false;
    }

    /// Feed one observation: at `now_ns` the audible position was `heard`.
    pub fn update(&mut self, now_ns: u64, heard: f64, delay_frames: f64, quantum: u32) -> ClockModel {
        let m = &mut self.model;
        m.delay_frames = delay_frames as f32;
        m.quantum = quantum;
        if !m.valid || now_ns <= m.t0_ns {
            self.reset(now_ns, heard);
            return self.model;
        }
        if self.nominal_rate == 0.0 {
            // Paused: the position stands still and there is nothing to fit.
            (m.t0_ns, m.pos0, m.rate, m.resid_ns) = (now_ns, heard, 0.0, 0.0);
            return self.model;
        }
        let dt = (now_ns - m.t0_ns) as f64;
        let predicted = m.pos0 + dt * m.rate;
        let err = heard - predicted;
        let err_ns = err / self.nominal_rate;
        if err_ns.abs() > Self::RESET_NS {
            self.reset(now_ns, heard);
            return self.model;
        }
        // Loop bandwidth in Hz; gains follow the usual critically damped
        // form with w = 2*pi*B*T, T being the time since the last update.
        let bw = if self.cycles < Self::ACQUIRE_CYCLES { 20.0 } else { 1.0 };
        let w = (std::f64::consts::TAU * bw * dt / 1e9).min(0.5);
        let k1 = std::f64::consts::SQRT_2 * w;
        let k2 = w * w;
        m.pos0 = predicted + k1 * err;
        m.rate = (m.rate + k2 * err / dt).clamp(self.nominal_rate * 0.995, self.nominal_rate * 1.005);
        m.t0_ns = now_ns;
        m.resid_ns = err_ns as f32;
        self.cycles += 1;
        self.model
    }

    fn reset(&mut self, now_ns: u64, heard: f64) {
        let m = &mut self.model;
        m.t0_ns = now_ns;
        m.pos0 = heard;
        m.rate = self.nominal_rate;
        m.epoch = m.epoch.wrapping_add(1);
        m.resid_ns = 0.0;
        m.valid = true;
        self.cycles = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise in -1.0..1.0.
    fn noise(state: &mut u64) -> f64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*state >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
    }

    #[test]
    fn locks_to_a_drifting_jittery_device() {
        // Device runs 150 ppm fast; cycles arrive every 64 frames with
        // +-100 us of wake-up jitter and +-8 frames of delay-report noise.
        let true_rate = 48_000.0 * (1.0 + 150e-6) / 1e9;
        let mut dll = Dll::new(48_000);
        let mut seed = 1u64;
        let mut worst_late = 0.0f64;
        let cycles = 20_000u64; // ~27 s
        for i in 0..cycles {
            let ideal_t = 1_000_000_000.0 + i as f64 * 64.0 / true_rate;
            let t = ideal_t + noise(&mut seed) * 100_000.0;
            let heard = (t - 1_000_000_000.0) * true_rate + noise(&mut seed) * 8.0;
            let m = dll.update(t as u64, heard, 0.0, 64);
            if i > cycles / 2 {
                // Compare the model against the true line half a cycle ahead.
                let probe = t + 600_000.0;
                let truth = (probe - 1_000_000_000.0) * true_rate;
                let err_ns = (m.pos_at(probe as u64) - truth) / true_rate;
                worst_late = worst_late.max(err_ns.abs());
            }
        }
        let m = dll.model;
        let ppm = (m.rate / (48_000.0 / 1e9) - 1.0) * 1e6;
        assert!((ppm - 150.0).abs() < 25.0, "rate estimate {ppm} ppm");
        // Input noise is +-8 frames (+-167 us); the loop must do far better.
        assert!(worst_late < 40_000.0, "worst model error {worst_late} ns");
    }

    #[test]
    fn restart_follows_a_new_speed_and_a_pause() {
        let mut dll = Dll::new(48_000);
        let mut t = 1_000_000_000u64;
        let mut pos = 0.0;
        for _ in 0..500 {
            dll.update(t, pos, 0.0, 64);
            t += 1_333_333;
            pos += 64.0;
        }
        let epoch = dll.model.epoch;
        // Half speed: 32 frames of the track per 64-frame cycle.
        dll.restart(24_000.0);
        for _ in 0..500 {
            dll.update(t, pos, 0.0, 64);
            t += 1_333_333;
            pos += 32.0;
        }
        assert_eq!(dll.model.epoch, epoch + 1);
        assert!((dll.model.rate * 1e9 - 24_000.0).abs() < 10.0, "{}", dll.model.rate * 1e9);
        assert!((dll.model.pos_at(t) - pos).abs() < 2.0);
        // Paused: time passes, the position does not.
        dll.restart(0.0);
        let m = dll.update(t, pos, 0.0, 64);
        assert!(m.valid && m.rate == 0.0);
        assert_eq!(m.pos_at(t + 5_000_000_000), pos);
    }

    #[test]
    fn a_seek_resets_instead_of_slewing() {
        let mut dll = Dll::new(48_000);
        let mut t = 1_000_000_000u64;
        let mut pos = 0.0;
        for _ in 0..1000 {
            dll.update(t, pos, 0.0, 64);
            t += 1_333_333;
            pos += 64.0;
        }
        let before = dll.model.epoch;
        let m = dll.update(t, pos + 48_000.0, 0.0, 64); // jump one second
        assert_eq!(m.epoch, before + 1);
        assert_eq!(m.pos0, pos + 48_000.0);
    }
}
