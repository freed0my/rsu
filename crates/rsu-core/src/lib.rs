//! The game core: osu!-standard rules as a deterministic state machine.
//!
//! Input is a stream of timestamped events in *map time*; output is
//! judgements and score. The core never reads a clock, never touches a
//! window, a device or a file, and uses integer arithmetic for everything
//! that decides a judgement, so the same events always give the same result.
//! That one property is what replays, headless tests and seeking rest on.

#![forbid(unsafe_code)]

pub mod auto;
pub mod chart;
pub mod game;
pub mod hp;

pub use chart::{Chart, ChartBuilder, Difficulty, HitObject, NestedKind, ObjectKind};
pub use game::{Button, Event, Game, HitResult, Input, InputKind, Summary};

/// Map time in microseconds. 0 is the start of the audio track.
pub type Time = i64;
pub const MS: Time = 1_000;

/// Position in 1/256 of an osu! pixel. The playfield is 512x384 osu! pixels.
/// Positions are quantised once, where input enters the core, so live play
/// and replay feed it bit-identical values.
pub type Fx = i32;
pub const FX_ONE: i32 = 256;

#[inline]
pub fn to_fx(px: f32) -> Fx {
    (px * FX_ONE as f32).round() as Fx
}

#[inline]
pub fn from_fx(v: Fx) -> f32 {
    v as f32 / FX_ONE as f32
}

/// Mods, as the bits osu! uses in replays.
pub mod mods {
    use crate::chart::Difficulty;

    /// No fail: health still moves, but running out of it does not end the play.
    pub const NO_FAIL: u32 = 1;
    /// Easy: circle size, approach rate, accuracy and drain halved.
    pub const EASY: u32 = 2;
    /// Hidden: no approach circles, objects fade out before their time.
    /// Changes only what is drawn.
    pub const HIDDEN: u32 = 8;
    /// Hard rock: circle size times 1.3, the rest times 1.4 (at most 10),
    /// the playfield flipped upside down.
    pub const HARD_ROCK: u32 = 16;
    /// UI/live-only until recordings have versioned SD/PF semantics.
    pub const SUDDEN_DEATH: u32 = 32;
    pub const PERFECT: u32 = 16384;
    pub const DOUBLE_TIME: u32 = 64;
    pub const HALF_TIME: u32 = 256;
    /// Relax: objects are clicked for the player and keys are not needed to
    /// follow sliders or spin.
    pub const RELAX: u32 = 128;
    /// Live-only until recordings can identify these semantics.
    pub const SPUN_OUT: u32 = 4096;
    /// Double time with raised pitch; always set together with DOUBLE_TIME.
    pub const NIGHTCORE: u32 = 512;

    /// Every mod this build plays.
    pub const KNOWN: u32 = NO_FAIL | EASY | HIDDEN | HARD_ROCK | DOUBLE_TIME | HALF_TIME | RELAX | NIGHTCORE;

    /// How fast the map runs: 1.5 for double time, 0.75 for half time.
    pub fn speed(mods: u32) -> f64 {
        if mods & (DOUBLE_TIME | NIGHTCORE) != 0 {
            1.5
        } else if mods & HALF_TIME != 0 {
            0.75
        } else {
            1.0
        }
    }

    /// Slowest and fastest custom rate, in percent (McOsu's speed slider
    /// runs to 2.5x; 0.05x is the least its audio library plays).
    pub const RATE_MIN_PCT: u16 = 5;
    pub const RATE_MAX_PCT: u16 = 250;

    /// How fast the map runs when the player picked a rate of their own:
    /// `rate_pct` percent, or what the mods say when it is 0. Like double
    /// time and half time it changes real time only; the rules, the
    /// windows and the score multiplier stay in map time (McOsu's
    /// `osu_speed_override` works the same way).
    pub fn speed_with(mods: u32, rate_pct: u16) -> f64 {
        match rate_pct {
            0 => speed(mods),
            pct => pct.clamp(RATE_MIN_PCT, RATE_MAX_PCT) as f64 / 100.0,
        }
    }

