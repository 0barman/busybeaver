# BusyBeaver 0.3 集成指南

本文面向接入 BusyBeaver 的应用开发者，覆盖从依赖配置到生产环境生命周期处理的最短路径。
精确行为保证见 [API 契约](API_CONTRACT_0_3.md)，机器可读失败信息见
[错误码参考](ERROR_CODES.md)。

## Runtime 与构造

BusyBeaver 在构造时绑定一个 Tokio runtime，后续调用和新 lane 不会静默改绑到调用方 runtime。

```rust
use busybeaver::{Beaver, BeaverError, ResourceLimits};

fn build_executor() -> Result<Beaver, BeaverError> {
    Beaver::builder("legacy-default", 256)
        .resource_limits(ResourceLimits::default())
        .build()
}
```

在 runtime 外使用 `.runtime_handle(handle)`。`new/new_with_handle`、`builder/try_new` 均返回
`Result`；非法容量和 runtime 缺失通过稳定的 `BeaverError` 变体报告，不会 panic。
可使用 `BeaverError::code()` 获取稳定的 `BB-*` 机器可读错误码；legacy listener 的
`RuntimeError` 以及 recurring/service 终态错误也提供 `code()`。
传入的 runtime 通常应启用 time driver；未启用时，timer 路径返回 `TimerUnavailable`，不使用
timer 的 work 与 lane 仍可继续运行。

## 选择模型

| 需求 | API |
|---|---|
| 可复用 typed operation | `TaskSpec` + `Beaver::spawn` |
| 有界队列、并发和隔离 | `Lane` |
| 业务 retry 与最后错误 | `RetryBuilder` |
| 动态或无限 cadence | `RecurringBuilder` |
| newest-wins replace | `TaskSlot` |
| session/page/request generation | `Scope` |
| readiness/restart/shutdown hook | `ServiceBuilder` |

## Typed task 与取消

```rust
use busybeaver::{CancelReason, TaskExit, TaskSpec};

async fn run(
    beaver: &busybeaver::Beaver,
    should_stop: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut handle = beaver.spawn(TaskSpec::new(|context| async move {
        context.sleep(std::time::Duration::from_secs(1)).await?;
        Ok::<_, busybeaver::Cancelled>(42)
    }))?;
    let control = handle.control();
    if should_stop {
        control.cancel(CancelReason::UserRequested);
    }
    match handle.join().await? {
        TaskExit::Completed(value) => println!("completed with {value}"),
        TaskExit::Cancelled { reason } => println!("cancelled: {reason:?}"),
        _ => println!("execution reached another terminal outcome"),
    }
    Ok(())
}
```

取消默认是 cooperative。使用 tracked child 和 `WorkContext::sleep` 完成结构化清理。
`AbortPolicy::Allowed` 只允许 drop tracked async body，不是 OS thread 或进程强杀。
execution 内等待其它 tracked execution 时使用 `wait_checked`；直接 self/ancestor wait 会返回
`ExecutionWaitError::WouldJoin`，不会形成结构化并发死锁。

## Lane、backpressure 与 QoS

```rust
use busybeaver::{LaneConfig, OrderingKey, Priority, SpawnOptions, TaskSpec};

async fn submit(beaver: &busybeaver::Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(
        LaneConfig::new("http").capacity(128).concurrency(16),
    )?;
    let options = SpawnOptions::new()
        .priority(Priority::new(6)?)
        .ordering_key(OrderingKey::try_from("tenant:17")?);
    let handle = lane.try_spawn_with_options(
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
        options,
    )?;
    handle.wait().await;
    Ok(())
}
```

`try_spawn` 立即报告 overload，`spawn` 等待容量，`spawn_timeout` 限制 admission。相同 ordering
key 不重叠；priority 有确定 aging。等待 producer 按 FIFO ticket admission，`try_spawn` 不能插队。
队列取消会真实删除 entry 并归还 capacity。

## Retry、Recurring、Scope/Slot 与 Service

- retry 必须显式授权，并保留 owned last business error；attempt timeout 默认不自动 retry。
- recurring 使用 `Continue/Stop(T)`，支持 fixed delay/rate、steps、dynamic function、missed tick、
  seeded jitter、显式 resume 和 typed retry composition。
- TaskSlot revision 明确拒绝 stale replace；Strict policy 不允许旧/新 execution 重叠。
- scoped spawn 与 generation rotate 线性化；旧 generation 不能继续 admission。
- service 不占普通 FIFO slot，提供 generation readiness/health、bounded restart、tracked child 和
  exactly-once shutdown hook。

详见 [API 契约](API_CONTRACT_0_3.md) 与 [迁移指南](MIGRATION_0_2_TO_0_3.md)。

## Checked shutdown

```rust
use busybeaver::{ShutdownMode, ShutdownOptions, ShutdownTimeoutAction};
use std::time::Duration;

async fn stop(beaver: &busybeaver::Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let handle = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::DrainFinite)
            .grace_period(Duration::from_secs(5))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;
    let grace = handle.wait_grace_outcome().await?;
    let final_report = handle.wait_final().await?;
    println!("grace: {grace:?}; final: {final_report:?}");
    Ok(())
}
```

shutdown 不可逆；相同 options 的并发调用共享 barrier。DrainFinite drain 有界工作，同时停止
recurring/service。timeout snapshot 保留 control 与可重复等待的 shutdown handle。

## Observation 与边界

event 和可选 tracing 均有界且脱敏；snapshot 只保留 bounded/TTL terminal summary。SDK 不接管
远端幂等、业务一致性、blocking/FFI/OS thread 退出、untracked child、远端副作用回滚，以及无
平台 adapter 时的真实 suspend 检测。

## 后续阅读

- 通过 [API 契约](API_CONTRACT_0_3.md) 核对完整行为保证。
- 通过 [错误码参考](ERROR_CODES.md) 接入 telemetry 与支持诊断。
- 从 legacy builder 升级时遵循 [0.2 → 0.3 迁移指南](MIGRATION_0_2_TO_0_3.md)。
- 参与 BusyBeaver 开发前阅读 [开发指南](DEVELOPMENT.md)。
