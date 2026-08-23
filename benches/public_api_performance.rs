use busybeaver::{
    work, AttemptContext, Backoff, Beaver, CancelReason, EventRecvError, EventStream, LaneConfig,
    OrderingKey, RangeIntervalBuilder, ResourceLimits, RetryBuilder, SpawnOptions, TaskSpec,
    WorkResult,
};
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "support/statistics.rs"]
mod statistics;

use statistics::{median_absolute_deviation, percentile};

type BenchResult<T> = Result<T, Box<dyn std::error::Error>>;

struct BenchConfig {
    profile: &'static str,
    case: &'static str,
    operations: usize,
    samples: usize,
    minimum_warmup_samples: usize,
    warmup_duration: Duration,
    measurement_duration: Duration,
    runtime_threads: usize,
    subscriber_mode: &'static str,
    ordering_keys: &'static str,
}

fn bench_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::other(message.into()))
}

fn env_usize(name: &str, default: usize) -> BenchResult<usize> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|error| bench_error(format!("invalid {name} value {value}: {error}"))),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(bench_error(format!("failed to read {name}: {error}"))),
    }
}

fn config() -> BenchResult<BenchConfig> {
    let requested_profile = match env::var("BB_BENCH_PROFILE") {
        Ok(profile) => profile,
        Err(env::VarError::NotPresent) => "smoke".to_string(),
        Err(error) => {
            return Err(bench_error(format!(
                "failed to read BB_BENCH_PROFILE: {error}"
            )))
        }
    };
    let (
        profile,
        default_operations,
        default_samples,
        default_warmup_samples,
        default_warmup_seconds,
        default_measurement_seconds,
    ) = match requested_profile.as_str() {
        "smoke" => ("smoke", 64, 2, 1, 0, 0),
        "quick" => ("quick", 4_096, 50, 5, 3, 10),
        "full" => ("full", 4_096, 100, 10, 5, 20),
        _ => {
            return Err(bench_error(format!(
                "unsupported BB_BENCH_PROFILE: {requested_profile}"
            )))
        }
    };
    let requested_case = match env::var("BB_BENCH_CASE") {
        Ok(case) => case,
        Err(env::VarError::NotPresent) => "lane_admission".to_string(),
        Err(error) => {
            return Err(bench_error(format!(
                "failed to read BB_BENCH_CASE: {error}"
            )))
        }
    };
    let case = match requested_case.as_str() {
        "lane_admission" => "lane_admission",
        "lane_drain" => "lane_drain",
        "lane_hot_key_blocked" => "lane_hot_key_blocked",
        "lane_waiters" => "lane_waiters",
        "event_spawn_join" => "event_spawn_join",
        "retry_attempts" => "retry_attempts",
        "retry_build" => "retry_build",
        "range_build" => "range_build",
        _ => {
            return Err(bench_error(format!(
                "unsupported BB_BENCH_CASE: {requested_case}"
            )))
        }
    };
    let operations = env_usize("BB_BENCH_OPERATIONS", default_operations)?;
    if operations == 0 {
        return Err(bench_error("BB_BENCH_OPERATIONS must be greater than zero"));
    }
    let samples = env_usize("BB_BENCH_SAMPLES", default_samples)?;
    if samples == 0 {
        return Err(bench_error("BB_BENCH_SAMPLES must be greater than zero"));
    }
    let runtime_threads = env_usize("BB_BENCH_RUNTIME_THREADS", 1)?;
    if runtime_threads == 0 {
        return Err(bench_error(
            "BB_BENCH_RUNTIME_THREADS must be greater than zero",
        ));
    }
    let requested_subscriber_mode = match env::var("BB_BENCH_SUBSCRIBER_MODE") {
        Ok(mode) => mode,
        Err(env::VarError::NotPresent) => match env_usize("BB_BENCH_SUBSCRIBERS", 0)? {
            0 => "none".to_string(),
            1 => "lagging".to_string(),
            count => {
                return Err(bench_error(format!(
                    "BB_BENCH_SUBSCRIBERS={count} is unsupported; use BB_BENCH_SUBSCRIBER_MODE"
                )))
            }
        },
        Err(error) => {
            return Err(bench_error(format!(
                "failed to read BB_BENCH_SUBSCRIBER_MODE: {error}"
            )))
        }
    };
    let subscriber_mode = match requested_subscriber_mode.as_str() {
        "none" => "none",
        "fast" => "fast",
        "lagging" => "lagging",
        _ => {
            return Err(bench_error(format!(
                "unsupported BB_BENCH_SUBSCRIBER_MODE: {requested_subscriber_mode}"
            )))
        }
    };
    let requested_keys = match env::var("BB_BENCH_ORDERING_KEYS") {
        Ok(keys) => keys,
        Err(env::VarError::NotPresent) => "none".to_string(),
        Err(error) => {
            return Err(bench_error(format!(
                "failed to read BB_BENCH_ORDERING_KEYS: {error}"
            )))
        }
    };
    let ordering_keys = match requested_keys.as_str() {
        "none" => "none",
        "hot" => "hot",
        "unique" => "unique",
        _ => {
            return Err(bench_error(format!(
                "unsupported BB_BENCH_ORDERING_KEYS: {requested_keys}"
            )))
        }
    };
    Ok(BenchConfig {
        profile,
        case,
        operations,
        samples,
        minimum_warmup_samples: env_usize("BB_BENCH_WARMUP_SAMPLES", default_warmup_samples)?,
        warmup_duration: Duration::from_secs(u64::try_from(env_usize(
            "BB_BENCH_WARMUP_SECONDS",
            default_warmup_seconds,
        )?)?),
        measurement_duration: Duration::from_secs(u64::try_from(env_usize(
            "BB_BENCH_MEASUREMENT_SECONDS",
            default_measurement_seconds,
        )?)?),
        runtime_threads,
        subscriber_mode,
        ordering_keys,
    })
}

