//! Sample-rate conversion (windowed-sinc, polyphase), whole tracks at once
//! ([`resample_stereo`]) or a piece at a time ([`Resampler`]). Both compute
//! every output frame with the same function from the same input frames,
//! so a track played from the middle sounds exactly like one converted
//! whole. Quality is favoured over speed; nothing here is real-time safe.

const HALF: usize = 16; // taps on each side of the centre
const TAPS: usize = HALF * 2;
const PHASES: usize = 256;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..40 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-14 {
            break;
        }
    }
    sum
}

/// One row per fractional phase (plus a guard row for interpolation).
fn build_table(cutoff: f64) -> Vec<[f32; TAPS]> {
    const BETA: f64 = 9.0;
    let i0b = bessel_i0(BETA);
    (0..=PHASES)
        .map(|p| {
            let frac = p as f64 / PHASES as f64;
            let mut row = [0.0f64; TAPS];
            let mut sum = 0.0;
            for (k, c) in row.iter_mut().enumerate() {
                let x = k as f64 - (HALF as f64 - 1.0) - frac;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    let a = std::f64::consts::PI * x * cutoff;
                    a.sin() / a
                };
                let r = x / HALF as f64;
                let win = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(BETA * (1.0 - r * r).sqrt()) / i0b };
                *c = cutoff * sinc * win;
                sum += *c;
            }
            let mut out = [0.0f32; TAPS];
            for (o, c) in out.iter_mut().zip(row) {
                *o = (c / sum) as f32; // unity gain at DC for every phase
            }
            out
        })
        .collect()
}

fn ratio_of(in_rate: u32, out_rate: u32) -> f64 {
    in_rate as f64 / out_rate as f64
}

fn table_for(ratio: f64) -> Vec<[f32; TAPS]> {
    // When downsampling the passband has to shrink to the new Nyquist.
    build_table((1.0 / ratio).min(1.0) * 0.96)
}

/// The first input frame output frame `n` reads.
fn first_tap(n: u64, ratio: f64) -> i64 {
    (n as f64 * ratio).floor() as i64 - (HALF as i64 - 1)
}

/// Output frame `n`, from input frames `get(i)` (`None` outside the input).
#[inline]
fn frame_at(table: &[[f32; TAPS]], ratio: f64, n: u64, get: impl Fn(i64) -> Option<(f32, f32)>) -> (f32, f32) {
    let pos = n as f64 * ratio;
    let i = pos.floor();
    let frac = (pos - i) * PHASES as f64;
    let p = frac.floor() as usize;
    let t = (frac - p as f64) as f32;
    let (a, b) = (&table[p], &table[p + 1]);
    let base = i as i64 - (HALF as i64 - 1);
    let (mut l, mut r) = (0.0f32, 0.0f32);
    for k in 0..TAPS {
        let Some((x, y)) = get(base + k as i64) else { continue };
        let c = a[k] + (b[k] - a[k]) * t;
        l += x * c;
        r += y * c;
    }
    (l, r)
}

