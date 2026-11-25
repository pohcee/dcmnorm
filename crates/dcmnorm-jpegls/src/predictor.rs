//! LOCO-I Median Edge Detection (MED) predictor.
//!
//! The predictor selects between three candidates based on the local
//! gradient pattern, effectively detecting vertical and horizontal edges
//! and choosing the prediction that follows the dominant edge direction.

/// Predict the current sample using the MED (Median Edge Detection) rule.
///
/// - `ra`: left neighbour (a)
/// - `rb`: above neighbour (b)
/// - `rc`: above-left neighbour (c)
///
/// Returns the predicted value before bias correction and clamping.
#[inline]
pub(crate) fn predict_med(ra: i32, rb: i32, rc: i32) -> i32 {
    if rc >= ra.max(rb) {
        // Vertical edge detected -- predict the smaller of a, b.
        ra.min(rb)
    } else if rc <= ra.min(rb) {
        // Horizontal edge detected -- predict the larger of a, b.
        ra.max(rb)
    } else {
        // No strong edge -- use the plane predictor.
        ra + rb - rc
    }
}

/// Clamp `val` to the inclusive range `[lo, hi]`.
#[inline]
pub(crate) fn clamp(val: i32, lo: i32, hi: i32) -> i32 {
    if val < lo {
        lo
    } else if val > hi {
        hi
    } else {
        val
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn med_no_edge() {
        // rc not extreme -- plane predictor: a + b - c = 10 + 20 - 15 = 15
        assert_eq!(predict_med(10, 20, 15), 15);
    }

    #[test]
    fn med_vertical_edge() {
        // rc >= max(a, b) => min(a, b)
        // a=5, b=3, c=10 => c >= 5 => min(5,3) = 3
        assert_eq!(predict_med(5, 3, 10), 3);
    }

    #[test]
    fn med_horizontal_edge() {
        // rc <= min(a, b) => max(a, b)
        // a=5, b=3, c=1 => c <= 3 => max(5,3) = 5
        assert_eq!(predict_med(5, 3, 1), 5);
    }

    #[test]
    fn med_all_equal() {
        assert_eq!(predict_med(100, 100, 100), 100);
    }

    #[test]
    fn med_zero() {
        assert_eq!(predict_med(0, 0, 0), 0);
    }

    #[test]
    fn med_large_values() {
        // 16-bit range
        let a = 60000;
        let b = 50000;
        let c = 55000;
        // plane: 60000 + 50000 - 55000 = 55000
        assert_eq!(predict_med(a, b, c), 55000);
    }

    #[test]
    fn clamp_within_range() {
        assert_eq!(clamp(50, 0, 255), 50);
    }

    #[test]
    fn clamp_below_range() {
        assert_eq!(clamp(-10, 0, 255), 0);
    }

    #[test]
    fn clamp_above_range() {
        assert_eq!(clamp(300, 0, 255), 255);
    }

    #[test]
    fn clamp_at_boundaries() {
        assert_eq!(clamp(0, 0, 255), 0);
        assert_eq!(clamp(255, 0, 255), 255);
    }
}
