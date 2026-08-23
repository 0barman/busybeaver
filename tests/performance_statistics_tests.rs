#[path = "../benches/support/statistics.rs"]
mod statistics;

use std::error::Error;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}

#[test]
fn percentile_uses_nearest_rank_for_tail_latency() -> TestResult {
    let two = [10_u128, 20];
    if statistics::percentile(&two, 95) != 20 || statistics::percentile(&two, 99) != 20 {
        return Err(test_error(
            "two-sample tail percentile did not select the maximum",
        ));
    }

    let five = [1_u128, 2, 3, 4, 5];
    if statistics::percentile(&five, 50) != 3
        || statistics::percentile(&five, 95) != 5
        || statistics::percentile(&five, 99) != 5
    {
        return Err(test_error(
            "five-sample nearest-rank percentiles were incorrect",
        ));
    }

    let fifty = (1_u128..=50).collect::<Vec<_>>();
    if statistics::percentile(&fifty, 95) != 48 || statistics::percentile(&fifty, 99) != 50 {
        return Err(test_error("fifty-sample tail percentiles were incorrect"));
    }

    if statistics::percentile(&[], 99) != 0 {
        return Err(test_error("empty percentile input did not return zero"));
    }
    Ok(())
}

#[test]
fn median_absolute_deviation_uses_the_same_rank_definition() -> TestResult {
    let samples = [1_u128, 2, 3, 100, 101];
    let median = statistics::percentile(&samples, 50);
    let deviation = statistics::median_absolute_deviation(&samples, median);
    if median != 3 || deviation != 2 {
        return Err(test_error(
            "median absolute deviation was calculated incorrectly",
        ));
    }
    Ok(())
}