/// Convert interleaved stereo from `in_rate` to `out_rate`.
pub fn resample_stereo(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if in_rate == out_rate || input.is_empty() {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    let ratio = ratio_of(in_rate, out_rate);
    let out_frames = Resampler::out_len(in_frames as u64, in_rate, out_rate) as usize;
    let table = table_for(ratio);
    let mut out = Vec::with_capacity(out_frames * 2);
    let get = |i: i64| (i >= 0 && (i as usize) < in_frames).then(|| (input[i as usize * 2], input[i as usize * 2 + 1]));
    for n in 0..out_frames {
        let (l, r) = frame_at(&table, ratio, n as u64, get);
        out.push(l);
        out.push(r);
    }
    out
}

/// The same conversion fed a piece at a time, starting at any output frame.
pub struct Resampler {
    ratio: f64,
    /// `None` when the rates are the same and frames pass straight through.
    table: Option<Vec<[f32; TAPS]>>,
    /// Input frames held, interleaved, the first being input frame `base`.
    input: Vec<f32>,
    base: u64,
    /// The output frame produced next.
    next: u64,
    /// All the input there is, once the source has ended.
    total: Option<u64>,
    in_rate: u32,
    out_rate: u32,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Resampler {
        let ratio = ratio_of(in_rate, out_rate);
        Resampler {
            ratio,
            table: (in_rate != out_rate).then(|| table_for(ratio)),
            input: Vec::new(),
            base: 0,
            next: 0,
            total: None,
            in_rate,
            out_rate,
        }
    }

    /// Output frames made from `in_frames` input frames.
    pub fn out_len(in_frames: u64, in_rate: u32, out_rate: u32) -> u64 {
        if in_rate == out_rate {
            in_frames
        } else {
            (in_frames as f64 / ratio_of(in_rate, out_rate)).floor() as u64
        }
    }

    /// The first input frame needed to produce output from frame `pos` on.
    pub fn first_input(&self, pos: u64) -> u64 {
        match self.table {
            None => pos,
            Some(_) => first_tap(pos, self.ratio).max(0) as u64,
        }
    }

    /// Start again: output from frame `pos`, input arriving from frame `at`
    /// (which must be [`Self::first_input`] of `pos`).
    pub fn restart(&mut self, pos: u64, at: u64) {
        self.input.clear();
        self.base = at;
        self.next = pos;
        self.total = None;
    }

    pub fn push(&mut self, frames: &[f32]) {
        self.input.extend_from_slice(frames);
    }

    /// The source has ended; whether that is news.
    pub fn finish(&mut self) -> bool {
        let first = self.total.is_none();
        self.total = Some(self.base + (self.input.len() / 2) as u64);
        first
    }

    /// Everything has been produced.
    pub fn finished(&self) -> bool {
        self.total.is_some_and(|t| self.next >= Self::out_len(t, self.in_rate, self.out_rate))
    }

    /// Append to `out` the frames that can be made now, at most `max`.
    pub fn drain(&mut self, out: &mut Vec<f32>, max: usize) {
        let held = self.base + (self.input.len() / 2) as u64;
        let end = match self.total {
            Some(t) => Self::out_len(t, self.in_rate, self.out_rate),
            None => u64::MAX,
        };
        let mut made = 0;
        while made < max && self.next < end {
            let n = self.next;
            let frame = match &self.table {
                None => {
                    if n >= held {
                        break;
                    }
                    let i = (n - self.base) as usize * 2;
                    (self.input[i], self.input[i + 1])
                }
                Some(table) => {
                    // Every tap must be there, unless the input has ended.
                    if self.total.is_none() && first_tap(n, self.ratio) + TAPS as i64 > held as i64 {
                        break;
                    }
                    let (input, base) = (&self.input, self.base as i64);
                    let get = |i: i64| {
                        (i >= 0 && i < held as i64).then(|| {
                            let at = (i - base) as usize * 2;
                            (input[at], input[at + 1])
                        })
                    };
                    frame_at(table, self.ratio, n, get)
                }
            };
            out.push(frame.0);
            out.push(frame.1);
            self.next += 1;
            made += 1;
        }
        // Let go of input no later frame reads.
        let keep_from = match self.table {
            None => self.next,
            Some(_) => first_tap(self.next, self.ratio).max(0) as u64,
        };
        let drop = keep_from.saturating_sub(self.base).min((self.input.len() / 2) as u64) as usize;
        if drop >= 16_384 || (drop > 0 && drop * 2 == self.input.len()) {
            self.input.drain(..drop * 2);
            self.base += drop as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let s = (std::f64::consts::TAU * freq * n as f64 / rate as f64).sin() as f32;
                [s, s]
            })
            .collect()
    }

    fn worst_error(from: u32, to: u32, freq: f64) -> f32 {
        let out = resample_stereo(&sine(from, freq, from as usize / 2), from, to);
        let ideal = sine(to, freq, out.len() / 2);
        // Skip the edges, where the kernel runs off the input.
        let skip = 200;
        out[skip..out.len() - skip].iter().zip(&ideal[skip..]).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn upsampling_preserves_a_tone() {
        assert!(worst_error(44_100, 48_000, 1_000.0) < 1e-3);
        assert!(worst_error(44_100, 48_000, 10_000.0) < 5e-3);
    }

    #[test]
    fn downsampling_preserves_a_tone() {
        assert!(worst_error(96_000, 48_000, 1_000.0) < 1e-3);
    }

    #[test]
    fn length_scales_with_the_ratio() {
        let out = resample_stereo(&vec![0.0; 44_100 * 2], 44_100, 48_000);
        assert_eq!(out.len() / 2, 48_000);
    }

    /// Feeding pieces of any size from any start gives exactly the frames
    /// of the whole conversion.
    #[test]
    fn piecewise_equals_whole() {
        for (from, to) in [(44_100, 48_000), (96_000, 48_000), (48_000, 48_000), (22_050, 48_000)] {
            let input: Vec<f32> =
                (0..from as usize / 3).flat_map(|i| [(i as f32 * 0.37).sin(), (i as f32 * 0.11).cos()]).collect();
            let whole = resample_stereo(&input, from, to);
            for start in [0u64, 1, 777, (whole.len() / 2 - 5) as u64] {
                let mut r = Resampler::new(from, to);
                let at = r.first_input(start);
                r.restart(start, at);
                let mut out = Vec::new();
                let mut fed = at as usize;
                let mut piece = 1;
                while fed < input.len() / 2 {
                    let n = piece.min(input.len() / 2 - fed);
                    r.push(&input[fed * 2..(fed + n) * 2]);
                    fed += n;
                    piece = piece * 3 + 1;
                    r.drain(&mut out, 1000);
                }
                r.finish();
                while !r.finished() {
                    r.drain(&mut out, 1000);
                }
                assert!(out == whole[start as usize * 2..], "{from}->{to} from {start}");
            }
        }
    }
}
