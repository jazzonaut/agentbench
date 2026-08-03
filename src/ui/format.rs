//! Number formatting shared by the screens.
//!
//! Collected here because the screens this replaced each formatted bytes inline, and had drifted to
//! different units for the same quantity: resident memory was MiB in one view and GiB in the other, so the
//! same process read as two different sizes depending on which screen you were looking at.

use std::time::Duration;

const MIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;

/// A percentage, one decimal place.
pub fn percent(value: f32) -> String {
    format!("{value:.1} %")
}

/// Bytes as GiB, one decimal place.
pub fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB)
}

/// Bytes as MiB, one decimal place.
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / MIB)
}

/// `used / total` in GiB, for a quantity with a known ceiling.
pub fn gib_of(used: u64, total: u64) -> String {
    format!("{:.1} / {:.1} GiB", used as f64 / GIB, total as f64 / GIB)
}

/// Elapsed time, one decimal place.
pub fn seconds(elapsed: Duration) -> String {
    format!("{:.1} s", elapsed.as_secs_f64())
}

/// `used / total` as a ratio, safe when `total` is zero.
///
/// Returns zero rather than `NaN` for an unknown total. A machine reporting no swap is ordinary, and its
/// meter should read empty instead of blank.
pub fn ratio(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_units_render_at_one_decimal_place() {
        assert_eq!(gib(1_073_741_824), "1.0 GiB");
        assert_eq!(mib(1_572_864), "1.5 MiB");
        assert_eq!(gib_of(1_073_741_824, 8_589_934_592), "1.0 / 8.0 GiB");
    }

    /// Values chosen to avoid a half-way case: the nearest `f32` to something like 42.05 sits just below
    /// it, so `{:.1}` rounds down and an assertion written from the decimal literal fails for a reason
    /// that has nothing to do with this function.
    #[test]
    fn a_percentage_and_a_duration_render_at_one_decimal_place() {
        assert_eq!(percent(42.44), "42.4 %");
        assert_eq!(percent(42.96), "43.0 %");
        assert_eq!(seconds(Duration::from_millis(1_240)), "1.2 s");
    }

    /// The machine with no swap, which would otherwise divide by zero.
    #[test]
    fn a_zero_total_yields_a_zero_ratio_rather_than_nan() {
        assert_eq!(ratio(0, 0), 0.0);
        assert_eq!(ratio(5, 0), 0.0);
        assert_eq!(ratio(2, 8), 0.25);
    }
}