async fn wait_until_running(lane: &busybeaver::Lane) -> BenchResult<()> {
    for _ in 0..100_000_usize {
        if lane.stats().running == 1 {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
    Err(bench_error("lane blocker did not start"))
}

fn benchmark_limits(operations: usize) -> ResourceLimits {
    ResourceLimits {
        max_active_executions: operations.saturating_add(64),
        event_capacity: 1_024,
        terminal_history_capacity: 0,
        ..ResourceLimits::default()
    }
}

struct BenchSubscriber {
    _lagging: Option<EventStream>,
    fast_worker: Option<tokio::task::JoinHandle<()>>,
}

impl BenchSubscriber {
    fn new(beaver: &Beaver, mode: &str) -> BenchResult<Self> {
        match mode {
            "none" => Ok(Self {
                _lagging: None,
                fast_worker: None,
            }),
            "lagging" => Ok(Self {
                _lagging: Some(beaver.subscribe_events()?),
                fast_worker: None,
            }),
            "fast" => {
                let mut events = beaver.subscribe_events()?;
                let fast_worker = tokio::spawn(async move {
                    while matches!(
                        events.recv().await,
                        Ok(_) | Err(EventRecvError::Lagged { .. })
                    ) {}
                });
                Ok(Self {
                    _lagging: None,
                    fast_worker: Some(fast_worker),
                })
            }
            _ => Err(bench_error("validated subscriber mode became unavailable")),
        }
    }

    async fn stop(mut self) {
        if let Some(worker) = self.fast_worker.take() {
            worker.abort();
            let _ = worker.await;
        }
    }
}

async fn lane_batch(
    operations: usize,
    measure_drain: bool,
    ordering_keys: &str,
) -> BenchResult<Duration> {
    let beaver = Beaver::builder("bench-lane", 8)
        .resource_limits(benchmark_limits(operations))
        .build()?;
    let lane = beaver.create_lane(
        LaneConfig::new("bench-lane")
            .capacity(operations.saturating_add(1))
            .concurrency(1),
    )?;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let blocker_release = Arc::clone(&release);
    let mut blocker = lane.try_spawn(TaskSpec::new(move |_| {
        let release = Arc::clone(&blocker_release);
        async move {
            match release.acquire_owned().await {
                Ok(_permit) => Ok::<(), String>(()),
                Err(error) => Err(error.to_string()),
            }
        }
    }))?;
    wait_until_running(&lane).await?;

    let task = TaskSpec::new(|_| async { Ok::<(), String>(()) });
    let hot_key = OrderingKey::new("bench-hot-key")?;
    let mut options = Vec::with_capacity(operations);
    for index in 0..operations {
        let option = match ordering_keys {
            "none" => SpawnOptions::default(),
            "hot" => SpawnOptions::default().ordering_key(hot_key.clone()),
            "unique" => SpawnOptions::default()
                .ordering_key(OrderingKey::new(format!("bench-key-{index:020}"))?),
            _ => {
                return Err(bench_error(
                    "validated ordering-key mode became unavailable",
                ))
            }
        };
        options.push(option);
    }
    let admission_started = Instant::now();
    let mut handles = Vec::with_capacity(operations);
    for option in options {
        handles.push(lane.try_spawn_with_options(task.clone(), option)?);
    }
    let admission_elapsed = admission_started.elapsed();

    let drain_started = Instant::now();
    release.add_permits(1);
    let _ = blocker.join().await?;
    for handle in &mut handles {
        let _ = handle.join().await?;
    }
    let drain_elapsed = drain_started.elapsed();
    beaver.destroy().await?;
    if measure_drain {
        Ok(drain_elapsed)
    } else {
        Ok(admission_elapsed)
    }
}

async fn lane_waiters(operations: usize) -> BenchResult<Duration> {
    let limits = ResourceLimits {
        max_waiting_producers_per_lane: operations.saturating_add(1),
        ..benchmark_limits(operations.saturating_add(2))
    };
    let beaver = Beaver::builder("bench-waiters", 8)
        .resource_limits(limits)
        .build()?;
    let lane = beaver.create_lane(LaneConfig::new("bench-waiters").capacity(1).concurrency(1))?;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let blocker_release = Arc::clone(&release);
    let mut blocker = lane.try_spawn(TaskSpec::new(move |_| {
        let release = Arc::clone(&blocker_release);
        async move {
            match release.acquire_owned().await {
                Ok(_permit) => Ok::<(), String>(()),
                Err(error) => Err(error.to_string()),
            }
        }
    }))?;
    wait_until_running(&lane).await?;
    let pending_task = TaskSpec::new(|context| async move {
        context.cancelled().await;
        Ok::<(), String>(())
    });
    let mut queued = lane.try_spawn(pending_task.clone())?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut workers = Vec::with_capacity(operations);
    for _ in 0..operations {
        let waiting_lane = lane.clone();
        let waiting_task = pending_task.clone();
        let waiting_sender = sender.clone();
        workers.push(tokio::spawn(async move {
            let result = waiting_lane.spawn(waiting_task).await;
            let _ = waiting_sender.send(result);
        }));
    }
    drop(sender);
    for _ in 0..100_000_usize {
        if lane.stats().waiting_producers == operations {
            break;
        }
        tokio::task::yield_now().await;
    }
    if lane.stats().waiting_producers != operations {
        return Err(bench_error(format!(
            "lane registered {} of {operations} benchmark waiters",
            lane.stats().waiting_producers
        )));
    }

    let started = Instant::now();
    queued.control().cancel(CancelReason::UserRequested);
    let _ = queued.join().await?;
    for _ in 0..operations {
        let received = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .map_err(|_| bench_error("lane waiter benchmark timed out"))?;
        let Some(result) = received else {
            return Err(bench_error("lane waiter result channel closed early"));
        };
        let mut admitted = result?;
        admitted.control().cancel(CancelReason::UserRequested);
        let _ = admitted.join().await?;
    }
    let elapsed = started.elapsed();

    release.add_permits(1);
    let _ = blocker.join().await?;
    for worker in workers {
        if let Err(error) = worker.await {
            return Err(bench_error(format!(
                "lane waiter benchmark worker failed: {error}"
            )));
        }
    }
    beaver.destroy().await?;
    Ok(elapsed)
}

async fn lane_hot_key_blocked(operations: usize) -> BenchResult<Duration> {
    let beaver = Beaver::builder("bench-hot-key-blocked", 8)
        .resource_limits(benchmark_limits(operations.saturating_add(1)))
        .build()?;
    let lane = beaver.create_lane(
        LaneConfig::new("bench-hot-key-blocked")
            .capacity(operations.saturating_add(1))
            .concurrency(operations.saturating_add(1)),
    )?;
    let hot_key = OrderingKey::new("bench-hot-key-blocked")?;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let blocker_release = Arc::clone(&release);
    let mut blocker = lane.try_spawn_with_options(
        TaskSpec::new(move |_| {
            let release = Arc::clone(&blocker_release);
            async move {
                match release.acquire_owned().await {
                    Ok(_permit) => Ok::<(), String>(()),
                    Err(error) => Err(error.to_string()),
                }
            }
        }),
        SpawnOptions::default().ordering_key(hot_key.clone()),
    )?;
    wait_until_running(&lane).await?;

    let task = TaskSpec::new(|context| async move {
        context.cancelled().await;
        Ok::<(), String>(())
    });
    let started = Instant::now();
    let mut handles = Vec::with_capacity(operations);
    for _ in 0..operations {
        handles.push(lane.try_spawn_with_options(
            task.clone(),
            SpawnOptions::default().ordering_key(hot_key.clone()),
        )?);
    }
    let elapsed = started.elapsed();

    for handle in &handles {
        handle.control().cancel(CancelReason::UserRequested);
    }
    for handle in &mut handles {
        let _ = handle.join().await?;
    }
    release.add_permits(1);
    let _ = blocker.join().await?;
    beaver.destroy().await?;
    Ok(elapsed)
}

async fn event_spawn_join(operations: usize, subscriber_mode: &str) -> BenchResult<Duration> {
    let beaver = Beaver::builder("bench-events", 8)
        .resource_limits(benchmark_limits(operations))
        .build()?;
    let subscriber = BenchSubscriber::new(&beaver, subscriber_mode)?;
    let task = TaskSpec::new(|_| async { Ok::<(), String>(()) });
    let started = Instant::now();
    let mut handles = Vec::with_capacity(operations);
    for _ in 0..operations {
        handles.push(beaver.spawn(task.clone())?);
    }
    for handle in &mut handles {
        let _ = handle.join().await?;
    }
    let elapsed = started.elapsed();
    subscriber.stop().await;
    beaver.destroy().await?;
    Ok(elapsed)
}

async fn retry_attempts(operations: usize, subscriber_mode: &str) -> BenchResult<Duration> {
    let attempts = u32::try_from(operations)
        .map_err(|_| bench_error("retry_attempts exceed the public u32 attempt range"))?;
    let beaver = Beaver::builder("bench-retry-attempts", 8)
        .resource_limits(benchmark_limits(1))
        .build()?;
    let subscriber = BenchSubscriber::new(&beaver, subscriber_mode)?;
    let lane = beaver.create_lane(LaneConfig::new("bench-retry-attempts"))?;
    let spec = RetryBuilder::new(|_: AttemptContext| async { Err::<(), u8>(1) })
        .max_attempts(attempts)
        .backoff(Backoff::None)
        .retry_all_errors()
        .build()?;
    let started = Instant::now();
    let mut handle = lane.try_spawn_retry(spec)?;
    let _ = handle.join().await?;
    let elapsed = started.elapsed();
    subscriber.stop().await;
    beaver.destroy().await?;
    Ok(elapsed)
}

fn retry_build(operations: usize) -> BenchResult<Duration> {
    let attempts = u32::try_from(operations)
        .map_err(|_| bench_error("retry_build operations exceed the public u32 attempt range"))?;
    let started = Instant::now();
    let spec = RetryBuilder::new(|_: AttemptContext| async { Err::<(), u8>(1) })
        .max_attempts(attempts)
        .backoff(Backoff::fixed(Duration::ZERO))
        .retry_all_errors()
        .build()?;
    std::hint::black_box(spec);
    Ok(started.elapsed())
}

fn range_build(operations: usize) -> BenchResult<Duration> {
    let attempts = u32::try_from(operations)
        .map_err(|_| bench_error("range_build operations exceed the public u32 attempt range"))?;
    let final_attempt = attempts.saturating_sub(1);
    let started = Instant::now();
    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), attempts)
        .add_range(final_attempt, final_attempt, Duration::from_millis(1))
        .build()?;
    std::hint::black_box(task);
    Ok(started.elapsed())
}