    /// A rate as a number of percent; one that equals what the mods already
    /// give is no rate of its own (0).
    pub fn rate_pct_for(mods: u32, speed: f64) -> u16 {
        let pct = (speed * 100.0).round().clamp(RATE_MIN_PCT as f64, RATE_MAX_PCT as f64) as u16;
        if (speed_with(mods, pct) - self::speed(mods)).abs() < 1e-9 {
            0
        } else {
            pct
        }
    }

    /// The map's difficulty as the mods make it (stable's numbers; the
    /// speed mods change times, not these values).
    pub fn difficulty(d: Difficulty, mods: u32) -> Difficulty {
        let (cs, rest) = if mods & HARD_ROCK != 0 {
            (1.3, 1.4)
        } else if mods & EASY != 0 {
            (0.5, 0.5)
        } else {
            return d;
        };
        let f = |v: f32, by: f32| (v * by).clamp(0.0, 10.0);
        Difficulty { cs: f(d.cs, cs), od: f(d.od, rest), ar: f(d.ar, rest), hp: f(d.hp, rest) }
    }

    /// Whether the playfield is upside down.
    pub fn flips(mods: u32) -> bool {
        mods & HARD_ROCK != 0
    }

    /// The score multiplier in millionths, the product of stable's factors
    /// (exact in this unit for every combination).
    pub fn score_ppm(mods: u32) -> u64 {
        let mut m = 1_000_000u64;
        let mut by = |percent: u64| m = m * percent / 100;
        if mods & NO_FAIL != 0 {
            by(50);
        }
        if mods & EASY != 0 {
            by(50);
        }
        if mods & HALF_TIME != 0 {
            by(30);
        }
        if mods & HARD_ROCK != 0 {
            by(106);
        }
        if mods & (DOUBLE_TIME | NIGHTCORE) != 0 {
            by(112);
        }
        if mods & HIDDEN != 0 {
            by(106);
        }
        m
    }

    /// Short names in osu!'s order: NF EZ HD HR DT/NC HT RX.
    const NAMES: [(u32, &str); 11] = [
        (NO_FAIL, "NF"),
        (EASY, "EZ"),
        (HIDDEN, "HD"),
        (HARD_ROCK, "HR"),
        (PERFECT, "PF"),
        (SUDDEN_DEATH, "SD"),
        (NIGHTCORE, "NC"),
        (DOUBLE_TIME, "DT"),
        (HALF_TIME, "HT"),
        (RELAX, "RX"),
        (SPUN_OUT, "SO"),
    ];

    /// The mods as "HDHR", "DTRX", or "" for none; displays without
    /// allocating.
    #[derive(Clone, Copy)]
    pub struct Names(pub u32);

