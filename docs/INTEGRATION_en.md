# Integration Guide (English)

### Creating a Beaver Instance
- Use the new method to create a Beaver instance. By specifying the name of the default worker thread and the channel capacity, you can call the enqueue method to submit tasks to that thread.

```rust
use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 125);
    Ok(())
}
```

### Creating Tasks with Specific Time Intervals
- The following example demonstrates how to create a task that executes according to a specific time sequence. You can pass an array of milliseconds via the intervals_millis function to have the task run periodically based on the defined intervals.

```rust
use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = TimeIntervalBuilder::new(work(move || async {
        println!("Simulating async task execution");
        // Simulate task processing time
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // The return value determines whether to retry/continue execution
        WorkResult::NeedRetry
    }))
        .listener(listener(
            move || {
                // Callback: Task execution completed
            },
            || {
                // Callback: Task execution interrupted
            },
        ))
        .intervals_millis(vec![1000, 2000, 3000, 4000])
        .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // For demonstration: Block the main thread to wait for tasks to finish
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### Creating Tasks with a Fixed Execution Count
- This example shows how to create a task with a fixed number of executions. Use the count function to set the total number of times the task should run.

```rust
use busybeaver::{listener, work, Beaver, FixedCountBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = FixedCountBuilder::new(work(move || async {
        println!("Simulating async task execution");
        // Simulate task processing time
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // The return value determines whether to retry execution
        WorkResult::NeedRetry
    }))
        .count(5)
        .listener(listener(
            move || {
                // Callback: Task execution completed
            },
            || {
                // Callback: Task execution interrupted
            },
        ))
        .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // For demonstration: Block the main thread to wait for tasks to finish
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### Creating Periodic Tasks
- The following example demonstrates a task that executes continuously at a fixed interval. You can specify the frequency using the interval method. 
- Note: If interval is set to Duration::ZERO, the internal logic skips tokio::time::sleep, resulting in zero-delay execution. The task will run indefinitely unless it returns WorkResult::Done(()) or is manually canceled.

```rust
use busybeaver::{listener, work, Beaver, PeriodicBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = PeriodicBuilder::new(work(move || async {
        println!("Simulating async task execution");
        // Simulate task processing time
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // The return value determines whether to repeat execution
        WorkResult::NeedRetry
    }))
        .interval(Duration::from_millis(2000))
        .listener(listener(
            move || {
                // Callback: Task execution completed
            },
            || {
                // Callback: Task execution interrupted
            },
        ))
        .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // For demonstration: Block the main thread to wait for tasks to finish
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### Creating Range-Based Interval Tasks
- This example shows how to configure a multi-stage interval task. You can set the total execution count (e.g., 20) and use the add_range function to define different execution frequencies for specific count intervals.

```rust
use busybeaver::{listener, work, Beaver, RangeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = RangeIntervalBuilder::new(
        work(move || async {
            println!("Simulating async task execution");
            // Simulate task processing time
            tokio::time::sleep(Duration::from_millis(1000)).await;
            // The return value determines whether to continue
            WorkResult::NeedRetry
        }),
        20,
    )
        .add_range(0, 5, Duration::from_millis(100))
        .add_range(6, 10, Duration::from_millis(500))
        .add_range(10, 30, Duration::from_millis(500))
        .listener(listener(
            move || {
                // Callback: Task execution completed
            },
            || {
                // Callback: Task execution interrupted
            },
        ))
        .build();

    let _ = beaver.enqueue(task.unwrap()).await;

    // For demonstration: Block the main thread to wait for tasks to finish
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### Enqueuing Tasks to Specific Threads
- To avoid blocking the default thread, you can use enqueue_on_new_thread to submit tasks to a specific named thread queue. 
- Persistent Tasks: If the long_resident property of a task is set to true, it will be preserved when calling cancel_non_long_resident, allowing for persistent execution.

```rust
use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let task = TimeIntervalBuilder::new(work(move || async {
        println!("Simulating async task execution");
        // Simulate task processing time
        tokio::time::sleep(Duration::from_millis(1000)).await;
        WorkResult::NeedRetry
    }))
        .listener(listener(
            move || {
                // Callback: Task execution completed
            },
            || {
                // Callback: Task execution interrupted
            },
        ))
        .intervals_millis(vec![1000, 2000, 3000, 4000])
        .build();

    // This task will be queued and executed in "thread_1" instead of "default"
    let ret = beaver
        .enqueue_on_new_thread(task.unwrap(), "thread_1", 100, false)
        .await;

    // For demonstration: Block the main thread to wait for tasks to finish
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}
```

### Canceling All Tasks
- Calling cancel_all will cancel all enqueued tasks in the Beaver instance, including those in the default thread and any threads created via enqueue_on_new_thread.
- Note: Tasks marked as long_resident will also be forcefully canceled.

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let _ = beaver.cancel_all().await;
    Ok(())
}
```

### Releasing Specific Thread Resources
- Use the release_thread_resource_by_name method to free a specific thread and its associated queue. 
- Note: The default thread created via Beaver::new cannot be released individually using this method.

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let _ = beaver.release_thread_resource_by_name("thread_1").await;
    Ok(())
}
```

### Destroying Resources
- The destroy method stops all running tasks and thoroughly releases all underlying resources held by the Beaver instance.

```rust
use busybeaver::Beaver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("default", 256);
    let ret = beaver.destroy();
    Ok(())
}
```