async fn run_sample(config: &BenchConfig) -> BenchResult<Duration> {
    match config.case {
        "lane_admission" => lane_batch(config.operations, false, config.ordering_keys).await,
        "lane_drain" => lane_batch(config.operations, true, config.ordering_keys).await,
        "lane_hot_key_blocked" => lane_hot_key_blocked(config.operations).await,
        "lane_waiters" => lane_waiters(config.operations).await,
        "event_spawn_join" => event_spawn_join(config.operations, config.subscriber_mode).await,
        "retry_attempts" => retry_attempts(config.operations, config.subscriber_mode).await,
        "retry_build" => retry_build(config.operations),
        "range_build" => range_build(config.operations),
        _ => Err(bench_error("validated benchmark case became unavailable")),
    }
}

async fn run(config: BenchConfig) -> BenchResult<()> {
    let warmup_started = Instant::now();
    let mut warmup_count = 0usize;
    while warmup_count < config.minimum_warmup_samples
        || warmup_started.elapsed() < config.warmup_duration
    {
        let _ = run_sample(&config).await?;
        warmup_count = warmup_count.saturating_add(1);
    }
    let mut samples = Vec::with_capacity(config.samples);
    let measurement_started = Instant::now();
    for index in 0..config.samples {
        let elapsed = run_sample(&config).await?;
        samples.push(elapsed.as_nanos());
        let remaining_samples = config.samples.saturating_sub(index.saturating_add(1));
        if remaining_samples > 0 {
            let remaining_window = config
                .measurement_duration
                .saturating_sub(measurement_started.elapsed());
            let divisor = u32::try_from(remaining_samples)
                .map_err(|_| bench_error("benchmark sample count exceeds u32 pacing range"))?;
            std::thread::sleep(remaining_window / divisor);
        }
    }
    let measurement_elapsed = measurement_started.elapsed();
    samples.sort_unstable();
    let median = percentile(&samples, 50);
    let p95 = percentile(&samples, 95);
    let p99 = percentile(&samples, 99);
    let mad = median_absolute_deviation(&samples, median);
    let per_operation = median.saturating_div(config.operations as u128);
    let raw_samples = samples
        .iter()
        .map(u128::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let subscribers = usize::from(config.subscriber_mode != "none");
    println!(
        "{{\"profile\":\"{}\",\"case\":\"{}\",\"operations\":{},\"samples\":{},\"warmup_samples\":{},\"warmup_ms\":{},\"measurement_ms\":{},\"runtime_threads\":{},\"subscribers\":{},\"subscriber_mode\":\"{}\",\"ordering_keys\":\"{}\",\"median_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"mad_ns\":{},\"median_ns_per_operation\":{},\"raw_ns\":[{}]}}",
        config.profile,
        config.case,
        config.operations,
        config.samples,
        warmup_count,
        warmup_started.elapsed().as_millis(),
        measurement_elapsed.as_millis(),
        config.runtime_threads,
        subscribers,
        config.subscriber_mode,
        config.ordering_keys,
        median,
        p95,
        p99,
        mad,
        per_operation,
        raw_samples
    );
    Ok(())
}

fn main() -> BenchResult<()> {
    let config = config()?;
    let mut runtime_builder = if config.runtime_threads == 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(config.runtime_threads);
        builder
    };
    let runtime = runtime_builder.enable_all().build()?;
    runtime.block_on(run(config))
}
