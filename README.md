# BusyBeaver

<p align="center">
  <strong><a href="#%E4%B8%AD%E6%96%87">中文</a></strong>
  ·
  <strong><a href="#english">English</a></strong>
</p>

---

## 中文

**BusyBeaver** 是一个带可配置重试策略的异步任务执行器，适用于 Rust 异步运行时（如 Tokio）。

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
use busybeaver::{work, Beaver, PeriodicBuilder, WorkResult};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .build()?;
    beaver.enqueue(task)?;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    beaver.cancel_all()?;
    beaver.destroy()?;
    Ok(())
}
```

### 许可证

MIT OR Apache-2.0

---

## English

**BusyBeaver** is a resilient async task executor with configurable retry policies for Rust async runtimes (e.g. Tokio).

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
use busybeaver::{work, Beaver, PeriodicBuilder, WorkResult};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .build()?;
    beaver.enqueue(task)?;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    beaver.cancel_all()?;
    beaver.destroy()?;
    Ok(())
}
```

### License

MIT OR Apache-2.0
