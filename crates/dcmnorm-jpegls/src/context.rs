//! T.87 (ITU-T T.87 / ISO 14495-1) JPEG-LS context model — optimized AoS + LUT layout.

const NUM_REGULAR_CONTEXTS: usize = 365;
const NUM_RUN_CONTEXTS: usize = 2;
const NUM_CONTEXTS: usize = NUM_REGULAR_CONTEXTS + NUM_RUN_CONTEXTS;
const RUN_CTX_BASE: usize = 365;

const MIN_C: i32 = -128;
const MAX_C: i32 = 127;

const BASIC_T1: i32 = 3;
const BASIC_T2: i32 = 7;
const BASIC_T3: i32 = 21;

pub(crate) const J_TABLE: [i32; 32] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 11, 12, 13,
    14, 15,
];

fn ceil_log2(x: i32) -> i32 {
    if x <= 1 {
        return 0;
    }
    32 - ((x - 1) as u32).leading_zeros() as i32
}

fn clamp_threshold(i: i32, lo: i32, hi: i32) -> i32 {
    if i > hi || i < lo {
        lo
    } else {
        i
    }
}

/// Single cache-friendly context entry (16 bytes = 4 contexts per 64-byte cache line).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CtxEntry {
    pub a: i32,
    pub b: i32,
    pub c: i32,
    pub n: i32,
}

impl CtxEntry {
    #[inline(always)]
    pub fn update_lossless(&mut self, err_val: i32, reset: i32) {
        self.b += err_val;
        self.a += err_val.abs();
        if self.n == reset {
            self.a >>= 1;
            self.b = if self.b >= 0 {
                self.b >> 1
            } else {
                -((1 - self.b) >> 1)
            };
            self.n >>= 1;
        }
        self.n += 1;
        if self.b <= -self.n {
            if self.c > MIN_C {
                self.c -= 1;
            }
            self.b += self.n;
            if self.b <= -self.n {
                self.b = -self.n + 1;
            }
        } else if self.b > 0 {
            if self.c < MAX_C {
                self.c += 1;
            }
            self.b -= self.n;
            if self.b > 0 {
                self.b = 0;
            }
        }
    }

    #[inline(always)]
    pub fn update_lossy(&mut self, err_val: i32, step: i32, reset: i32) {
        self.b += err_val * step;
        self.a += err_val.abs();
        if self.n == reset {
            self.a >>= 1;
            self.b = if self.b >= 0 {
                self.b >> 1
            } else {
                -((1 - self.b) >> 1)
            };
            self.n >>= 1;
        }
        self.n += 1;
        if self.b <= -self.n {
            if self.c > MIN_C {
                self.c -= 1;
            }
            self.b += self.n;
            if self.b <= -self.n {
                self.b = -self.n + 1;
            }
        } else if self.b > 0 {
            if self.c < MAX_C {
                self.c += 1;
            }
            self.b -= self.n;
            if self.b > 0 {
                self.b = 0;
            }
        }
    }
}

/// Inline quantization LUT covers `2 * t3 + 1 <= 1024` (`t3 <= 511`), which covers all
/// standard 2..=16-bit T.87 default thresholds with zero heap allocation and zero bounds checks.
/// For 8-bit (`max_val <= 255`), `d` is guaranteed in `[-255, 255]`, so lookups skip clamping.
const INLINE_QUANT_MAX_T3: i32 = 511;
const INLINE_QUANT_LEN: usize = 1024;
const NO_CLAMP_OFFSET: i32 = 256;

pub(crate) struct ContextModel {
    pub t1: i32,
    pub t2: i32,
    pub t3: i32,
    pub max_val: i32,
    pub near: i32,
    pub range: i32,
    pub qbpp: i32,
    pub limit: i32,
    pub reset: i32,
    pub no_clamp_quant: bool,
    pub use_inline_quant: bool,
    pub is_pow2_range: bool,

    /// Compact AoS context table (365 regular + 2 run contexts).
    pub ctx: [CtxEntry; NUM_CONTEXTS],
    /// Nn[RItype]: negative-error counters for the two run contexts (A.7.2).
    pub nn: [i32; 2],

