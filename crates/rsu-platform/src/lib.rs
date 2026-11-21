//! Platform layer: windows, raw input and the audio device. Knows nothing
//! about the game; exposes plain data through narrow interfaces.

pub mod audio_pw;
pub mod evdev;
pub mod window;
