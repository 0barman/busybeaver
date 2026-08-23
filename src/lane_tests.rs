use super::{select_ready_candidate, Priority, ReadyCandidate};
use crate::{Beaver, CancelReason, ExecutionId, Lane, LaneConfig, TaskExit, TaskHandle, TaskSpec};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

type WaiterResult = (u8, Result<TaskHandle<(), &'static str>, super::SpawnError>);

fn test_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::other(message.into()))
}

fn reference_selection(
    dispatch_count: u64,
    candidates: &[ReadyCandidate],
) -> Option<ReadyCandidate> {
    if dispatch_count % 8 == 7 {
        candidates
            .iter()
            .copied()
            .min_by_key(|candidate| candidate.sequence)
    } else {
        candidates
            .iter()
            .copied()
            .max_by_key(|candidate| (candidate.priority, std::cmp::Reverse(candidate.sequence)))
    }
}

#[derive(Clone, Copy)]
struct SchedulerModelEntry {
    execution_id: ExecutionId,
    priority: Priority,
    sequence: u64,
    ordering_key: Option<u8>,
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn decrement_model_key(
    refcounts: &mut HashMap<u8, usize>,
    key: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(count) = refcounts.get_mut(&key) else {
        return Err(test_error("scheduler model key refcount was missing"));
    };
    let Some(next) = count.checked_sub(1) else {
        return Err(test_error("scheduler model key refcount underflowed"));
    };
    if next == 0 {
        refcounts.remove(&key);
    } else {
        *count = next;
    }
    Ok(())
}

fn validate_scheduler_model(
    seed: u64,
    step: usize,
    queued: &[SchedulerModelEntry],
    running: &[SchedulerModelEntry],
    active_keys: &HashSet<u8>,
    refcounts: &HashMap<u8, usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    let scanned_active = running
        .iter()
        .filter_map(|entry| entry.ordering_key)
        .collect::<HashSet<_>>();
    if &scanned_active != active_keys {
        return Err(test_error(format!(
            "active ordering keys diverged at seed {seed:#x}, step {step}"
        )));
    }

    let mut scanned_refcounts = HashMap::new();
    for key in queued
        .iter()
        .chain(running.iter())
        .filter_map(|entry| entry.ordering_key)
    {
        let count = scanned_refcounts.entry(key).or_insert(0usize);
        let Some(next) = count.checked_add(1) else {
            return Err(test_error("scanned scheduler key refcount overflowed"));
        };
        *count = next;
    }
    if &scanned_refcounts != refcounts {
        return Err(test_error(format!(
            "ordering-key refcounts diverged at seed {seed:#x}, step {step}"
        )));
    }

    let mut ready_by_priority = [0usize; 8];
    let mut blocked = 0usize;
    for entry in queued {
        if entry
            .ordering_key
            .is_some_and(|key| active_keys.contains(&key))
        {
            blocked = blocked.saturating_add(1);
        } else if let Some(count) = ready_by_priority.get_mut(usize::from(entry.priority.value())) {
            *count = count.saturating_add(1);
        }
    }
    let classified = ready_by_priority
        .into_iter()
        .fold(blocked, usize::saturating_add);
    if classified != queued.len() {
        return Err(test_error(format!(
            "scheduler stats did not classify every queued entry at seed {seed:#x}, step {step}"
        )));
    }
    Ok(())
}

fn waiting_spec() -> TaskSpec<(), &'static str> {
    TaskSpec::new(|context| async move {
        context.cancelled().await;
        Ok(())
    })
}

