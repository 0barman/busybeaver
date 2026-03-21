# 集成文档（中文）

### 创建Beaver
- 使用 new 方法创建 Beaver 实例。通过指定默认工作线程的名称和通道容量（Channel Capacity），即可调用 enqueue 方法向该线程提交待执行的任务。

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

### 将任务调度至特定执行线程
- 若希望避免任务在默认线程（如 default）中排队阻塞，可以使用 enqueue_on_new_thread 将任务提交至指定名称的新线程队列中执行。 
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

    // 该任务会在thread_1所在的线程队列中等待被执行，而不是default
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
- 调用 cancel_all 方法将取消 Beaver 中所有已入队的任务，范围涵盖默认线程及通过 enqueue_on_new_thread 动态创建的所有线程。 
- 注意：标记为“常驻执行”的任务也会在此操作中被强制取消。

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let ret = beaver.cancel_all();
    Ok(())
}
```

### 释放指定线程资源
- 通过 release_thread_resource_by_name 方法可以释放特定名称的线程及其关联队列。 
- 注意：初始化时通过 Beaver::new 创建的默认线程无法通过此方法单独释放。

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let ret = beaver.release_thread_resource_by_name("thread_1");
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
    let ret = beaver.destroy();
    Ok(())
}
```