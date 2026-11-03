//! Money rules (NFR-30). All amounts are whole minor units (kobo, cents) in `i64`; intermediate
//! products use `i128` so they cannot overflow. There is no floating point anywhere.
//!
//! Rounding: tax on a line is `line_subtotal * rate_bps / 10_000` rounded half up to the nearest minor
//! unit, computed per line and then summed, so a receipt's lines always add up to its total.

/// Tax on `base` at `rate_bps` basis points (750 = 7.5%), rounded half up.
pub fn tax_minor(base: i64, rate_bps: i32) -> i64 {
    ((base as i128 * rate_bps as i128 + 5_000) / 10_000) as i64
}

/// `total * part / whole`, rounded half up. Used to refund part of a line: the refund for units
/// `before+1..=after` is `share(total, after, qty) - share(total, before, qty)`, so partial refunds over
/// time add up to exactly the line total with no drift.
pub fn share(total: i64, part: i64, whole: i64) -> i64 {
    if whole == 0 {
        return 0;
    }
    ((total as i128 * part as i128 * 2 + whole as i128) / (2 * whole as i128)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tax_rounds_half_up() {
        assert_eq!(tax_minor(10_000, 750), 750); // 7.5% of 100.00
        assert_eq!(tax_minor(1_999, 750), 150); // 149.925 -> 150
        assert_eq!(tax_minor(1_000, 750), 75);
        assert_eq!(tax_minor(66, 750), 5); // 4.95 -> 5
        assert_eq!(tax_minor(6, 750), 0); // 0.45 -> 0
        assert_eq!(tax_minor(7, 750), 1); // 0.525 -> 1
        assert_eq!(tax_minor(12_345, 0), 0);
    }

    #[test]
    fn partial_refunds_add_up_exactly() {
        let total = 1_001; // 3 units
        let parts: i64 = (1..=3).map(|n| share(total, n, 3) - share(total, n - 1, 3)).sum();
        assert_eq!(parts, total);
        assert_eq!(share(total, 3, 3), total);
        assert_eq!(share(total, 0, 3), 0);
    }
}
