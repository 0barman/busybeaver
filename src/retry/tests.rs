use super::{apply_jitter, Jitter};
use std::time::Duration;

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
