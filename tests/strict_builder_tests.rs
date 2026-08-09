use busybeaver::{
    work, Beaver, FixedCountBuilder, PeriodicBuilder, RangeIntervalBuilder, TimeIntervalBuilder,
    ValidationError, WorkResult,
};
use std::time::Duration;

#[test]
fn strict_fixed_count_rejects_zero_attempts() {
    let error = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(0)
        .build_strict()
        .err()
        .expect("strict builder must reject zero attempts");
    assert_eq!(error, ValidationError::ZeroAttempts);
}

#[test]
fn strict_time_interval_rejects_empty_schedule() {
    let error = TimeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }))
        .intervals_millis([])
        .build_strict()
        .err()
        .expect("strict builder must reject an empty schedule");
    assert_eq!(error, ValidationError::EmptySchedule);
}

#[test]
fn strict_range_rejects_zero_attempts_and_reversed_ranges() {
    let zero = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 0)
        .build_strict()
        .err()
        .expect("strict builder must reject zero attempts");
    assert_eq!(zero, ValidationError::ZeroAttempts);

    let reversed = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 2)
        .add_range(1, 0, Duration::from_millis(1))
        .build_strict()
        .err()
        .expect("strict builder must reject reversed ranges");
    assert_eq!(reversed, ValidationError::InvalidRange { start: 1, end: 0 });

    let too_many = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 1)
        .add_range(0, 0, Duration::ZERO)
        .add_range(0, 0, Duration::ZERO)
        .build_strict()
        .err()
        .expect("strict builder must reject too many ranges");
    assert_eq!(
        too_many,
        ValidationError::TooManyRanges {
            total: 1,
            ranges_count: 2
        }
    );

    let overflow = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 1)
        .add_range(0, 0, Duration::MAX)
        .build_strict()
        .err()
        .expect("strict builder must reject millisecond overflow");
    assert_eq!(overflow, ValidationError::DurationOverflow);
}

#[test]
fn strict_periodic_rejects_zero_interval() {
    let error = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::ZERO)
        .build_strict()
        .err()
        .expect("strict builder must reject a zero interval");
    assert_eq!(error, ValidationError::ZeroInterval);
}

#[test]
fn strict_builders_do_not_change_legacy_zero_value_behavior() {
    assert!(
        FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
            .count(0)
            .build()
            .is_ok()
    );
    assert!(
        TimeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }))
            .intervals_millis([])
            .build()
            .is_ok()
    );
    assert!(
        RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 0)
            .build()
            .is_ok()
    );
    assert!(
        PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .interval(Duration::ZERO)
            .build()
            .is_ok()
    );
}

#[test]
fn strict_beaver_constructor_validates_capacity_and_runtime() {
    assert!(matches!(
        Beaver::try_new("zero", 0),
        Err(ValidationError::InvalidCapacity { capacity: 0, .. })
    ));
    assert!(matches!(
        Beaver::try_new("outside-runtime", 1),
        Err(ValidationError::RuntimeUnavailable)
    ));
}

#[tokio::test]
async fn strict_beaver_constructor_binds_current_runtime() {
    let beaver = Beaver::try_new("strict", 1).expect("runtime is available");
    beaver.destroy().await.expect("destroy");
}
