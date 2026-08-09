# 集成文档（中文）

### 选择 API

新项目建议使用类型化 `Scheduler`：每次提交都有独立运行标识与类型化终态，并支持
lane、任务组、重试、调度、取消和可信关闭。`Beaver` 与 Builder API 继续用于兼容
0.2 代码。

```rust
use busybeaver::{Job, Scheduler, TaskTerminal};

#[tokio::main]
async fn main() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, String>(42) }))
        .await
        .unwrap();

    assert!(matches!(handle.join().await, TaskTerminal::Completed(42)));
    assert!(scheduler.shutdown().await.is_complete());
}
```

兼容边界见 [0.3 迁移指南](MIGRATION_0_3.md)，运行时、目标平台与 panic 语义见
[平台支持](PLATFORM_SUPPORT.md)。
稳定错误码及不绑定具体 logger 的 SDK 日志约定见
[错误码与 SDK 日志](ERROR_CODES_AND_LOGGING.md)。

### 创建 Beaver
- 使用 `new` 创建 Beaver 实例。通过指定默认执行 lane 的名称和通道容量（Channel Capacity），即可调用 `enqueue` 向该 lane 提交任务。lane 是 Tokio task 与队列，不代表独占操作系统线程。需要校验错误而不是 panic 时使用 `Beaver::try_new`。

```rust
use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 125);
    Ok(())
}
```

### 创建按指定时间间隔执行的任务
- 以下示例展示了如何创建按特定时间序列执行的任务。你可以通过 intervals_millis 函数传入一个毫秒数组，使任务按照数组定义的时间间隔节奏循环执行。

```rust
use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = TimeIntervalBuilder::new(work(move || async {
        println!("模拟任务异步执行");
        // 模拟任务异步执行的耗时
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // 根据此返回值决定是否重试执行
        WorkResult::NeedRetry
    }))
    .listener(listener(
        move || {
            // 任务执行完成回调
        },
        || {
            // 任务执行被中断回调
        },
    ))
    .intervals_millis(vec![1000, 2000, 3000, 4000])
    .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // 为了演示效果，在此添加阻塞以等待任务执行
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### 创建固定次数执行的任务
- 以下示例展示了如何创建具有固定执行次数的任务。通过 count 函数即可轻松设定任务的运行总数。

```rust
use busybeaver::{listener, work, Beaver, FixedCountBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = FixedCountBuilder::new(work(move || async {
        println!("模拟任务异步执行");
        // 模拟任务异步执行的耗时
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // 根据此返回值决定是否重试执行
        WorkResult::NeedRetry
    }))
    .count(5)
    .listener(listener(
        move || {
            // 任务执行完成回调
        },
        || {
            // 任务执行被中断回调
        },
    ))
    .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // 为了演示效果，在此添加阻塞以等待任务执行
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### 创建周期性循环任务
- 以下示例展示了如何创建一个按固定周期持续执行的任务。你可以通过 interval 方法指定执行频率。
- 注意：若将 interval 设为 Duration::ZERO，内部将跳过 tokio::time::sleep 逻辑，实现无间隔的持续运行。该任务会一直执行，除非返回 WorkResult::Done(()) 或被手动取消。

```rust
use busybeaver::{listener, work, Beaver, PeriodicBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = PeriodicBuilder::new(work(move || async {
        println!("模拟任务异步执行");
        // 模拟任务异步执行的耗时
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // 根据此返回值决定是否重试执行
        WorkResult::NeedRetry
    }))
    .interval(Duration::from_millis(2000))
    .listener(listener(
        move || {
            // 任务执行完成回调
        },
        || {
            // 任务执行被中断回调
        },
    ))
    .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // 为了演示效果，在此添加阻塞以等待任务执行
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### 创建分段区间执行的任务
- 以下示例展示了如何配置分段间隔任务。你可以设定任务的总执行次数（例如 20 次），并利用 add_range 函数为总次数内的不同阶段（区间）配置差异化的执行频率。

```rust
use busybeaver::{listener, work, Beaver, RangeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = RangeIntervalBuilder::new(
        work(move || async {
            println!("模拟任务异步执行");
            // 模拟任务异步执行的耗时
            tokio::time::sleep(Duration::from_millis(1000)).await;
            // 根据此返回值决定是否重试执行
            WorkResult::NeedRetry
        }),
        20,
    )
    .add_range(0, 5, Duration::from_millis(100))
    .add_range(6, 10, Duration::from_millis(500))
    .add_range(10, 30, Duration::from_millis(500))
    .listener(listener(
        move || {
            // 任务执行完成回调
        },
        || {
            // 任务执行被中断回调
        },
    ))
    .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // 为了演示效果，在此添加阻塞以等待任务执行
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### 将任务调度至特定执行 lane
- 若希望避免任务在默认 lane（如 default）中排队，可以使用 `enqueue_on_new_thread` 将任务提交至指定名称的新 lane 中执行。方法名因兼容性保留，但不会创建独占操作系统线程。
- 常驻任务：若将任务的 long_resident 属性设为 true，则该任务在调用 cancel_non_long_resident 时会被保留，从而实现常驻执行。

```rust
use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = TimeIntervalBuilder::new(work(move || async {
        println!("模拟任务异步执行");
        // 模拟任务异步执行的耗时
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // 根据此返回值决定是否重试执行
        WorkResult::NeedRetry
    }))
        .listener(listener(
            move || {
                // 任务执行完成回调
            },
            || {
                // 任务执行被中断回调
            },
        ))
        .intervals_millis(vec![1000, 2000, 3000, 4000])
        .build();

    // 该任务会在 thread_1 lane 中等待执行，而不是 default lane
    let ret = beaver
        .enqueue_on_new_thread(task.unwrap(), "thread_1", 100, false)
        .await;

    // 为了演示效果，在此添加阻塞以等待任务执行
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### 取消所有任务执行
- 调用 `cancel_all` 将取消 Beaver 中所有已入队的任务，范围涵盖默认 lane 及通过 `enqueue_on_new_thread` 动态创建的所有 lane。
- 注意：标记为“常驻执行”的任务也会在此操作中被强制取消。

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    beaver.cancel_all().await?;
    Ok(())
}
```

### 释放指定 lane 资源
- 通过 `release_thread_resource_by_name` 可以释放特定名称的 lane 及其关联队列。
- 注意：方法名因兼容性保留；通过 `Beaver::new` 创建的默认 lane 无法通过此方法单独释放。

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    beaver.release_thread_resource_by_name("thread_1").await?;
    Ok(())
}
```

### 销毁资源
- 使用 destroy 函数将停止所有正在运行的任务，并彻底销毁 Beaver 实例持有的全部底层资源。

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    beaver.destroy().await?;
    Ok(())
}
```
