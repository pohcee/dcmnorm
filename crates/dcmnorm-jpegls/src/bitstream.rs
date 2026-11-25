//! T.87 bit-level reader and writer with single-bit stuffing after `0xFF`.
//!
//! Optimized with:
//! - Left-justified `u64` bit buffer in `BitReader` with SWAR 8-byte `0xFF`-free fast refill
//! - Combined single-refill `read_limited_golomb`
//! - Buffered `BitWriter` (flushing to `W` in chunks, never 1 byte per dynamic dispatch)
//! - Word-level `write_limited_golomb` (no per-bit loop)

use std::io::Write;

use crate::error::CodecError;

/// Fast SWAR test: returns true if any byte in `w` is `0xFF`.
#[inline(always)]
fn has_0xff(w: u64) -> bool {
    let inv = !w;
    (inv.wrapping_sub(0x0101_0101_0101_0101) & !inv & 0x8080_8080_8080_8080) != 0
}

/// Compile-time 8-bit Golomb-Rice decode lookup table for `k in 0..8`.
/// When `len > 0`, the entire non-escaped Golomb code fits in the top `len` bits
/// (so `unary_zeros <= 7 < escape` for all valid T.87 parameters) and decodes to `val`.
#[derive(Clone, Copy)]
struct GolombEntry {
    len: u8,
    val: u8,
}

const fn build_golomb_lut() -> [[GolombEntry; 256]; 8] {
    let mut table = [[GolombEntry { len: 0, val: 0 }; 256]; 8];
    let mut k = 0usize;
    while k < 8 {
        let mut byte = 0usize;
        while byte < 256 {
            let b = byte as u8;
            let zeros = b.leading_zeros() as usize;
            let total = zeros + 1 + k;
            if zeros < 8 && total <= 8 {
                let rem = if k > 0 {
                    (byte >> (8 - total)) & ((1 << k) - 1)
                } else {
                    0
                };
                let val = (zeros << k) | rem;
                table[k][byte] = GolombEntry {
                    len: total as u8,
                    val: val as u8,
                };
            }
            byte += 1;
        }
        k += 1;
    }
    table
}

static GOLOMB_LUT: [[GolombEntry; 256]; 8] = build_golomb_lut();

