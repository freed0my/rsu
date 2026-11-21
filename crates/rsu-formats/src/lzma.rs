//! A decoder for the `.lzma` ("LZMA alone") container, the one osu! uses for
//! the cursor stream inside `.osr` replays. Written from the reference
//! decoder in Igor Pavlov's LZMA SDK (`LzmaSpec.cpp`), which is public
//! domain; the whole output is kept in memory and serves as the dictionary.

#[derive(Debug, PartialEq, Eq)]
pub enum LzmaError {
    Truncated,
    Corrupt(&'static str),
}

impl std::fmt::Display for LzmaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LzmaError::Truncated => write!(f, "LZMA stream is truncated"),
            LzmaError::Corrupt(what) => write!(f, "LZMA stream is corrupt ({what})"),
        }
    }
}

impl std::error::Error for LzmaError {}

type Res<T> = Result<T, LzmaError>;

const PROB_INIT: u16 = 1 << 10;
const NUM_STATES: usize = 12;
const POS_BITS_MAX: usize = 4;
const END_POS_MODEL_INDEX: u32 = 14;
const NUM_FULL_DISTANCES: usize = 1 << (END_POS_MODEL_INDEX >> 1);
const NUM_ALIGN_BITS: u32 = 4;
const MATCH_MIN_LEN: usize = 2;

/// No sane replay comes close; this only stops a damaged size field from
/// asking for gigabytes up front.
const MAX_RESERVE: u64 = 64 << 20;

struct Range<'a> {
    data: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
}

