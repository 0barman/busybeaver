use super::{
    apply_jitter, build_delays, Backoff, DelayPlan, Jitter, RetryBuildError, RetryBuilder,
};
use std::error::Error;
use std::time::Duration;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}

#[test]
fn seeded_jitter_is_replayable() {
    let original = vec![Duration::from_secs(10); 4];
    let mut first = original.clone();
    let mut second = original;
    apply_jitter(&mut first, Jitter::seeded(7, 0.25), None).unwrap();
    apply_jitter(&mut second, Jitter::seeded(7, 0.25), None).unwrap();
    assert_eq!(first, second);
}

#[test]
fn jitter_is_clamped_to_an_exponential_cap() {
    let cap = Duration::from_secs(10);
    let mut delays = vec![cap; 8];
    apply_jitter(&mut delays, Jitter::seeded(9, 1.0), Some(cap)).unwrap();
    assert!(delays.into_iter().all(|delay| delay <= cap));
}

#[test]
fn compact_delay_plan_matches_dense_oracle_and_replays_independently() -> TestResult {
    let retry_count = 8;
    let cases = vec![
        (Backoff::None, None),
        (
            Backoff::Fixed(Duration::from_millis(17)),
            Some(Jitter::seeded(7, 0.25)),
        ),
        (
            Backoff::Explicit(
                (1..=retry_count)
                    .map(|value| Duration::from_millis(value as u64 * 3))
                    .collect(),
            ),
            Some(Jitter::seeded(11, 1.0)),
        ),
        (
            Backoff::Exponential {
                base: Duration::from_millis(5),
                multiplier: 2.0,
                cap: Duration::from_millis(40),
            },
            Some(Jitter::seeded(13, 0.5)),
        ),
    ];
    for (backoff, jitter) in cases {
        let cap = match &backoff {
            Backoff::Exponential { cap, .. } => Some(*cap),
            _ => None,
        };
        let mut dense = build_delays(backoff.clone(), retry_count)?;
        if let Some(jitter) = jitter {
            apply_jitter(&mut dense, jitter, cap)?;
        }
        let plan = DelayPlan::build(backoff, retry_count, jitter)?;
        let first = plan.iter().collect::<Result<Vec<_>, _>>()?;
        let second = plan.iter().collect::<Result<Vec<_>, _>>()?;
        if first != dense || second != dense {
            return Err(test_error(
                "compact retry delay plan differed from the dense oracle",
            ));
        }
    }
    Ok(())
}

#[test]
fn compact_delay_plan_preserves_validation_precedence_and_debug_shape() -> TestResult {
    let invalid = DelayPlan::build(
        Backoff::Exponential {
            base: Duration::from_secs(1),
            multiplier: f64::NAN,
            cap: Duration::from_secs(2),
        },
        2,
        Some(Jitter::seeded(1, -1.0)),
    );
    if !matches!(invalid, Err(RetryBuildError::InvalidBackoffMultiplier)) {
        return Err(test_error(
            "backoff validation no longer precedes jitter validation",
        ));
    }

    let spec = RetryBuilder::new(|_| async { Err::<(), _>("retry") })
        .max_attempts(3)
        .retry_all_errors()
        .fixed_delay(Duration::from_secs(1))
        .build()?;
    let debug = format!("{spec:?}");
    if !debug.contains("delays: [1s, 1s]") {
        return Err(test_error(format!(
            "RetrySpec Debug delay shape changed: {debug}"
        )));
    }
    Ok(())
}
