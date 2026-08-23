# BusyBeaver 完整开发者文档

[English guide](GUIDE_en.md) · [README](../README.md) · [变更记录](../CHANGELOG.md) ·
[Rust API 参考](https://docs.rs/busybeaver)

本文是面向应用开发者、维护者和发布工程师的唯一中文主文档，完整说明公开执行模型、生命周期
契约、资源边界、诊断方式和仓库质量门禁。版本差异统一记录在变更记录中，不再维护独立迁移
文档或某个小版本专属介绍。

## 环境要求与安装

BusyBeaver 支持 Rust 1.89 及以上版本、原生 Tokio runtime 和 `Send + 'static` future。Rust
1.89 是最低支持 Rust 版本（MSRV）；仓库开发工具链由 [`rust-toolchain.toml`](../rust-toolchain.toml)
固定为 Rust 1.89.0，从本仓库中执行命令时 rustup 会自动选择该版本。应在 Tokio runtime 内构造
executor，或显式传入 `tokio::runtime::Handle`。使用 sleep、deadline、retry、recurring schedule、
admission timeout 或 shutdown grace period 时必须启用 Tokio time。

```toml
[dependencies]
busybeaver = "0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
```

BusyBeaver 没有默认 feature。可选 `tracing` feature 会向 tracing 生态输出有界、脱敏的生命周期
字段。

```toml
busybeaver = { version = "0.3", features = ["tracing"] }
```

```rust,no_run
use busybeaver::{Beaver, BeaverError, ResourceLimits};

fn build_executor() -> Result<Beaver, BeaverError> {
    Beaver::builder("default", 256)
        .resource_limits(ResourceLimits::default())
        .build()
}
```

当前调用方不在 Tokio 中时，使用 `Beaver::try_new_with_handle` 或
`BeaverBuilder::runtime_handle`。构造过程可失败：非法 capacity、非法资源限制和 runtime 缺失
都会返回 `BeaverError`，不会触发 panic。

## 选择执行模型

| 需求 | 主要 API |
| --- | --- |
| 单个 typed 异步操作 | `TaskSpec<T, E>` 与 `Beaver::spawn` |
| 有界队列、并发、优先级与有序执行 | `Lane` 与 `LaneConfig` |
| 保留最后业务错误的 retry | `RetryBuilder` |
| 固定、分段或动态 recurring 工作 | `RecurringBuilder` 与 `Schedule` |
| newest-wins 替换 | `TaskSlot` |
| session、页面或请求 generation | `Scope` |
| readiness、health、restart 与 shutdown hook | `ServiceBuilder` |
| 兼容 count/time/range/periodic 工作 | legacy builder 与 `Beaver::enqueue` |
| 有界 event、snapshot 与 history | `subscribe_events` 与 `snapshot` |
| 可共享、可报告 shutdown | `Beaver::shutdown` |

## Typed execution、身份与结果

`TaskSpecId` 标识可复用定义；每次成功接纳都会获得唯一 `ExecutionId`、独立状态、取消单元、
结果单元和 tracked-child 集合。

```rust,no_run
use busybeaver::{Beaver, CancelReason, TaskExit, TaskSpec};
use std::io;
use std::time::Duration;

async fn typed_task(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let spec = TaskSpec::new(|context| async move {
        context.sleep(Duration::from_millis(10)).await?;
        Ok::<_, busybeaver::Cancelled>(42_u8)
    });
    let mut handle = beaver.spawn(spec)?;
    let control = handle.control();
    if control.state().is_terminal() {
        return Err(io::Error::other("new execution was already terminal").into());
    }
    control.cancel(CancelReason::Other("application stop".to_string()));
    match handle.join().await? {
        TaskExit::Cancelled { .. } | TaskExit::Completed(_) => Ok(()),
        _ => Err(io::Error::other("unexpected terminal outcome").into()),
    }
}
```

`TaskHandle::wait` 可重复调用并返回脱敏的 `TaskExitSummary`；`TaskHandle::join` 只成功取走一次
typed `TaskExit<T, E>`；`TaskControlHandle` 可 clone 且不携带结果类型。drop 普通 handle 只会
detach；`cancel_on_drop` 才会构造主动取消 guard。drop 尚未完成的 wait/join future 不会消费
结果。

每个 accepted execution 恰好进入一个终态：`Completed`、`Failed`、`Cancelled`、`Aborted`、
`Panicked` 或 `ExecutorStopped`。cancel 与 deadline 采用 first-wins。只有配置
`AbortPolicy::Allowed` 的工作允许 forced cancellation，而且只能 drop 被追踪的 async future；
它不能抢占 CPU loop、blocking syscall、`spawn_blocking`、OS/FFI thread、未追踪 Tokio task 或
远端副作用。

`TaskSelector` 与 `Beaver::cancel_snapshot` 提供线性化的批量取消快照，`BatchCancelReport`
记录每个选中 execution 的结果。一个 tracked execution 等待另一个 execution 时使用
`wait_checked`；直接 self/ancestor join 返回 `ExecutionWaitError::WouldJoin`。

### 结构化 child

`WorkContext::spawn_child` 与 `spawn_child_future` 创建 tracked child。parent 终态清理会先关闭
child admission，再取消并 join 剩余 child。retry 在进入下一 attempt 前也会清理本 attempt 的
child。应用自行创建的 Tokio task 不受追踪，生命周期仍由调用方负责。

## Lane、过载、优先级与 ordering key

queue capacity 只计算 queued entry；running concurrency 是独立限制。queued entry 转为 running
时会立即归还 queue capacity。

```rust,no_run
use busybeaver::{Beaver, LaneConfig, OrderingKey, Priority, SpawnOptions, TaskSpec};

async fn lane_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(
        LaneConfig::new("network").capacity(128).concurrency(8),
    )?;
    let options = SpawnOptions::new()
        .priority(Priority::new(6)?)
        .ordering_key(OrderingKey::try_from("customer:42")?);
    let handle = lane.try_spawn_with_options(
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
        options,
    )?;
    handle.wait().await;
    Ok(())
}
```

- `try_spawn` 立即返回 overload。
- `spawn` 使用 FIFO producer ticket 等待容量。
- `spawn_timeout` 在方法调用时捕获 admission deadline。
- 非等待 producer 不能插到正式 waiter 前面。
- priority 范围为 0 到 7；连续七次 priority selection 后，第八次选择 oldest-ready。
- 相同 ordering key 不并发；被 key 阻塞的 entry 不参与 aging。
- queued cancel 会真实删除 entry，同时归还 capacity 与 key 引用。
- Lane 配置不可变；同名但不同配置会返回错误。

`LaneStats` 提供 queued、running、capacity、waiter 和 key-blocked 统计。`close` 停止新 admission；
`close_and_cancel` 还会等待 accepted work 终止。捕获的 runtime 消失后，accepted handle 会进入
终态，等待 producer 返回 `ExecutorUnavailable`。

## Typed retry

retry 必须显式授权：配置 `retry_all_errors` 或 predicate。attempt timeout 默认不代表可 retry，
因为远端副作用可能已经发生；只有显式配置 `retry_timed_out_attempts` 才会继续。

```rust,no_run
use busybeaver::{AttemptContext, Backoff, Beaver, LaneConfig, RetryBuilder, TaskExit};
use std::time::Duration;

async fn retry_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("retry"))?;
    let retry = RetryBuilder::new(|attempt: AttemptContext| async move {
        if attempt.number() < 3 { Err("temporary") } else { Ok(7_u8) }
    })
    .max_attempts(4)
    .retry_all_errors()
    .backoff(Backoff::fixed(Duration::from_millis(10)))
    .build()?;
    let mut handle = lane.spawn_retry(retry).await?;
    match handle.join().await? {
        TaskExit::Completed(7) => Ok(()),
        _ => Err(std::io::Error::other("retry did not complete").into()),
    }
}
```

backoff 支持 none、fixed、explicit 和 exponential；带 seed 的 `Jitter` 可确定复现。overall
deadline、attempt timeout、retry predicate 和 `RetryDecisionContext` 可以组合。
`RetrySpec::clone` 共享不可变配置，每次 execution 使用独立 attempt 与 jitter iterator。

## Recurring execution 与 schedule

recurring tick 不重叠。`TickOutcome::Continue` 安排下一 tick；`TickOutcome::Stop(T)` 以 typed 值
完成。

```rust,no_run
use busybeaver::{Beaver, LaneConfig, RecurringBuilder, Schedule, TickOutcome};
use std::time::Duration;

async fn recurring_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("recurring"))?;
    let recurring = RecurringBuilder::new(|tick| async move {
        Ok::<_, ()>(if tick.number() >= 3 {
            TickOutcome::Stop(tick.number())
        } else {
            TickOutcome::Continue
        })
    })
    .schedule(Schedule::fixed_delay(Duration::from_millis(20)))
    .build()?;
    let mut handle = lane.spawn_recurring(recurring).await?;
    let _ = handle.join().await?;
    Ok(())
}
```

schedule 支持 initial delay、fixed delay、fixed rate、finite steps、repeat-last、dynamic decision、
missed-tick policy、resume policy 和确定性 jitter。tick failure 与 panic restart 通过
`TickFailurePolicy`、`PanicPolicy` 和 `RestartPolicy` 显式配置并受上限约束。
`RecurringBuilder::from_retry` 会在每个 tick 内执行一套完整 typed retry。

## Slot 与 Scope

`TaskSlot` 管理带 revision 的 newest-wins transaction。`StrictSingleInstance` 等待旧 execution
终止；`AvailabilityFirst` 允许文档约定的重叠。stale revision 与相同 revision 定义冲突都有
显式 outcome。drop `ReplaceHandle` 不会撤销已接受 transaction。

`Scope` 拥有当前 `ScopeGeneration`。rotation 会关闭旧 generation admission，按
`RotationPolicy` cancel 或 drain，join child scope，再发布新 generation。drop `RotationHandle`
不会撤销已接受 rotation。

```rust,no_run
use busybeaver::{Beaver, LaneConfig, ReplacePolicy, RotationPolicy, SlotKey, TaskSpec};

async fn slot_and_scope(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("lifecycle"))?;
    let slot = beaver.create_task_slot(SlotKey::new("latest")?, lane.clone())?;
    let replacement = slot.replace(
        1,
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
        ReplacePolicy::StrictSingleInstance,
    )?;
    let _ = replacement.await?;

    let scope = beaver.create_scope("session", lane)?;
    let rotation = scope.rotate(RotationPolicy::Strict)?;
    let _ = rotation.wait().await;
    Ok(())
}
```

strict self-replace 与 self-rotation 会在修改当前 transaction/generation 前直接拒绝。

## Supervised Service

service 使用独立 supervisor，不占普通 FIFO capacity。readiness 与 health 都绑定 generation。
failure/panic restart 必须显式启用，并受 window、次数和 backoff 约束。shutdown 会禁止 restart、
取消 tracked child，并且恰好调用一次经过 panic/timeout 隔离的 shutdown hook。

```rust,no_run
use busybeaver::{Beaver, CancelReason, ServiceBuilder};

async fn service_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let service = ServiceBuilder::new(|context| async move {
        context.ready();
        context.cancelled().await;
        Ok::<_, ()>(())
    })
    .build()?;
    let mut handle = beaver.start_service(service)?;
    handle.wait_ready().await?;
    handle.control().cancel(CancelReason::UserRequested);
    let _ = handle.join().await?;
    Ok(())
}
```

`ServiceStatus`、`HealthStatus` 和 `ServiceHandle::status` 暴露当前 generation。
`ServiceContext::spawn_child` 追踪 service child，hook 结果会进入 checked shutdown cleanup record。

## Legacy 兼容模型

以下 legacy 模型仍是受支持的公开 API：

| Builder | 执行行为 |
| --- | --- |
| `FixedCountBuilder` | 最多执行 N 次，可配置 progress callback |
| `TimeIntervalBuilder` | 每次执行前都使用对应 delay，包括第一次 |
| `RangeIntervalBuilder` | 按 attempt index 分段；第一次立即执行；后添加 range 覆盖先添加 range |
| `PeriodicBuilder` | resident periodic 工作，panic 后按周期自愈 |

```rust,no_run
use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};

async fn legacy_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(3)
        .build()?;
    beaver.enqueue(task).await?;
    Ok(())
}
```

`work`、`work_with_state`、`Work` 与 `WorkResult` 定义 legacy work；`WorkListener`、`listener`、
`listener_with_error` 与 `FixedCountProgress` 提供生命周期和进度 callback。callback panic 会被
隔离，但同步 callback 仍应保持短小，否则会阻塞 runtime worker。名称中的 “thread” 表示 Tokio
worker lane，不代表独占 OS thread。

## Event、Snapshot 与隐私

`subscribe_events` 返回有界 broadcast `EventStream`。慢 subscriber 收到
`EventRecvError::Lagged`，不会反压 execution。同一 execution 的 `Admitted` 先于后续状态与
terminal event，`Terminal` 恰好发送一次。

```rust,no_run
use busybeaver::{Beaver, TaskEvent, TaskSpec};

async fn observe(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = beaver.subscribe_events()?;
    let handle = beaver.spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) }))?;
    let execution_id = handle.execution_id();
    loop {
        match events.recv().await? {
            TaskEvent::Terminal { execution_id: observed, .. } if observed == execution_id => break,
            _ => {}
        }
    }
    let snapshot = beaver.snapshot();
    println!("active={}, history={}", snapshot.active.len(), snapshot.terminal_history.len());
    Ok(())
}
```

`ExecutorSnapshot` 包含排序后的 active task、有界/TTL terminal history、LaneStats 和 live
scope/slot/subscriber 数量。event、report 与默认 tracing 不包含 typed 业务值、业务错误、panic
文本、ordering-key 字节或任意 metadata。

## Checked Shutdown

生命周期不可逆：`Running -> ShuttingDown(shared barrier) -> Stopped(shared report)`。相同 options
的并发 caller 获得同一个 `ShutdownHandle`，冲突 options 返回
`ShutdownError::ConfigConflict`。

```rust,no_run
use busybeaver::{Beaver, ShutdownMode, ShutdownOptions, ShutdownTimeoutAction};
use std::time::Duration;

async fn stop(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::DrainFinite)
            .grace_period(Duration::from_secs(5))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;
    let _ = shutdown.wait_grace_outcome().await?;
    let report = shutdown.wait_final().await?;
    println!("shutdown tasks={}", report.tasks.len());
    Ok(())
}
```

`CancelAll` 取消全部工作；`DrainFinite` drain finite execution，同时停止 recurring 与 service。
timeout action 可以继续追踪、只对 opt-in future 请求 abort，或不设 grace cutoff 持续等待。
progress/final report 保留 task exit、forced-cancel request、cleanup phase、callback failure 与 worker
failure。`destroy` 仍是兼容 helper；checked shutdown 才是完整可报告生命周期。

## Error、资源限制与 runtime 边界

业务策略应 match typed error variant，telemetry 记录 `code()`；不要解析 `Display` 文本。带
`#[non_exhaustive]` 的公开 error/outcome enum 必须保留 fallback match arm。

| Error 家族 | 稳定 code 示例 |
| --- | --- |
| 构造与资源 | `BB-RUNTIME-UNAVAILABLE`、`BB-INVALID-LANE-CAPACITY`、`BB-RESOURCE-LIMIT-EXCEEDED` |
| admission 与生命周期 | `BB-QUEUE-FULL`、`BB-EXECUTOR-SHUTTING-DOWN`、`BB-SHUTDOWN-TIMED-OUT` |
| legacy execution | `BB-RUNTIME-TASK-FAILED`、`BB-RUNTIME-RETRIES-EXHAUSTED` |
| recurring | `BB-RECURRING-TICK-FAILED`、`BB-RECURRING-RESTART-LIMIT-EXCEEDED`、`BB-SCHEDULE-*` |
| service | `BB-SERVICE-BODY-FAILED`、`BB-SERVICE-RESTART-LIMIT-EXCEEDED` |

`ResourceLimits` 对 active execution、lane、scope、slot、event subscriber、tracked child、waiting
producer、ordering key、event capacity、tag 与 terminal history 设置上限。构造后限制不可变，
admission 失败会完整回滚，不留下 ghost execution/event。

runtime 没有 time driver 时，timer-dependent 工作返回 `TimerUnavailable`，不使用 timer 的工作仍
可运行。panic 隔离依赖 `panic = "unwind"`；`panic = "abort"` 仍会终止进程。BusyBeaver 不提供
分布式 lease、远端幂等、数据库事务或外部副作用回滚。

## Future Drop 契约

| Future 或 handle | Drop 行为 |
| --- | --- |
| lane spawn/retry/recurring 在 admission 前 | 不发布 execution |
| 已 accepted 的 lane spawn/retry/recurring | execution 继续 |
| wait/join future | 除非结果已原子取走，否则仍可再次读取 |
| `cancel_and_wait` future | 同步提交的取消继续生效 |
| slot replace 或 scope rotate handle | 已接受 supervisor 继续 |
| shutdown wait | 共享 shutdown supervisor 继续 |
| 普通 `TaskHandle` | detach；只有 cancel-on-drop 才请求取消 |

内部状态锁持有期间不会调用或析构用户 future、callback、typed value、业务错误、metadata 或 hook。

## 贡献与发布指南

production 代码不得新增 `unsafe`、未检查索引/算术、`unwrap`、`expect`、panic/assert 宏、
`todo`、`unimplemented`、`unreachable` 或动态 `RefCell` borrow。可恢复失败通过 typed `Result`
或 terminal outcome 返回；私有 invariant failure 只记录不含业务数据的稳定诊断。

并发测试使用 deterministic barrier，时间测试使用 paused Tokio time。stress test 只能作为补充，
不能替代确定性回归测试。公开行为修改必须有 integration test；私有状态转换必须有同模块 unit
test；文档中的 Rust 示例必须能够编译。

日常开发、发布门禁和 MSRV 检查全部使用 [`rust-toolchain.toml`](../rust-toolchain.toml) 固定的
Rust 1.89.0 工具链；如果 rustup 尚未自动安装，请执行 `rustup toolchain install 1.89.0`。从仓库
根目录执行完整发布门禁，其中显式指定的 `rustup run 1.89.0` 命令用于明确标识 MSRV 兼容性检查：

```text
cargo fmt --manifest-path busybeaver/Cargo.toml --all -- --check
cargo clippy --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- --include-ignored
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --no-default-features -- --include-ignored
cargo test --manifest-path busybeaver/Cargo.toml --release --all-targets --all-features
cargo test --manifest-path busybeaver/Cargo.toml --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path busybeaver/Cargo.toml --no-deps --all-features
RUSTFLAGS="-C panic=abort" cargo check --manifest-path busybeaver/Cargo.toml --lib --all-features
rustup run 1.89.0 cargo check --manifest-path busybeaver/Cargo.toml --all-targets --all-features
cargo package --manifest-path busybeaver/Cargo.toml --locked
```

性能修改还必须在相同机器、toolchain、feature 与 lockfile 上执行公开 API A/B benchmark，记录
median、tail latency、allocation、峰值 RSS 和普通路径回归。Linux、macOS、Windows 跨平台 CI
仍是发布硬门禁。

## 公开功能覆盖索引

本文覆盖全部公开功能家族：构造与资源限制；typed task 身份、control、result、cancel、forced
cancel、selector 与 tracked child；lane、priority、ordering 与 backpressure；retry；recurring
schedule/policy；slot；scope；service；checked shutdown 与 cleanup report；event、snapshot 与
retention；四类 legacy builder、work/listener/progress helper；稳定诊断；runtime、panic、隐私和
Drop 契约；贡献与发布验证。所有字段和方法签名以 [Rust API 参考](https://docs.rs/busybeaver)
为准。