    /// Inline gradient quantization LUT.
    /// - When `no_clamp_quant` (`max_val <= 255`): `quant_lut[(d + 256) & 511]`.
    /// - Otherwise when `use_inline_quant` (`t3 <= 511`): `quant_lut[(d.clamp(-t3, t3) + t3) & 1023]`.
    quant_lut: [i8; INLINE_QUANT_LEN],

    pub j: [i32; 32],
    pub run_index: usize,
}

impl ContextModel {
    pub fn new(max_val: i32, near: i32, reset: i32) -> Self {
        Self::with_presets(max_val, near, reset, None, None, None)
    }

    pub fn with_presets(
        max_val: i32,
        near: i32,
        reset: i32,
        t1_override: Option<i32>,
        t2_override: Option<i32>,
        t3_override: Option<i32>,
    ) -> Self {
        let bpp = ceil_log2(max_val + 1).max(2);
        let limit = 2 * (bpp + bpp.max(8));
        let range = if near == 0 {
            max_val + 1
        } else {
            (max_val + 2 * near) / (2 * near + 1) + 1
        };
        let qbpp = ceil_log2(range);

        let (dt1, dt2, dt3) = Self::default_thresholds(max_val, near);
        let t1 = t1_override.unwrap_or(dt1);
        let t2 = t2_override.unwrap_or(dt2);
        let t3 = t3_override.unwrap_or(dt3);

        let a_init = ((range + 32) / 64).max(2);
        let entry = CtxEntry {
            a: a_init,
            b: 0,
            c: 0,
            n: 1,
        };

        let no_clamp_quant = max_val <= 255;
        let use_inline_quant = t3 <= INLINE_QUANT_MAX_T3;
        let mut quant_lut = [0i8; INLINE_QUANT_LEN];
        if no_clamp_quant {
            for (idx, slot) in quant_lut[..512].iter_mut().enumerate() {
                let d = idx as i32 - NO_CLAMP_OFFSET;
                *slot = Self::quantize_raw(d, t1, t2, t3, near) as i8;
            }
        } else if use_inline_quant {
            let len = (2 * t3 + 1) as usize;
            for (idx, slot) in quant_lut[..len].iter_mut().enumerate() {
                let d = idx as i32 - t3;
                *slot = Self::quantize_raw(d, t1, t2, t3, near) as i8;
            }
        }
        let is_pow2_range = near == 0 && range == max_val + 1 && (range & max_val) == 0;

        Self {
            t1,
            t2,
            t3,
            max_val,
            near,
            range,
            qbpp,
            limit,
            reset,
            no_clamp_quant,
            use_inline_quant,
            is_pow2_range,
            ctx: [entry; NUM_CONTEXTS],
            nn: [0, 0],
            quant_lut,
            j: J_TABLE,
            run_index: 0,
        }
    }

    #[inline(always)]
    fn quantize_raw(d: i32, t1: i32, t2: i32, t3: i32, near: i32) -> i32 {
        if d <= -t3 {
            -4
        } else if d <= -t2 {
            -3
        } else if d <= -t1 {
            -2
        } else if d < -near {
            -1
        } else if d <= near {
            0
        } else if d < t1 {
            1
        } else if d < t2 {
            2
        } else if d < t3 {
            3
        } else {
            4
        }
    }

    pub(crate) fn default_thresholds(max_val: i32, near: i32) -> (i32, i32, i32) {
        if max_val >= 128 {
            let factor = (max_val.min(4095) + 128) / 256;
            let t1 = clamp_threshold(factor * (BASIC_T1 - 2) + 2 + 3 * near, near + 1, max_val);
            let t2 = clamp_threshold(factor * (BASIC_T2 - 3) + 3 + 5 * near, t1, max_val);
            let t3 = clamp_threshold(factor * (BASIC_T3 - 4) + 4 + 7 * near, t2, max_val);
            (t1, t2, t3)
        } else {
            let factor = 256 / (max_val + 1);
            let t1 = clamp_threshold((BASIC_T1 / factor + 3 * near).max(2), near + 1, max_val);
            let t2 = clamp_threshold((BASIC_T2 / factor + 5 * near).max(3), t1, max_val);
            let t3 = clamp_threshold((BASIC_T3 / factor + 7 * near).max(4), t2, max_val);
            (t1, t2, t3)
        }
    }

