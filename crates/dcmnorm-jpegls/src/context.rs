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

pub(crate) struct ContextModel {
    pub t3: i32,
    pub max_val: i32,
    pub near: i32,
    pub range: i32,
    pub qbpp: i32,
    pub limit: i32,
    pub reset: i32,

    /// Compact AoS context table (365 regular + 2 run contexts).
    pub ctx: [CtxEntry; NUM_CONTEXTS],
    /// Nn[RItype]: negative-error counters for the two run contexts (A.7.2).
    pub nn: [i32; 2],

    /// Precomputed gradient quantization LUT for `d` clamped to `[-t3, t3]`.
    /// Indexed by `(d.clamp(-t3, t3) + t3) as usize`.
    quant_lut: Vec<i8>,

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

        // Build compact gradient quantization LUT over [-t3, t3] (max ~1024 bytes for 16-bit).
        let lut_len = (2 * t3 + 1) as usize;
        let mut quant_lut = Vec::with_capacity(lut_len);
        for d in -t3..=t3 {
            let q = if d <= -t3 {
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
            };
            quant_lut.push(q);
        }

        Self {
            t3,
            max_val,
            near,
            range,
            qbpp,
            limit,
            reset,
            ctx: [entry; NUM_CONTEXTS],
            nn: [0, 0],
            quant_lut,
            j: J_TABLE,
            run_index: 0,
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

    #[inline(always)]
    pub fn quantize_gradient(&self, d: i32) -> i32 {
        let idx = (d.clamp(-self.t3, self.t3) + self.t3) as usize;
        i32::from(self.quant_lut[idx])
    }

    #[inline(always)]
    pub fn get_context_index(&self, d1: i32, d2: i32, d3: i32) -> (usize, i32) {
        let q1 = self.quantize_gradient(d1);
        let q2 = self.quantize_gradient(d2);
        let q3 = self.quantize_gradient(d3);

        let raw = q1 * 81 + q2 * 9 + q3;
        if raw < 0 {
            ((-raw) as usize, -1)
        } else {
            (raw as usize, 1)
        }
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

    #[inline(always)]
    pub fn update_stats_lossless(&mut self, q: usize, err_val: i32) {
        let reset = self.reset;
        let e = &mut self.ctx[q];
        e.b += err_val;
        e.a += err_val.abs();
        if e.n == reset {
            e.a >>= 1;
            e.b = if e.b >= 0 {
                e.b >> 1
            } else {
                -((1 - e.b) >> 1)
            };
            e.n >>= 1;
        }
        e.n += 1;
        if e.b <= -e.n {
            if e.c > MIN_C {
                e.c -= 1;
            }
            e.b += e.n;
            if e.b <= -e.n {
                e.b = -e.n + 1;
            }
        } else if e.b > 0 {
            if e.c < MAX_C {
                e.c += 1;
            }
            e.b -= e.n;
            if e.b > 0 {
                e.b = 0;
            }
        }
    }

    #[inline(always)]
    pub fn update_stats(&mut self, q: usize, err_val: i32) {
        let reset = self.reset;
        let step = 2 * self.near + 1;
        let e = &mut self.ctx[q];
        e.b += err_val * step;
        e.a += err_val.abs();
        if e.n == reset {
            e.a >>= 1;
            e.b = if e.b >= 0 {
                e.b >> 1
            } else {
                -((1 - e.b) >> 1)
            };
            e.n >>= 1;
        }
        e.n += 1;
        if e.b <= -e.n {
            if e.c > MIN_C {
                e.c -= 1;
            }
            e.b += e.n;
            if e.b <= -e.n {
                e.b = -e.n + 1;
            }
        } else if e.b > 0 {
            if e.c < MAX_C {
                e.c += 1;
            }
            e.b -= e.n;
            if e.b > 0 {
                e.b = 0;
            }
        }
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