    impl std::fmt::Display for Names {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for (bit, name) in NAMES {
                // Nightcore is shown instead of double time, not beside it.
                if self.0 & bit != 0 && !(bit == DOUBLE_TIME && self.0 & NIGHTCORE != 0) && !(bit == SUDDEN_DEATH && self.0 & PERFECT != 0) {
                    f.write_str(name)?;
                }
            }
            Ok(())
        }
    }

    pub fn name(mods: u32) -> Names {
        Names(mods)
    }

    /// Parse a mod list such as "dt", "hd,hr" or "HDHR". Unknown names are
    /// an error.
    pub fn parse(text: &str) -> Result<u32, String> {
        parse_with_live_mods(text, false)
    }

    /// Parse live-play mods; these must not be used to generate replays.
    pub fn parse_live(text: &str) -> Result<u32, String> {
        parse_with_live_mods(text, true)
    }

    fn parse_with_live_mods(text: &str, live: bool) -> Result<u32, String> {
        let mut mods = 0;
        for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let lower = part.to_ascii_lowercase();
            let pieces: Vec<&str> = match lower.as_str() {
                "relax" | "none" | "nm" => vec![lower.as_str()],
                _ if lower.len() % 2 == 0 && lower.is_ascii() => {
                    (0..lower.len()).step_by(2).map(|i| &lower[i..i + 2]).collect()
                }
                _ => vec![lower.as_str()],
            };
            for piece in pieces {
                mods |= match piece {
                    "nf" => NO_FAIL,
                    "ez" => EASY,
                    "hd" => HIDDEN,
                    "hr" => HARD_ROCK,
                    "dt" => DOUBLE_TIME,
                    "nc" => DOUBLE_TIME | NIGHTCORE,
                    "ht" => HALF_TIME,
                    "rx" | "relax" => RELAX,
                    "so" if live => SPUN_OUT,
                    "sd" => SUDDEN_DEATH,
                    "pf" => SUDDEN_DEATH | PERFECT,
                    "none" | "nm" => 0,
                    other => return Err(format!("unknown mod '{other}' (known: nf, ez, hd, hr, dt, nc, ht, rx, sd, pf)")),
                };
            }
        }
        if mods & DOUBLE_TIME != 0 && mods & HALF_TIME != 0 {
            return Err("double time and half time exclude each other".into());
        }
        if mods & EASY != 0 && mods & HARD_ROCK != 0 {
            return Err("easy and hard rock exclude each other".into());
        }
        if mods & NO_FAIL != 0 && mods & SUDDEN_DEATH != 0 {
            return Err("no fail and sudden death/perfect exclude each other".into());
        }
        Ok(mods)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cli_spun_out_mod() {
            assert_eq!(parse_live("so"), Ok(SPUN_OUT));
            assert!(parse("so").is_err());
            assert_eq!(parse_live("HDSO"), Ok(HIDDEN | SPUN_OUT));
        }

        #[test]
        fn cli_sudden_death_and_perfect_mods() {
            assert_eq!(parse("sd"), Ok(SUDDEN_DEATH));
            assert_eq!(parse("pf"), Ok(SUDDEN_DEATH | PERFECT));
            assert_eq!(parse("hdpf"), Ok(HIDDEN | SUDDEN_DEATH | PERFECT));
            assert!(parse("nfsd").is_err());
        }

        #[test]
        fn names_and_parsing() {
            assert_eq!(name(HIDDEN | HARD_ROCK | DOUBLE_TIME | NIGHTCORE).to_string(), "HDHRNC");
            assert_eq!(name(0).to_string(), "");
            assert_eq!(parse("hdhr"), Ok(HIDDEN | HARD_ROCK));
            assert_eq!(parse("HD, dt"), Ok(HIDDEN | DOUBLE_TIME));
            assert!(parse("ezhr").is_err() && parse("fl").is_err());
        }

        #[test]
        fn custom_rate_overrides_the_mods() {
            assert_eq!(speed_with(DOUBLE_TIME, 0), 1.5);
            assert_eq!(speed_with(DOUBLE_TIME, 130), 1.3);
            assert_eq!(speed_with(0, 1), 0.05, "clamped to the slowest");
            assert_eq!(speed_with(0, 900), 2.5);
            assert_eq!(rate_pct_for(0, 1.0), 0);
            assert_eq!(rate_pct_for(DOUBLE_TIME, 1.5), 0);
            assert_eq!(rate_pct_for(0, 1.25), 125);
            assert_eq!(rate_pct_for(HALF_TIME, 1.0), 100);
        }

        #[test]
        fn difficulty_and_score() {
            let d = Difficulty { cs: 4.0, od: 8.0, ar: 9.0, hp: 5.0 };
            let hr = difficulty(d, HARD_ROCK);
            assert!((hr.cs - 5.2).abs() < 1e-5 && hr.ar == 10.0 && hr.od == 10.0 && (hr.hp - 7.0).abs() < 1e-5);
            assert_eq!(difficulty(d, EASY), Difficulty { cs: 2.0, od: 4.0, ar: 4.5, hp: 2.5 });
            assert_eq!(score_ppm(HIDDEN | HARD_ROCK | DOUBLE_TIME), 1_258_432);
            assert_eq!(score_ppm(EASY | HALF_TIME), 150_000);
        }
    }
}

/// Version of the rules. Bump on any change that can alter a judgement; a
/// replay only promises to reproduce under the version that recorded it.
pub const RULESET_VERSION: u32 = 4;
