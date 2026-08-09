use busybeaver::{
    listener, work, Beaver, FixedCountBuilder, Job, LaneConfig, Scheduler, TaskTerminal, WorkResult,
};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

const BUILD_ITERATIONS: u32 = 100_000;
const EXECUTION_ITERATIONS: u32 = 1_000;
const BURST_ITERATIONS: usize = 10_000;

fn legacy_task_build() {
    let started = Instant::now();
    for _ in 0..BUILD_ITERATIONS {
        black_box(
            FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
                .count(black_box(3))
                .build()
                .expect("benchmark task must build"),
        );
    }
    println!(
        "legacy/fixed_count_build: {:?} for {BUILD_ITERATIONS} iterations",
        started.elapsed()
    );
}

fn legacy_enqueue_and_shutdown() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("benchmark runtime must build");

    let started = Instant::now();
    runtime.block_on(async {
        for _ in 0..EXECUTION_ITERATIONS {
            let beaver = Beaver::new("benchmark", 1);
            let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
                .count(1)
                .build()
                .expect("benchmark task must build");
            beaver
                .enqueue(task)
                .await
                .expect("benchmark enqueue must succeed");
            beaver
                .destroy()
                .await
                .expect("benchmark shutdown must succeed");
        }
    });
    println!(
        "legacy/enqueue_and_shutdown: {:?} for {EXECUTION_ITERATIONS} iterations",
        started.elapsed()
    );
}

fn legacy_burst() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("benchmark runtime must build");
    let started = Instant::now();
    runtime.block_on(async {
        let beaver = Beaver::new("benchmark", BURST_ITERATIONS);
        let completed = Arc::new(AtomicUsize::new(0));
        for _ in 0..BURST_ITERATIONS {
            let completed = Arc::clone(&completed);
            let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
                .listener(listener(
                    move || {
                        completed.fetch_add(1, Ordering::Relaxed);
                    },
                    || {},
                ))
                .build()
                .expect("benchmark task must build");
            beaver
                .enqueue(task)
                .await
                .expect("benchmark enqueue must succeed");
        }
        while completed.load(Ordering::Relaxed) != BURST_ITERATIONS {
            tokio::task::yield_now().await;
        }
        beaver
            .destroy()
            .await
            .expect("benchmark shutdown must succeed");
    });
    println!(
        "legacy/burst: {:?} for {BURST_ITERATIONS} completed tasks",
        started.elapsed()
    );
}

fn scheduler_burst() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("benchmark runtime must build");
    let started = Instant::now();
    runtime.block_on(async {
        let scheduler = Scheduler::builder()
            .default_lane(LaneConfig::new(BURST_ITERATIONS, 1).unwrap())
            .build()
            .unwrap();
        let mut handles = Vec::with_capacity(BURST_ITERATIONS);
        for _ in 0..BURST_ITERATIONS {
            handles.push(
                scheduler
                    .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
                    .await
                    .unwrap(),
            );
        }
        for handle in handles {
            assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
        }
        assert!(scheduler.shutdown().await.is_complete());
    });
    println!(
        "scheduler/burst: {:?} for {BURST_ITERATIONS} completed tasks",
        started.elapsed()
    );
}

fn main() {
    legacy_task_build();
    legacy_enqueue_and_shutdown();
    legacy_burst();
    scheduler_burst();
}