// ---------------------------------------------------------------------------
// BitReader
// ---------------------------------------------------------------------------

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Left-justified bit buffer: bit 63 is the next bit to read.
    bits: u64,
    /// Number of valid bits currently in `bits` (0..=64).
    n_bits: i32,
    /// Previous byte pulled into `bits` was `0xFF`, so next byte carries 7 bits.
    last_ff: bool,
    /// Marker observed at `self.pos` (not yet consumed).
    pending_marker: Option<u16>,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bits: 0,
            n_bits: 0,
            last_ff: false,
            pending_marker: None,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Byte offset of the marker terminating the scan.
    pub fn marker_pos(&self) -> usize {
        if self.pending_marker.is_some() {
            return self.pos;
        }
        if self.last_ff {
            return self.pos + 1;
        }
        // Rewind any whole 8-bit non-FF bytes that `refill()` read ahead.
        // Because `refill` stops at any marker (`0xFF >=0x80`), any unread
        // whole bytes here are pure data bytes before EOI/SOS.
        let mut rem_bits = self.n_bits;
        let mut p = self.pos;
        while rem_bits >= 8 && p > 0 {
            let prev_is_ff = p >= 2 && self.data[p - 2] == 0xFF;
            let width = if prev_is_ff { 7 } else { 8 };
            if rem_bits < width {
                break;
            }
            rem_bits -= width;
            p -= 1;
        }
        p
    }

    /// Pull as many bits as fit into `self.bits` (up to 56..64 bits).
    /// Never returns an error; stops at EOF or a marker (`pending_marker`).
    #[inline(always)]
    fn refill(&mut self) {
        if !self.last_ff && self.n_bits <= 32 && self.pos + 8 <= self.data.len() {
            let word = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
            if !has_0xff(word) {
                let bytes = ((64 - self.n_bits) >> 3) as usize;
                let shift_bits = (bytes * 8) as u32;
                let mask = if shift_bits == 64 {
                    !0u64
                } else {
                    !0u64 << (64 - shift_bits)
                };
                self.bits |= (word & mask) >> (self.n_bits as u32);
                self.n_bits += shift_bits as i32;
                self.pos += bytes;
                return;
            }
        }
        self.refill_slow();
    }

    #[cold]
    #[inline(never)]
    fn refill_slow(&mut self) {
        while self.n_bits <= 56 {
            if self.pending_marker.is_some() || self.pos >= self.data.len() {
                break;
            }
            let b = self.data[self.pos];
            if self.last_ff {
                self.pos += 1;
                let shift = 57 - self.n_bits;
                self.bits |= (u64::from(b & 0x7F)) << (shift as u32);
                self.n_bits += 7;
                self.last_ff = false;
            } else if b == 0xFF {
                match self.data.get(self.pos + 1).copied() {
                    Some(next) if next >= 0x80 => {
                        self.pending_marker = Some(0xFF00u16 | u16::from(next));
                        break;
                    }
                    _ => {
                        self.pos += 1;
                        let shift = 56 - self.n_bits;
                        self.bits |= 0xFFu64 << (shift as u32);
                        self.n_bits += 8;
                        self.last_ff = true;
                    }
                }
            } else {
                self.pos += 1;
                let shift = 56 - self.n_bits;
                self.bits |= u64::from(b) << (shift as u32);
                self.n_bits += 8;
                self.last_ff = false;
            }
        }
    }

    #[inline(always)]
    fn ensure(&mut self, n: i32) -> Result<(), CodecError> {
        if self.n_bits < n {
            self.refill();
            if self.n_bits < n {
                if let Some(m) = self.pending_marker {
                    return Err(CodecError::Marker(m));
                }
                return Err(CodecError::InvalidData(
                    "unexpected end of JPEG-LS data".into(),
                ));
            }
        }
        Ok(())
    }

    /// Read `n` bits (0..=32) and return them right-justified.
    #[inline(always)]
    pub fn read_bits(&mut self, n: i32) -> Result<u32, CodecError> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure(n)?;
        let val = (self.bits >> (64 - n) as u32) as u32;
        self.bits <<= n as u32;
        self.n_bits -= n;
        Ok(val)
    }

    /// Read a single bit.
    #[inline(always)]
    pub fn read_bit(&mut self) -> Result<u32, CodecError> {
        self.ensure(1)?;
        let val = (self.bits >> 63) as u32;
        self.bits <<= 1;
        self.n_bits -= 1;
        Ok(val)
    }

    /// Read a length-limited Golomb-Rice code (T.87 A.5.3).
    #[inline(always)]
    pub fn read_limited_golomb(
        &mut self,
        k: i32,
        glimit: i32,
        qbpp: i32,
    ) -> Result<u32, CodecError> {
        let escape = glimit - qbpp - 1;
        if self.n_bits < 32 {
            self.refill();
        }
        // Ultra-fast 8-bit L1 LUT path: covers ~85-95% of regular pixels in 1 lookup + 1 shift.
        if (k as u32) < 8 && escape >= 8 {
            let entry = GOLOMB_LUT[k as usize][(self.bits >> 56) as usize];
            if entry.len > 0 && i32::from(entry.len) <= self.n_bits {
                self.bits <<= entry.len;
                self.n_bits -= i32::from(entry.len);
                return Ok(u32::from(entry.val));
            }
        }
        let lz = self.bits.leading_zeros() as i32;
        if lz < self.n_bits {
            if lz < escape {
                let total = lz + 1 + k;
                if total <= self.n_bits {
                    let after_one = self.bits << ((lz + 1) as u32);
                    let r = if k > 0 {
                        (after_one >> ((64 - k) as u32)) as u32
                    } else {
                        0
                    };
                    self.bits = if total == 64 {
                        0
                    } else {
                        self.bits << (total as u32)
                    };
                    self.n_bits -= total;
                    return Ok(((lz as u32) << k) | r);
                }
            } else if lz == escape {
                let total = escape + 1 + qbpp;
                if total <= self.n_bits {
                    let after_one = self.bits << ((escape + 1) as u32);
                    let val = (after_one >> ((64 - qbpp) as u32)) as u32;
                    self.bits = if total == 64 {
                        0
                    } else {
                        self.bits << (total as u32)
                    };
                    self.n_bits -= total;
                    return Ok(val.wrapping_add(1));
                }
            }
        }
        self.read_limited_golomb_slow(k, escape, qbpp)
    }

    #[cold]
    #[inline(never)]
    fn read_limited_golomb_slow(
        &mut self,
        k: i32,
        escape: i32,
        qbpp: i32,
    ) -> Result<u32, CodecError> {
        let mut count = 0i32;
        loop {
            if self.n_bits == 0 {
                self.ensure(1)?;
            }
            let lz = (self.bits.leading_zeros() as i32).min(self.n_bits);
            if lz < self.n_bits {
                let total = count + lz;
                if total > escape {
                    return Err(CodecError::InvalidData(
                        "Golomb unary prefix exceeds escape limit".into(),
                    ));
                }
                let consume = lz + 1;
                self.bits = if consume == 64 {
                    0
                } else {
                    self.bits << (consume as u32)
                };
                self.n_bits -= consume;
                if total < escape {
                    let r = if k > 0 { self.read_bits(k)? } else { 0 };
                    return Ok(((total as u32) << k) | r);
                } else {
                    return Ok(self.read_bits(qbpp)?.wrapping_add(1));
                }
            }
            count += self.n_bits;
            self.bits = 0;
            self.n_bits = 0;
            if count > escape {
                return Err(CodecError::InvalidData(
                    "Golomb unary prefix exceeds escape limit".into(),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BitWriter
// ---------------------------------------------------------------------------

const WRITER_BUF_CAP: usize = 4096;

pub(crate) struct BitWriter<W: Write> {
    inner: W,
    buf: [u8; WRITER_BUF_CAP],
    buf_len: usize,
    /// Right-justified accumulator: valid bits occupy the low `n_bits`.
    bits: u64,
    n_bits: i32,
    last_ff: bool,
}

impl<W: Write> BitWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: [0u8; WRITER_BUF_CAP],
            buf_len: 0,
            bits: 0,
            n_bits: 0,
            last_ff: false,
        }
    }

    #[inline(always)]
    fn push_byte(&mut self, b: u8) -> Result<(), CodecError> {
        if self.buf_len == WRITER_BUF_CAP {
            self.flush_buf()?;
        }
        self.buf[self.buf_len] = b;
        self.buf_len += 1;
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn flush_buf(&mut self) -> Result<(), CodecError> {
        if self.buf_len > 0 {
            self.inner.write_all(&self.buf[..self.buf_len])?;
            self.buf_len = 0;
        }
        Ok(())
    }

    #[inline(always)]
    fn drain(&mut self) -> Result<(), CodecError> {
        while self.n_bits >= 8 {
            let width = if self.last_ff { 7 } else { 8 };
            let shift = self.n_bits - width;
            let mask = (1u64 << width) - 1;
            let b = ((self.bits >> (shift as u32)) & mask) as u8;
            self.push_byte(b)?;
            self.n_bits -= width;
            self.last_ff = b == 0xFF;
        }
        if self.last_ff && self.n_bits >= 7 {
            let shift = self.n_bits - 7;
            let b = ((self.bits >> (shift as u32)) & 0x7F) as u8;
            self.push_byte(b)?;
            self.n_bits -= 7;
            self.last_ff = false;
        }
        Ok(())
    }

    /// Write `n` bits (0..=32) from `val` (MSB-first).
    #[inline(always)]
    pub fn write_bits(&mut self, val: u32, n: i32) -> Result<(), CodecError> {
        if n == 0 {
            return Ok(());
        }
        if self.n_bits + n > 64 {
            self.drain()?;
        }
        let mask = if n == 32 {
            u64::from(u32::MAX)
        } else {
            (1u64 << (n as u32)) - 1
        };
        self.bits = (self.bits << (n as u32)) | (u64::from(val) & mask);
        self.n_bits += n;
        if self.n_bits >= 32 {
            self.drain()?;
        }
        Ok(())
    }

    #[inline(always)]
    pub fn write_bit(&mut self, bit: u32) -> Result<(), CodecError> {
        if self.n_bits >= 56 {
            self.drain()?;
        }
        self.bits = (self.bits << 1) | u64::from(bit & 1);
        self.n_bits += 1;
        Ok(())
    }

    /// Write `count` zero bits followed by a `1` bit, then `rem_bits` low bits of `rem`.
    #[inline(always)]
    fn write_unary_and_rem(
        &mut self,
        zeros: i32,
        rem: u32,
        rem_bits: i32,
    ) -> Result<(), CodecError> {
        let total = zeros + 1 + rem_bits;
        if total <= 32 {
            // Fast path: pack `zeros` zeros + `1` + `rem` into a single shift+OR!
            if self.n_bits + total > 64 {
                self.drain()?;
            }
            let rem_mask = if rem_bits == 32 {
                u64::from(u32::MAX)
            } else {
                (1u64 << (rem_bits as u32)) - 1
            };
            let code = (1u64 << (rem_bits as u32)) | (u64::from(rem) & rem_mask);
            self.bits = (self.bits << (total as u32)) | code;
            self.n_bits += total;
            if self.n_bits >= 32 {
                self.drain()?;
            }
            return Ok(());
        }
        // Slow path: write zeros in chunks of up to 32, then 1, then remainder.
        let mut z = zeros;
        while z > 0 {
            let chunk = z.min(32);
            self.write_bits(0, chunk)?;
            z -= chunk;
        }
        self.write_bit(1)?;
        if rem_bits > 0 {
            self.write_bits(rem, rem_bits)?;
        }
        Ok(())
    }

    #[inline(always)]
    pub fn write_limited_golomb(
        &mut self,
        k: i32,
        val: u32,
        glimit: i32,
        qbpp: i32,
    ) -> Result<(), CodecError> {
        let q = (val >> k) as i32;
        let escape = glimit - qbpp - 1;
        if q < escape {
            self.write_unary_and_rem(q, val, k)
        } else {
            self.write_unary_and_rem(escape, val.wrapping_sub(1), qbpp)
        }
    }

    pub fn flush(&mut self) -> Result<(), CodecError> {
        self.drain()?;
        if self.n_bits > 0 {
            let width = if self.last_ff { 7 } else { 8 };
            debug_assert!(self.n_bits < width, "drain should leave < width bits");
            let pad = width - self.n_bits;
            self.bits <<= pad as u32;
            let mask = (1u64 << (width as u32)) - 1;
            let b = (self.bits & mask) as u8;
            self.push_byte(b)?;
            self.bits = 0;
            self.n_bits = 0;
            self.last_ff = b == 0xFF;
        }
        if self.last_ff {
            self.push_byte(0x00)?;
            self.last_ff = false;
        }
        self.flush_buf()?;
        self.inner.flush()?;
        Ok(())
    }
}
