//! Audio engine: the mixer that runs in the device's real-time callback, the
//! clock model that turns audio position into the game's master clock, and
//! load-time decoding. Device I/O lives in rsu-platform; this crate only
//! fills buffers it is handed.

pub mod bank;
pub mod clock;
pub mod decode;
pub mod handoff;
pub mod mixer;
pub mod resample;
pub mod stream;

/// The engine's fixed internal format: 48 kHz interleaved stereo f32.
/// Tracks are converted to it once at load time so the callback only mixes.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
