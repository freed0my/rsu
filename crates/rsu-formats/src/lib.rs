//! File formats: `.osu` beatmaps in, rsu replays in and out. Pure parsing
//! and encoding over byte slices; the only I/O is two convenience wrappers.

#![forbid(unsafe_code)]

pub mod curve;
pub mod hitsound;
pub mod lzma;
pub mod md5;
pub mod osr;
pub mod osu;
pub mod osudb;
pub mod replay;
pub mod stacking;
