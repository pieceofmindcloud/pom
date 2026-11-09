//! Payout cycles. Every node numbers cycles from the same epoch, so a
//! timestamp falls in the same cycle wherever it is read (spec 1).

/// 2026-01-01T00:00:00Z in milliseconds.
pub const METERING_EPOCH_MS: u64 = 1_767_225_600_000;

/// The cycle a timestamp falls in. A zero cycle length is treated as one
/// second so a broken setting never divides by zero.
pub fn cycle_of(ts_ms: u64, cycle_secs: u64) -> u64 {
    ts_ms.saturating_sub(METERING_EPOCH_MS) / (cycle_secs.max(1) * 1000)
}

/// `[start, end)` of a cycle in milliseconds.
pub fn cycle_bounds_ms(cycle: u64, cycle_secs: u64) -> (u64, u64) {
    let len = cycle_secs.max(1) * 1000;
    (METERING_EPOCH_MS + cycle * len, METERING_EPOCH_MS + (cycle + 1) * len)
}

/// The oldest cycle still kept when `current` is open and `retain` closed
/// cycles are kept besides it (spec 12). `retain` is at least 1.
pub fn oldest_kept(current: u64, retain: u32) -> u64 {
    current.saturating_sub(retain.max(1) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_numbers_agree_from_a_fixed_epoch() {
        let c = 7 * 86_400;
        assert_eq!(cycle_of(METERING_EPOCH_MS, c), 0);
        assert_eq!(cycle_of(METERING_EPOCH_MS + c * 1000 - 1, c), 0);
        assert_eq!(cycle_of(METERING_EPOCH_MS + c * 1000, c), 1);
        let (start, end) = cycle_bounds_ms(1, c);
        assert_eq!(end - start, c * 1000);
        assert_eq!(cycle_of(start, c), 1);
        assert_eq!(cycle_of(end, c), 2);
    }

    #[test]
    fn before_the_epoch_is_cycle_zero_and_zero_length_does_not_panic() {
        assert_eq!(cycle_of(0, 86_400), 0);
        assert_eq!(cycle_of(METERING_EPOCH_MS + 5_000, 0), 5);
    }

    #[test]
    fn retention_keeps_at_least_one_closed_cycle() {
        assert_eq!(oldest_kept(10, 2), 8);
        assert_eq!(oldest_kept(10, 0), 9);
        assert_eq!(oldest_kept(0, 3), 0);
    }
}