async fn wait_for_waiter_count(
    lane: &Lane,
    expected: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..10_000_usize {
        if lane.stats().waiting_producers == expected {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
    Err(test_error(format!(
        "lane did not reach {expected} waiting producers"
    )))
}

async fn receive_waiter(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<WaiterResult>,
) -> Result<(u8, TaskHandle<(), &'static str>), Box<dyn std::error::Error>> {
    let received = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .map_err(|_| test_error("timed out waiting for a targeted producer notification"))?;
    let Some((number, result)) = received else {
        return Err(test_error("waiting producer result channel closed early"));
    };
    let handle = result.map_err(|error| {
        test_error(format!(
            "waiting producer {number} failed admission: {error}"
        ))
    })?;
    Ok((number, handle))
}

#[test]
fn selector_matches_reference_model_for_priority_aging_and_blocked_keys(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = 0x78b9_214d_2c31_4a05_u64;
    for case in 0..64_u64 {
        let mut candidates = Vec::new();
        for queue_index in 0..128_usize {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let priority = Priority::new(((seed >> 32) & 7) as u8)?;
            let blocked = seed & 3 == 0;
            if !blocked {
                candidates.push(ReadyCandidate {
                    queue_index,
                    execution_id: ExecutionId::new(),
                    priority,
                    sequence: queue_index as u64,
                });
            }
        }

        for dispatch_offset in 0..16_u64 {
            let dispatch_count = case.wrapping_add(dispatch_offset);
            let expected = reference_selection(dispatch_count, &candidates);
            let actual = select_ready_candidate(dispatch_count, candidates.iter().copied());
            if actual.map(|candidate| candidate.execution_id)
                != expected.map(|candidate| candidate.execution_id)
            {
                return Err(test_error(format!(
                    "lane selector diverged from reference model in case {case} at dispatch {dispatch_count}"
                )));
            }
            if actual.map(|candidate| candidate.queue_index)
                != expected.map(|candidate| candidate.queue_index)
            {
                return Err(test_error(format!(
                    "lane selector chose a different queue index in case {case} at dispatch {dispatch_count}"
                )));
            }
        }
    }

    if select_ready_candidate(7, std::iter::empty()).is_some() {
        return Err(test_error("an empty ready set selected an execution"));
    }
    Ok(())
}

#[test]
fn scheduler_matches_reference_through_random_lifecycle_operations(
) -> Result<(), Box<dyn std::error::Error>> {
    for seed_index in 0..100_u64 {
        let seed = 0xD1B5_4A32_D192_ED03_u64 ^ seed_index.wrapping_mul(0x9E37_79B9);
        let mut random = seed;
        let mut queued = Vec::<SchedulerModelEntry>::new();
        let mut running = Vec::<SchedulerModelEntry>::new();
        let mut active_keys = HashSet::<u8>::new();
        let mut refcounts = HashMap::<u8, usize>::new();
        let mut sequence = 0_u64;
        let mut dispatch_count = 0_u64;

        for step in 0..10_000_usize {
            let operation = next_random(&mut random) % 4;
            match operation {
                0 if queued.len() < 64 => {
                    let priority = Priority::new((next_random(&mut random) & 7) as u8)?;
                    let ordering_key = match next_random(&mut random) % 3 {
                        0 => None,
                        _ => Some((next_random(&mut random) & 7) as u8),
                    };
                    if let Some(key) = ordering_key {
                        let count = refcounts.entry(key).or_insert(0);
                        let Some(next) = count.checked_add(1) else {
                            return Err(test_error("scheduler model refcount overflowed"));
                        };
                        *count = next;
                    }
                    queued.push(SchedulerModelEntry {
                        execution_id: ExecutionId::new(),
                        priority,
                        sequence,
                        ordering_key,
                    });
                    sequence = sequence.saturating_add(1);
                }
                1 if !queued.is_empty() => {
                    let index =
                        usize::try_from(next_random(&mut random) % u64::try_from(queued.len())?)?;
                    let removed = queued.remove(index);
                    if let Some(key) = removed.ordering_key {
                        decrement_model_key(&mut refcounts, key)?;
                    }
                }
                2 if running.len() < 8 => {
                    let candidates = queued
                        .iter()
                        .copied()
                        .enumerate()
                        .filter_map(|(queue_index, entry)| {
                            let blocked = entry
                                .ordering_key
                                .is_some_and(|key| active_keys.contains(&key));
                            (!blocked).then_some(ReadyCandidate {
                                queue_index,
                                execution_id: entry.execution_id,
                                priority: entry.priority,
                                sequence: entry.sequence,
                            })
                        })
                        .collect::<Vec<_>>();
                    let expected = reference_selection(dispatch_count, &candidates);
                    let actual = select_ready_candidate(dispatch_count, candidates.into_iter());
                    if actual.map(|candidate| candidate.execution_id)
                        != expected.map(|candidate| candidate.execution_id)
                        || actual.map(|candidate| candidate.queue_index)
                            != expected.map(|candidate| candidate.queue_index)
                    {
                        return Err(test_error(format!(
                            "scheduler lifecycle selection diverged at seed {seed:#x}, step {step}"
                        )));
                    }
                    if let Some(selected) = actual {
                        let entry = queued.remove(selected.queue_index);
                        if let Some(key) = entry.ordering_key {
                            active_keys.insert(key);
                        }
                        running.push(entry);
                        dispatch_count = dispatch_count.saturating_add(1);
                    }
                }
                3 if !running.is_empty() => {
                    let index =
                        usize::try_from(next_random(&mut random) % u64::try_from(running.len())?)?;
                    let finished = running.remove(index);
                    if let Some(key) = finished.ordering_key {
                        active_keys.remove(&key);
                        decrement_model_key(&mut refcounts, key)?;
                    }
                }
                _ => {}
            }
            validate_scheduler_model(seed, step, &queued, &running, &active_keys, &refcounts)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn targeted_waiters_preserve_fifo_when_a_middle_waiter_is_dropped(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("lane-targeted-waiters", 32)?;
    let lane = beaver.create_lane(LaneConfig::new("serial").capacity(1).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..10_000_usize {
        if lane.stats().running == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    if lane.stats().running != 1 {
        return Err(test_error("initial lane execution did not start"));
    }
    let mut queued = lane.try_spawn(waiting_spec())?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut workers = Vec::new();

    for number in 0_u8..8 {
        let waiting_lane = lane.clone();
        let waiting_sender = sender.clone();
        workers.push(tokio::spawn(async move {
            let result = waiting_lane.spawn(waiting_spec()).await;
            let _ = waiting_sender.send((number, result));
        }));
        wait_for_waiter_count(&lane, usize::from(number) + 1).await?;
    }
    drop(sender);

    let middle = workers.remove(3);
    let notifications_before_middle_drop = lane.core.shared.targeted_waiter_notification_count();
    middle.abort();
    let _ = middle.await;
    wait_for_waiter_count(&lane, 7).await?;
    if lane.core.shared.targeted_waiter_notification_count() != notifications_before_middle_drop {
        return Err(test_error(
            "dropping a middle waiter incorrectly woke another producer",
        ));
    }

    let notifications_before_release = lane.core.shared.targeted_waiter_notification_count();
    queued.control().cancel(CancelReason::UserRequested);
    let queued_exit = queued.join().await?;
    if !matches!(queued_exit, TaskExit::Cancelled { .. }) {
        return Err(test_error("queued capacity holder was not cancelled"));
    }

    let expected_order = [0_u8, 1, 2, 4, 5, 6, 7];
    for (index, expected) in expected_order.into_iter().enumerate() {
        let (actual, mut admitted) = receive_waiter(&mut receiver).await?;
        if actual != expected {
            return Err(test_error(format!(
                "targeted waiter order changed at position {index}: expected {expected}, got {actual}"
            )));
        }
        if index == 0 {
            let notification_delta = lane
                .core
                .shared
                .targeted_waiter_notification_count()
                .saturating_sub(notifications_before_release);
            if notification_delta > 2 {
                return Err(test_error(format!(
                    "one capacity release produced {notification_delta} targeted wakes"
                )));
            }
        }
        admitted.control().cancel(CancelReason::UserRequested);
        let exit = admitted.join().await?;
        if !matches!(exit, TaskExit::Cancelled { .. }) {
            return Err(test_error(format!(
                "admitted waiter {actual} did not terminate as cancelled"
            )));
        }
    }

    wait_for_waiter_count(&lane, 0).await?;
    running.control().cancel(CancelReason::UserRequested);
    let running_exit = running.join().await?;
    if !matches!(running_exit, TaskExit::Cancelled { .. }) {
        return Err(test_error("initial running task did not cancel"));
    }
    for worker in workers {
        if let Err(error) = worker.await {
            return Err(test_error(format!(
                "waiting producer worker did not finish cleanly: {error}"
            )));
        }
    }
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn closing_lane_wakes_every_targeted_waiter() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("lane-targeted-close", 16)?;
    let lane = beaver.create_lane(LaneConfig::new("serial-close").capacity(1).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..10_000_usize {
        if lane.stats().running == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    if lane.stats().running != 1 {
        return Err(test_error("initial close-test execution did not start"));
    }
    let mut queued = lane.try_spawn(waiting_spec())?;
    let mut workers = Vec::new();
    for number in 0..4_usize {
        let waiting_lane = lane.clone();
        workers.push(tokio::spawn(async move {
            waiting_lane.spawn(waiting_spec()).await
        }));
        wait_for_waiter_count(&lane, number + 1).await?;
    }

    lane.close();
    for worker in workers {
        let joined = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .map_err(|_| test_error("lane close did not wake a targeted waiter"))?;
        match joined {
            Ok(Err(super::SpawnError::LaneClosing)) => {}
            Ok(Ok(mut handle)) => {
                handle.control().cancel(CancelReason::UserRequested);
                let _ = handle.join().await?;
                return Err(test_error("waiter was admitted after the lane closed"));
            }
            Ok(Err(error)) => {
                return Err(test_error(format!(
                    "lane close woke a waiter with the wrong error: {error}"
                )));
            }
            Err(error) => {
                return Err(test_error(format!(
                    "targeted waiter worker failed while closing: {error}"
                )));
            }
        }
    }
    wait_for_waiter_count(&lane, 0).await?;

    running.control().cancel(CancelReason::UserRequested);
    queued.control().cancel(CancelReason::UserRequested);
    let _ = running.join().await?;
    let _ = queued.join().await?;
    beaver.destroy().await?;
    Ok(())
}
