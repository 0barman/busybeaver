pub(crate) fn percentile(sorted: &[u128], percentile: usize) -> u128 {
    let Some(last_index) = sorted.len().checked_sub(1) else {
        return 0;
    };
    let bounded = percentile.min(100);
    let rank = sorted
        .len()
        .saturating_mul(bounded)
        .saturating_add(99)
        .saturating_div(100)
        .max(1);
    let index = rank.saturating_sub(1).min(last_index);
    sorted.get(index).copied().map_or(0, |value| value)
}

pub(crate) fn median_absolute_deviation(sorted: &[u128], median: u128) -> u128 {
    let mut deviations = sorted
        .iter()
        .map(|sample| sample.abs_diff(median))
        .collect::<Vec<_>>();
    deviations.sort_unstable();
    percentile(&deviations, 50)
}
