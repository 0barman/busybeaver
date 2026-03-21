# BusyBeaver

<p align="center">
  <strong><a href="#%E4%B8%AD%E6%96%87">中文</a></strong>
  ·
  <strong><a href="#english">English</a></strong>
</p>

---

## 中文

**BusyBeaver** 是一个支持按次数、按周期、按时间间隔等策略执行任务的库。让您代码中需要周期执行的任务，如心跳、心跳上报、定时轮询、定时清理等执行更简单、更可靠。
它本质是一个带可配置重试策略的异步任务执行器，专为 Rust 异步运行时（如 Tokio）打造。无论任务是“固定执行 N 次后停止”，还是“每隔 X 毫秒重复一次”，或是“在指定时间窗口内按间隔执行”，BusyBeaver 都能帮你优雅处理，同时内置指数退避、重试上限、任务监听与进度回调等机制，让你的异步代码不再需要自己手动写一堆 tokio::time + loop + retry 逻辑。

### 特性

- 可配置的重试策略与退避
- 支持按次数、按周期、按时间间隔等任务类型
- 任务监听与进度回调
- 与 Tokio 集成

### 集成文档

- [集成文档（中文）](docs/INTEGRATION_zh.md)

### 快速开始

```toml
[dependencies]
busybeaver = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
```

```rust
use busybeaver::{work, Beaver, TimeIntervalBuilder, WorkResult};
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

    // 注意：为了测试在此等待任务执行完毕才添加的阻塞
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### 许可证

MIT OR Apache-2.0

---

## English

**BusyBeaver** BusyBeaver is a task-scheduling library that supports execution strategies based on counts, cycles, and custom time intervals. It streamlines periodic tasks in your codebase—such as heartbeats, metric reporting, scheduled polling, and automated cleanup—making them simpler and more reliable.At its core, BusyBeaver is an asynchronous task executor with configurable retry strategies, purpose-built for Rust async runtimes like Tokio. Whether a task needs to stop after exactly $N$ executions, repeat every $X$ milliseconds, or run at specific intervals within a defined time window, BusyBeaver handles the complexity elegantly. Equipped with built-in mechanisms like exponential backoff, retry limits, task listeners, and progress callbacks, it eliminates the need to manually write tedious tokio::time + loop + retry boilerplate in your asynchronous code.

### Features

- Configurable retry policies and backoff
- Task types: fixed count, periodic, time interval
- Task listeners and progress callbacks
- Tokio integration

### Integration docs

- [Integration guide (English)](docs/INTEGRATION_en.md)

### Quick start

```toml
[dependencies]
busybeaver = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
```


```rust
use busybeaver::{work, Beaver, TimeIntervalBuilder, WorkResult};
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

    // 注意：为了测试在此等待任务执行完毕才添加的阻塞
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### License

MIT OR Apache-2.0