impl<'a> Range<'a> {
    fn new(data: &'a [u8]) -> Res<Range<'a>> {
        if data.len() < 5 {
            return Err(LzmaError::Truncated);
        }
        if data[0] != 0 {
            return Err(LzmaError::Corrupt("range coder start"));
        }
        let code = u32::from_be_bytes(data[1..5].try_into().unwrap());
        if code == u32::MAX {
            return Err(LzmaError::Corrupt("range coder start"));
        }
        Ok(Range { data, pos: 5, range: u32::MAX, code })
    }

    fn byte(&mut self) -> Res<u8> {
        let b = *self.data.get(self.pos).ok_or(LzmaError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn normalize(&mut self) -> Res<()> {
        if self.range < 1 << 24 {
            self.range <<= 8;
            self.code = (self.code << 8) | self.byte()? as u32;
        }
        Ok(())
    }

    fn bit(&mut self, prob: &mut u16) -> Res<u32> {
        let bound = (self.range >> 11) * *prob as u32;
        let bit = if self.code < bound {
            self.range = bound;
            *prob += ((1 << 11) - *prob) >> 5;
            0
        } else {
            self.range -= bound;
            self.code -= bound;
            *prob -= *prob >> 5;
            1
        };
        self.normalize()?;
        Ok(bit)
    }

    fn direct(&mut self, bits: u32) -> Res<u32> {
        let mut v = 0;
        for _ in 0..bits {
            self.range >>= 1;
            let b = if self.code >= self.range {
                self.code -= self.range;
                1
            } else {
                0
            };
            v = (v << 1) | b;
            self.normalize()?;
        }
        Ok(v)
    }

    fn tree(&mut self, probs: &mut [u16], bits: u32) -> Res<u32> {
        let mut m = 1usize;
        for _ in 0..bits {
            m = (m << 1) | self.bit(&mut probs[m])? as usize;
        }
        Ok(m as u32 - (1 << bits))
    }

    fn reverse(&mut self, probs: &mut [u16], bits: u32) -> Res<u32> {
        let mut m = 1usize;
        let mut v = 0;
        for i in 0..bits {
            let b = self.bit(&mut probs[m])?;
            m = (m << 1) | b as usize;
            v |= b << i;
        }
        Ok(v)
    }
}

struct Len {
    choice: u16,
    choice2: u16,
    low: [[u16; 1 << 3]; 1 << POS_BITS_MAX],
    mid: [[u16; 1 << 3]; 1 << POS_BITS_MAX],
    high: [u16; 1 << 8],
}

impl Len {
    fn new() -> Len {
        Len {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [[PROB_INIT; 8]; 16],
            mid: [[PROB_INIT; 8]; 16],
            high: [PROB_INIT; 256],
        }
    }

    fn decode(&mut self, rc: &mut Range, pos_state: usize) -> Res<usize> {
        Ok(if rc.bit(&mut self.choice)? == 0 {
            rc.tree(&mut self.low[pos_state], 3)? as usize
        } else if rc.bit(&mut self.choice2)? == 0 {
            8 + rc.tree(&mut self.mid[pos_state], 3)? as usize
        } else {
            16 + rc.tree(&mut self.high, 8)? as usize
        })
    }
}

/// Decompress a whole `.lzma` stream: 5 bytes of properties, a 64-bit
/// uncompressed size (all ones when unknown, then an end marker closes the
/// stream), and the range-coded data.
pub fn decompress(data: &[u8]) -> Res<Vec<u8>> {
    if data.len() < 13 {
        return Err(LzmaError::Truncated);
    }
    let mut d = data[0] as u32;
    if d >= 9 * 5 * 5 {
        return Err(LzmaError::Corrupt("properties"));
    }
    let lc = d % 9;
    d /= 9;
    let lp = d % 5;
    let pb = d / 5;
    let dict_size = u32::from_le_bytes(data[1..5].try_into().unwrap()).max(1 << 12) as usize;
    let size = u64::from_le_bytes(data[5..13].try_into().unwrap());
    let known = size != u64::MAX;
    let mut out: Vec<u8> = Vec::with_capacity(if known { size.min(MAX_RESERVE) as usize } else { 0 });
    let mut rc = Range::new(&data[13..])?;

    let mut literal = vec![PROB_INIT; 0x300 << (lc + lp)];
    let mut pos_slot = [[PROB_INIT; 1 << 6]; 4];
    let mut pos_decoders = [PROB_INIT; 1 + NUM_FULL_DISTANCES - END_POS_MODEL_INDEX as usize];
    let mut align = [PROB_INIT; 1 << NUM_ALIGN_BITS];
    let mut is_match = [PROB_INIT; NUM_STATES << POS_BITS_MAX];
    let mut is_rep = [PROB_INIT; NUM_STATES];
    let mut is_rep_g0 = [PROB_INIT; NUM_STATES];
    let mut is_rep_g1 = [PROB_INIT; NUM_STATES];
    let mut is_rep_g2 = [PROB_INIT; NUM_STATES];
    let mut is_rep0_long = [PROB_INIT; NUM_STATES << POS_BITS_MAX];
    let mut len_decoder = Len::new();
    let mut rep_len_decoder = Len::new();

    let (mut rep0, mut rep1, mut rep2, mut rep3) = (0usize, 0usize, 0usize, 0usize);
    let mut state = 0usize;
    let pb_mask = (1usize << pb) - 1;
    let lp_mask = (1usize << lp) - 1;

    loop {
        if known && out.len() as u64 == size {
            // An end marker may still follow; it is allowed but not needed.
            return Ok(out);
        }
        let pos_state = out.len() & pb_mask;
        if rc.bit(&mut is_match[(state << POS_BITS_MAX) + pos_state])? == 0 {
            let prev = out.last().copied().unwrap_or(0) as usize;
            let lit_state = ((out.len() & lp_mask) << lc) + (prev >> (8 - lc));
            let probs = &mut literal[0x300 * lit_state..0x300 * (lit_state + 1)];
            let mut symbol = 1usize;
            if state >= 7 {
                let mut match_byte = out[out.len() - rep0 - 1] as usize;
                while symbol < 0x100 {
                    let match_bit = (match_byte >> 7) & 1;
                    match_byte <<= 1;
                    let bit = rc.bit(&mut probs[((1 + match_bit) << 8) + symbol])? as usize;
                    symbol = (symbol << 1) | bit;
                    if match_bit != bit {
                        break;
                    }
                }
            }
            while symbol < 0x100 {
                symbol = (symbol << 1) | rc.bit(&mut probs[symbol])? as usize;
            }
            out.push((symbol - 0x100) as u8);
            state = if state < 4 {
                0
            } else if state < 10 {
                state - 3
            } else {
                state - 6
            };
            continue;
        }

        let len;
        if rc.bit(&mut is_rep[state])? != 0 {
            if out.is_empty() {
                return Err(LzmaError::Corrupt("repeat before any data"));
            }
            if rc.bit(&mut is_rep_g0[state])? == 0 {
                if rc.bit(&mut is_rep0_long[(state << POS_BITS_MAX) + pos_state])? == 0 {
                    state = if state < 7 { 9 } else { 11 };
                    out.push(out[out.len() - rep0 - 1]);
                    continue;
                }
            } else {
                let dist;
                if rc.bit(&mut is_rep_g1[state])? == 0 {
                    dist = rep1;
                } else {
                    if rc.bit(&mut is_rep_g2[state])? == 0 {
                        dist = rep2;
                    } else {
                        dist = rep3;
                        rep3 = rep2;
                    }
                    rep2 = rep1;
                }
                rep1 = rep0;
                rep0 = dist;
            }
            len = rep_len_decoder.decode(&mut rc, pos_state)?;
            state = if state < 7 { 8 } else { 11 };
        } else {
            rep3 = rep2;
            rep2 = rep1;
            rep1 = rep0;
            len = len_decoder.decode(&mut rc, pos_state)?;
            state = if state < 7 { 7 } else { 10 };

            let slot = rc.tree(&mut pos_slot[len.min(3)], 6)?;
            let dist = if slot < 4 {
                slot
            } else {
                let direct = (slot >> 1) - 1;
                let base = (2 | (slot & 1)) << direct;
                if slot < END_POS_MODEL_INDEX {
                    let from = (base - slot) as usize;
                    base + rc.reverse(&mut pos_decoders[from..], direct)?
                } else {
                    let high = rc.direct(direct - NUM_ALIGN_BITS)? << NUM_ALIGN_BITS;
                    base.wrapping_add(high).wrapping_add(rc.reverse(&mut align, NUM_ALIGN_BITS)?)
                }
            };
            if dist == u32::MAX {
                if known && out.len() as u64 != size {
                    return Err(LzmaError::Corrupt("end marker before the declared size"));
                }
                return Ok(out);
            }
            rep0 = dist as usize;
            if rep0 >= dict_size || rep0 >= out.len() {
                return Err(LzmaError::Corrupt("distance"));
            }
        }

        let n = len + MATCH_MIN_LEN;
        if known {
            let left = size - out.len() as u64;
            if (n as u64) > left {
                return Err(LzmaError::Corrupt("match past the declared size"));
            }
        }
        // A match may overlap its own output, so copy byte by byte.
        let from = out.len() - rep0 - 1;
        for i in 0..n {
            out.push(out[from + i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `printf 'hello hello hello lzma\n' | xz --format=lzma -9`: known size
    /// left unset (all ones) and closed by an end marker.
    const HELLO: &[u8] = &[
        0x5d, 0x00, 0x00, 0x00, 0x04, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x34, 0x19, 0x49, 0xee,
        0x8d, 0xe9, 0x50, 0x95, 0xfc, 0x0b, 0xee, 0xfb, 0x1a, 0xd9, 0x1f, 0xdf, 0xff, 0xff, 0xf6, 0x62, 0x80, 0x00,
    ];

    #[test]
    fn a_known_size_stops_the_decoder_without_a_marker() {
        // The same stream with its size filled in: decoding stops there even
        // though the end marker that follows is never read.
        let mut sized = HELLO.to_vec();
        sized[5..13].copy_from_slice(&23u64.to_le_bytes());
        assert_eq!(decompress(&sized).unwrap(), b"hello hello hello lzma\n");
        sized[5..13].copy_from_slice(&5u64.to_le_bytes());
        assert_eq!(decompress(&sized).unwrap(), b"hello");
    }

    #[test]
    fn decodes_an_xz_made_stream_with_end_marker() {
        assert_eq!(decompress(HELLO).unwrap(), b"hello hello hello lzma\n");
    }

    #[test]
    fn damage_is_an_error_not_a_panic() {
        assert_eq!(decompress(&HELLO[..10]), Err(LzmaError::Truncated));
        for cut in 13..HELLO.len() {
            let _ = decompress(&HELLO[..cut]);
        }
        for at in 13..HELLO.len() {
            let mut bad = HELLO.to_vec();
            bad[at] ^= 0x55;
            let _ = decompress(&bad);
        }
    }
}
