use super::duration_from_nanos;
use std::time::Duration;

#[test]
fn nanosecond_conversion_preserves_subseconds_and_rejects_overflow() {
    let converted = duration_from_nanos(1_000_000_001);
    assert_eq!(converted, Some(Duration::new(1, 1)));

    let overflowing_seconds = u128::from(u64::MAX)
        .saturating_add(1)
        .saturating_mul(1_000_000_000);
    assert_eq!(duration_from_nanos(overflowing_seconds), None);
}