    #[cold]
    #[inline(never)]
    fn compute_qs_slow(&self, d1: i32, d2: i32, d3: i32) -> i32 {
        let q1 = Self::quantize_raw(d1, self.t1, self.t2, self.t3, self.near);
        let q2 = Self::quantize_raw(d2, self.t1, self.t2, self.t3, self.near);
        let q3 = Self::quantize_raw(d3, self.t1, self.t2, self.t3, self.near);
        (q1 * 9 + q2) * 9 + q3
    }

    /// Compute the signed context id `qs` in `-364..=364`.
    /// `qs == 0` iff all three gradients are within `[-near, near]` (run mode condition).
    #[inline(always)]
    pub fn compute_qs(&self, d1: i32, d2: i32, d3: i32) -> i32 {
        if self.no_clamp_quant {
            let q1 = i32::from(self.quant_lut[((d1 + NO_CLAMP_OFFSET) as usize) & 511]);
            let q2 = i32::from(self.quant_lut[((d2 + NO_CLAMP_OFFSET) as usize) & 511]);
            let q3 = i32::from(self.quant_lut[((d3 + NO_CLAMP_OFFSET) as usize) & 511]);
            (q1 * 9 + q2) * 9 + q3
        } else if self.use_inline_quant {
            let t3 = self.t3;
            let q1 = i32::from(self.quant_lut[((d1.clamp(-t3, t3) + t3) as usize) & 1023]);
            let q2 = i32::from(self.quant_lut[((d2.clamp(-t3, t3) + t3) as usize) & 1023]);
            let q3 = i32::from(self.quant_lut[((d3.clamp(-t3, t3) + t3) as usize) & 1023]);
            (q1 * 9 + q2) * 9 + q3
        } else {
            self.compute_qs_slow(d1, d2, d3)
        }
    }

    /// Split signed `qs` into `(q_index in 0..=364, sign in {-1, 1})` branchlessly.
    #[inline(always)]
    pub fn split_qs(qs: i32) -> (usize, i32) {
        let mask = qs >> 31;
        let q = ((qs ^ mask) - mask) as usize;
        (q.min(364), 1 | mask)
    }

    /// Branchless Golomb-Rice parameter `k`: smallest `k >= 0` with `(n << k) >= a`.
    #[inline(always)]
    pub fn compute_k_from(n: i32, a: i32) -> i32 {
        if n >= a {
            return 0;
        }
        let s = (n.leading_zeros() - a.leading_zeros()) as i32;
        s + i32::from((n << s) < a)
    }

    /// Reset adaptive statistics and run state at a JPEG-LS restart marker (RSTm).
    pub fn reset_for_restart(&mut self) {
        let a_init = ((self.range + 32) / 64).max(2);
        let entry = CtxEntry {
            a: a_init,
            b: 0,
            c: 0,
            n: 1,
        };
        self.ctx.fill(entry);
        self.nn = [0, 0];
        self.run_index = 0;
    }

    #[inline(always)]
    pub fn increment_run_index(&mut self) {
        if self.run_index < 31 {
            self.run_index += 1;
        }
    }

    #[inline(always)]
    pub fn decrement_run_index(&mut self) {
        if self.run_index > 0 {
            self.run_index -= 1;
        }
    }

    #[inline(always)]
    pub fn compute_k_run(&self, ri_type: usize) -> i32 {
        let e = &self.ctx[RUN_CTX_BASE + ri_type];
        let temp = if ri_type == 1 { e.a + (e.n >> 1) } else { e.a };
        Self::compute_k_from(e.n, temp)
    }

    #[inline(always)]
    pub fn update_run_stats(&mut self, ri_type: usize, e_mapped: u32, negative: bool) {
        if negative {
            self.nn[ri_type] += 1;
        }
        let reset = self.reset;
        let e = &mut self.ctx[RUN_CTX_BASE + ri_type];
        e.a += ((e_mapped + 1 - ri_type as u32) >> 1) as i32;
        if e.n == reset {
            e.a >>= 1;
            e.n >>= 1;
            self.nn[ri_type] >>= 1;
        }
        e.n += 1;
    }
}
