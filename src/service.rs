use crate::execution::{self, ExecutionKind, ExecutionRegistry, PreparedExecution};
use crate::{
    AbortPolicy, CancelReason, ExecutorStopReason, PanicSource, RestartPolicy, TaskControlHandle,
    TaskExit, TaskFailure, TaskHandle, TaskSpecId, WorkContext,
};
use crate::{CleanupOutcome, CleanupPhase};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

type BoxServiceFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>;
type ServiceFactory<T, E> =
    dyn Fn(ServiceContext) -> BoxServiceFuture<T, E> + Send + Sync + 'static;
type BoxHookFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
type ShutdownHook = dyn Fn(ServiceContext) -> BoxHookFuture + Send + Sync + 'static;

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HealthStatus {
    Healthy,
    Degraded(Arc<str>),
    Unhealthy(Arc<str>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HookOutcome {
    NotRequired,
    Completed,
    Failed(Arc<str>),
    Panicked,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ServiceStatus {
    Starting {
        generation: u64,
    },
    Ready {
        generation: u64,
        health: HealthStatus,
    },
    Stopping {
        generation: u64,
    },
    Restarting {
        generation: u64,
        restart: u32,
    },
    Stopped {
        generation: u64,
        hook: HookOutcome,
    },
}

pub(crate) struct ServiceState {
    status: watch::Sender<ServiceStatus>,
    generation: Mutex<u64>,
}

impl ServiceState {
    fn set_status(&self, status: ServiceStatus) {
        self.status.send_replace(status);
    }
}

#[derive(Clone)]
pub struct ServiceContext {
    work: WorkContext,
    state: Arc<ServiceState>,
    generation: u64,
}

impl ServiceContext {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn work(&self) -> &WorkContext {
        &self.work
    }

    pub fn ready(&self) -> bool {
        let current = *self
            .state
            .generation
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if current != self.generation || self.work.is_cancelled() {
            return false;
        }
        self.state.set_status(ServiceStatus::Ready {
            generation: self.generation,
            health: HealthStatus::Healthy,
        });
        true
    }

    pub fn set_health(&self, health: HealthStatus) -> bool {
        let current = *self
            .state
            .generation
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if current != self.generation || self.work.is_cancelled() {
            return false;
        }
        self.state.set_status(ServiceStatus::Ready {
            generation: self.generation,
            health,
        });
        true
    }

    pub fn is_cancelled(&self) -> bool {
        self.work.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.work.cancelled().await;
    }

    pub fn control(&self) -> TaskControlHandle {
        self.work.control()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RestartTrigger {
    Never,
    OnFailure,
    OnPanic,
    OnFailureOrPanic,
}

impl RestartTrigger {
    fn failure(self) -> bool {
        matches!(self, Self::OnFailure | Self::OnFailureOrPanic)
    }

    fn panic(self) -> bool {
        matches!(self, Self::OnPanic | Self::OnFailureOrPanic)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ServiceBuildError {
    RestartPolicyRequired,
    ZeroRestartBudget,
    ZeroRestartWindow,
    ZeroHookTimeout,
}

impl fmt::Display for ServiceBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RestartPolicyRequired => {
                formatter.write_str("restart trigger requires a restart policy")
            }
            Self::ZeroRestartBudget => formatter.write_str("restart budget must be non-zero"),
            Self::ZeroRestartWindow => formatter.write_str("restart window must be non-zero"),
            Self::ZeroHookTimeout => formatter.write_str("shutdown hook timeout must be non-zero"),
        }
    }
}

impl std::error::Error for ServiceBuildError {}

#[derive(Debug)]
#[non_exhaustive]
pub enum ServiceFailure<E> {
    BodyFailed {
        generation: u64,
        error: E,
    },
    RestartLimitExceeded {
        generation: u64,
        last_error: Option<E>,
    },
    GenerationOverflow,
    CounterOverflow {
        code: &'static str,
        last_error: Option<E>,
    },
}

impl<E> ServiceFailure<E> {
    /// Stable machine-readable code for service terminal failures.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BodyFailed { .. } => "BB-SERVICE-BODY-FAILED",
            Self::RestartLimitExceeded { .. } => "BB-SERVICE-RESTART-LIMIT-EXCEEDED",
            Self::GenerationOverflow => "BB-SERVICE-GENERATION-OVERFLOW",
            Self::CounterOverflow { code, .. } => code,
        }
    }
}

pub struct ServiceSpec<T, E> {
    id: TaskSpecId,
    factory: Arc<ServiceFactory<T, E>>,
    shutdown_hook: Option<Arc<ShutdownHook>>,
    hook_timeout: Duration,
    restart_trigger: RestartTrigger,
    restart_policy: Option<RestartPolicy>,
    abort_policy: AbortPolicy,
    tag: Option<Arc<str>>,
    marker: PhantomData<fn() -> (T, E)>,
}

impl<T, E> Clone for ServiceSpec<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            factory: Arc::clone(&self.factory),
            shutdown_hook: self.shutdown_hook.clone(),
            hook_timeout: self.hook_timeout,
            restart_trigger: self.restart_trigger,
            restart_policy: self.restart_policy.clone(),
            abort_policy: self.abort_policy,
            tag: self.tag.clone(),
            marker: PhantomData,
        }
    }
}

impl<T, E> ServiceSpec<T, E> {
    pub fn id(&self) -> TaskSpecId {
        self.id
    }
}

pub struct ServiceBuilder<T, E> {
    factory: Arc<ServiceFactory<T, E>>,
    shutdown_hook: Option<Arc<ShutdownHook>>,
    hook_timeout: Duration,
    restart_trigger: RestartTrigger,
    restart_policy: Option<RestartPolicy>,
    abort_policy: AbortPolicy,
    tag: Option<Arc<str>>,
}

impl<T, E> ServiceBuilder<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    pub fn new<F, Fut>(factory: F) -> Self
    where
        F: Fn(ServiceContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        Self {
            factory: Arc::new(move |context| Box::pin(factory(context))),
            shutdown_hook: None,
            hook_timeout: Duration::from_secs(5),
            restart_trigger: RestartTrigger::Never,
            restart_policy: None,
            abort_policy: AbortPolicy::CooperativeOnly,
            tag: None,
        }
    }

    pub fn shutdown_hook<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(ServiceContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.shutdown_hook = Some(Arc::new(move |context| Box::pin(hook(context))));
        self
    }

    pub fn shutdown_hook_timeout(mut self, timeout: Duration) -> Self {
        self.hook_timeout = timeout;
        self
    }

    pub fn restart(mut self, trigger: RestartTrigger, policy: RestartPolicy) -> Self {
        self.restart_trigger = trigger;
        self.restart_policy = Some(policy);
        self
    }

    pub fn abort_policy(mut self, policy: AbortPolicy) -> Self {
        self.abort_policy = policy;
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(Arc::from(tag.into()));
        self
    }

    pub fn build(self) -> Result<ServiceSpec<T, E>, ServiceBuildError> {
        if !matches!(self.restart_trigger, RestartTrigger::Never) && self.restart_policy.is_none() {
            return Err(ServiceBuildError::RestartPolicyRequired);
        }
        if let Some(policy) = &self.restart_policy {
            if policy.maximum_restarts() == 0 {
                return Err(ServiceBuildError::ZeroRestartBudget);
            }
            if policy.configured_window().is_zero() {
                return Err(ServiceBuildError::ZeroRestartWindow);
            }
        }
        if self.hook_timeout.is_zero() {
            return Err(ServiceBuildError::ZeroHookTimeout);
        }
        Ok(ServiceSpec {
            id: TaskSpecId::new(),
            factory: self.factory,
            shutdown_hook: self.shutdown_hook,
            hook_timeout: self.hook_timeout,
            restart_trigger: self.restart_trigger,
            restart_policy: self.restart_policy,
            abort_policy: self.abort_policy,
            tag: self.tag,
            marker: PhantomData,
        })
    }
}

pub struct ServiceHandle<T, E> {
    task: TaskHandle<T, E>,
    state: Arc<ServiceState>,
}

impl<T: Send, E: Send> ServiceHandle<T, E> {
    pub(crate) fn new(task: TaskHandle<T, E>, state: Arc<ServiceState>) -> Self {
        Self { task, state }
    }

    pub fn status(&self) -> ServiceStatus {
        self.state.status.borrow().clone()
    }

    pub async fn wait_ready(&self) -> Result<u64, ServiceWaitError> {
        let mut status = self.state.status.subscribe();
        loop {
            match status.borrow().clone() {
                ServiceStatus::Ready { generation, .. } => return Ok(generation),
                ServiceStatus::Stopped { .. } => return Err(ServiceWaitError::StoppedBeforeReady),
                _ => {}
            }
            status
                .changed()
                .await
                .map_err(|_| ServiceWaitError::SupervisorUnavailable)?;
        }
    }

    pub fn control(&self) -> TaskControlHandle {
        self.task.control()
    }

    pub fn into_task_handle(self) -> TaskHandle<T, E> {
        self.task
    }
}

impl<T, E> Deref for ServiceHandle<T, E> {
    type Target = TaskHandle<T, E>;

    fn deref(&self) -> &Self::Target {
        &self.task
    }
}

impl<T, E> DerefMut for ServiceHandle<T, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.task
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ServiceWaitError {
    StoppedBeforeReady,
    SupervisorUnavailable,
}

impl fmt::Display for ServiceWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StoppedBeforeReady => formatter.write_str("service stopped before readiness"),
            Self::SupervisorUnavailable => formatter.write_str("service supervisor unavailable"),
        }
    }
}

impl std::error::Error for ServiceWaitError {}

pub(crate) fn prepare_service<T, E>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    spec: ServiceSpec<T, E>,
) -> crate::BeaverResult<(PreparedExecution<T, E>, Arc<ServiceState>)>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let id = spec.id;
    let tag = spec.tag.clone();
    let abort_policy = spec.abort_policy;
    let (status, _) = watch::channel(ServiceStatus::Starting { generation: 1 });
    let state = Arc::new(ServiceState {
        status,
        generation: Mutex::new(1),
    });
    let run_state = Arc::clone(&state);
    let prepared = execution::prepare_exit_future(registry, runtime, id, tag, move |work| {
        run_service(spec, work, run_state)
    })?;
    prepared.start.set_kind(ExecutionKind::Service);
    prepared.start.set_abort_policy(abort_policy);
    Ok((prepared, state))
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self(handle)
    }

    async fn join(&mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run_service<T, E>(
    spec: ServiceSpec<T, E>,
    base_work: WorkContext,
    state: Arc<ServiceState>,
) -> TaskExit<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let mut generation = 1u64;
    let mut restart_times = VecDeque::new();
    let mut restart_count = 0u32;
    loop {
        *state
            .generation
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = generation;
        state.set_status(ServiceStatus::Starting { generation });
        let work = base_work.for_attempt(u32::try_from(generation).map_or(u32::MAX, |value| value));
        work.begin_attempt();
        let context = ServiceContext {
            work: work.clone(),
            state: Arc::clone(&state),
            generation,
        };
        let future = catch_unwind(AssertUnwindSafe(|| (spec.factory)(context.clone())));
        let future = match future {
            Ok(future) => future,
            Err(payload) => {
                let message = panic_message(payload);
                if execution::is_timer_unavailable_message(&message) {
                    state.set_status(ServiceStatus::Stopped {
                        generation,
                        hook: HookOutcome::NotRequired,
                    });
                    return TaskExit::Failed(TaskFailure::TimerUnavailable);
                }
                if !spec.restart_trigger.panic() {
                    state.set_status(ServiceStatus::Stopped {
                        generation,
                        hook: HookOutcome::NotRequired,
                    });
                    return TaskExit::Panicked {
                        source: PanicSource::Factory,
                        message,
                    };
                }
                match admit_restart(
                    &spec,
                    &mut restart_times,
                    &mut restart_count,
                    generation,
                    &state,
                    &work,
                )
                .await
                {
                    RestartAdmission::Admitted => {}
                    RestartAdmission::Rejected => {
                        return TaskExit::Failed(TaskFailure::Service(
                            ServiceFailure::RestartLimitExceeded {
                                generation,
                                last_error: None,
                            },
                        ));
                    }
                    RestartAdmission::CounterOverflow { code } => {
                        return TaskExit::Failed(TaskFailure::Service(
                            ServiceFailure::CounterOverflow {
                                code,
                                last_error: None,
                            },
                        ));
                    }
                }
                let Some(next_generation) = generation.checked_add(1) else {
                    return TaskExit::Failed(TaskFailure::Service(
                        ServiceFailure::GenerationOverflow,
                    ));
                };
                generation = next_generation;
                continue;
            }
        };
        let mut body = AbortOnDrop::new(tokio::spawn(future));
        let joined = tokio::select! {
            biased;
            _ = work.cancelled() => {
                state.set_status(ServiceStatus::Stopping { generation });
                work.set_cleanup_pending(CleanupPhase::ShutdownHook);
                let hook = run_shutdown_hook(&spec, context).await;
                work.finish_cleanup(hook_cleanup_outcome(&hook));
                let body_result = body.join().await;
                work.close_attempt_children_and_wait().await;
                state.set_status(ServiceStatus::Stopped { generation, hook });
                return match body_result {
                    Ok(Ok(value)) => TaskExit::Completed(value),
                    Ok(Err(error)) => TaskExit::Failed(TaskFailure::Service(
                        ServiceFailure::BodyFailed { generation, error },
                    )),
                    Err(error) if error.is_panic() => {
                        let Some(payload) = crate::internal::take_join_panic(
                            error,
                            "BB-SERVICE-CANCEL-JOIN-PANIC-MISCLASSIFIED",
                        ) else {
                            return TaskExit::ExecutorStopped {
                                reason: ExecutorStopReason::InternalInvariantViolation,
                            };
                        };
                        let message = panic_message(payload);
                        if execution::is_timer_unavailable_message(&message) {
                            TaskExit::Failed(TaskFailure::TimerUnavailable)
                        } else {
                            TaskExit::Panicked {
                                source: PanicSource::ServiceBody,
                                message,
                            }
                        }
                    }
                    Err(_) => TaskExit::Cancelled { reason: cancel_reason(&work) },
                };
            }
            result = body.join() => result,
        };
        work.close_attempt_children_and_wait().await;
        if work.is_cancelled() {
            state.set_status(ServiceStatus::Stopped {
                generation,
                hook: HookOutcome::NotRequired,
            });
            return TaskExit::Cancelled {
                reason: cancel_reason(&work),
            };
        }
        match joined {
            Ok(Ok(value)) => {
                state.set_status(ServiceStatus::Stopped {
                    generation,
                    hook: HookOutcome::NotRequired,
                });
                return TaskExit::Completed(value);
            }
            Ok(Err(error)) => {
                if !spec.restart_trigger.failure() {
                    state.set_status(ServiceStatus::Stopped {
                        generation,
                        hook: HookOutcome::NotRequired,
                    });
                    return TaskExit::Failed(TaskFailure::Service(ServiceFailure::BodyFailed {
                        generation,
                        error,
                    }));
                }
                match admit_restart(
                    &spec,
                    &mut restart_times,
                    &mut restart_count,
                    generation,
                    &state,
                    &work,
                )
                .await
                {
                    RestartAdmission::Admitted => {}
                    RestartAdmission::Rejected => {
                        state.set_status(ServiceStatus::Stopped {
                            generation,
                            hook: HookOutcome::NotRequired,
                        });
                        return TaskExit::Failed(TaskFailure::Service(
                            ServiceFailure::RestartLimitExceeded {
                                generation,
                                last_error: Some(error),
                            },
                        ));
                    }
                    RestartAdmission::CounterOverflow { code } => {
                        state.set_status(ServiceStatus::Stopped {
                            generation,
                            hook: HookOutcome::NotRequired,
                        });
                        return TaskExit::Failed(TaskFailure::Service(
                            ServiceFailure::CounterOverflow {
                                code,
                                last_error: Some(error),
                            },
                        ));
                    }
                }
            }
            Err(error) if error.is_panic() => {
                let Some(payload) =
                    crate::internal::take_join_panic(error, "BB-SERVICE-JOIN-PANIC-MISCLASSIFIED")
                else {
                    return TaskExit::ExecutorStopped {
                        reason: ExecutorStopReason::InternalInvariantViolation,
                    };
                };
                let message = panic_message(payload);
                if execution::is_timer_unavailable_message(&message) {
                    state.set_status(ServiceStatus::Stopped {
                        generation,
                        hook: HookOutcome::NotRequired,
                    });
                    return TaskExit::Failed(TaskFailure::TimerUnavailable);
                }
                if !spec.restart_trigger.panic() {
                    state.set_status(ServiceStatus::Stopped {
                        generation,
                        hook: HookOutcome::NotRequired,
                    });
                    return TaskExit::Panicked {
                        source: PanicSource::ServiceBody,
                        message,
                    };
                }
                match admit_restart(
                    &spec,
                    &mut restart_times,
                    &mut restart_count,
                    generation,
                    &state,
                    &work,
                )
                .await
                {
                    RestartAdmission::Admitted => {}
                    RestartAdmission::Rejected => {
                        state.set_status(ServiceStatus::Stopped {
                            generation,
                            hook: HookOutcome::NotRequired,
                        });
                        return TaskExit::Failed(TaskFailure::Service(
                            ServiceFailure::RestartLimitExceeded {
                                generation,
                                last_error: None,
                            },
                        ));
                    }
                    RestartAdmission::CounterOverflow { code } => {
                        state.set_status(ServiceStatus::Stopped {
                            generation,
                            hook: HookOutcome::NotRequired,
                        });
                        return TaskExit::Failed(TaskFailure::Service(
                            ServiceFailure::CounterOverflow {
                                code,
                                last_error: None,
                            },
                        ));
                    }
                }
            }
            Err(_) => {
                return TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RuntimeUnavailable,
                };
            }
        }
        let Some(next_generation) = generation.checked_add(1) else {
            return TaskExit::Failed(TaskFailure::Service(ServiceFailure::GenerationOverflow));
        };
        generation = next_generation;
    }
}

fn hook_cleanup_outcome(hook: &HookOutcome) -> CleanupOutcome {
    match hook {
        HookOutcome::NotRequired => CleanupOutcome::NotRequired,
        HookOutcome::Completed => CleanupOutcome::Completed,
        HookOutcome::Failed(message) => CleanupOutcome::Failed(message.to_string()),
        HookOutcome::Panicked => CleanupOutcome::Failed("shutdown hook panicked".to_string()),
        HookOutcome::TimedOut => CleanupOutcome::Failed("shutdown hook timed out".to_string()),
    }
}

enum RestartAdmission {
    Admitted,
    Rejected,
    CounterOverflow { code: &'static str },
}

const SERVICE_RESTART_COUNT_OVERFLOW: &str = "BB-SERVICE-RESTART-COUNT-OVERFLOW";

fn next_restart_count(current: u32) -> Result<u32, &'static str> {
    current.checked_add(1).ok_or(SERVICE_RESTART_COUNT_OVERFLOW)
}

async fn admit_restart<T, E>(
    spec: &ServiceSpec<T, E>,
    restart_times: &mut VecDeque<Instant>,
    restart_count: &mut u32,
    generation: u64,
    state: &ServiceState,
    work: &WorkContext,
) -> RestartAdmission {
    let Some(policy) = &spec.restart_policy else {
        return RestartAdmission::Rejected;
    };
    let now = Instant::now();
    while restart_times
        .front()
        .is_some_and(|at| now.duration_since(*at) > policy.configured_window())
    {
        restart_times.pop_front();
    }
    if restart_times.len() >= policy.maximum_restarts() as usize || work.is_cancelled() {
        return RestartAdmission::Rejected;
    }
    let next_restart_count = match next_restart_count(*restart_count) {
        Ok(next) => next,
        Err(code) => {
            crate::internal::log_internal_error(
                code,
                "service cumulative restart counter overflowed",
            );
            return RestartAdmission::CounterOverflow { code };
        }
    };
    restart_times.push_back(now);
    *restart_count = next_restart_count;
    state.set_status(ServiceStatus::Restarting {
        generation,
        restart: *restart_count,
    });
    if work.sleep(policy.configured_backoff()).await.is_ok() {
        RestartAdmission::Admitted
    } else {
        RestartAdmission::Rejected
    }
}

async fn run_shutdown_hook<T, E>(spec: &ServiceSpec<T, E>, context: ServiceContext) -> HookOutcome {
    let Some(hook) = &spec.shutdown_hook else {
        return HookOutcome::NotRequired;
    };
    let future = match catch_unwind(AssertUnwindSafe(|| hook(context))) {
        Ok(future) => future,
        Err(_) => return HookOutcome::Panicked,
    };
    let mut task = AbortOnDrop::new(tokio::spawn(future));
    match tokio::time::timeout(spec.hook_timeout, task.join()).await {
        Ok(Ok(Ok(()))) => HookOutcome::Completed,
        Ok(Ok(Err(error))) => HookOutcome::Failed(Arc::from(error)),
        Ok(Err(error)) if error.is_panic() => HookOutcome::Panicked,
        Ok(Err(_)) => HookOutcome::Failed(Arc::from("hook runtime unavailable")),
        Err(_) => HookOutcome::TimedOut,
    }
}

fn cancel_reason(work: &WorkContext) -> CancelReason {
    match work.stop_cause() {
        Some(crate::StopCauseSummary::Cancel(reason)) => reason,
        _ => CancelReason::ExecutorShutdown,
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    if let Ok(message) = payload.downcast::<String>() {
        return *message;
    }
    "panic (unknown payload)".to_string()
}

#[cfg(test)]
mod tests {
    use super::{next_restart_count, ServiceFailure, SERVICE_RESTART_COUNT_OVERFLOW};

    #[test]
    fn service_restart_counter_reports_a_stable_overflow_code() -> Result<(), &'static str> {
        if next_restart_count(0) != Ok(1) {
            return Err("valid restart count did not advance");
        }
        if next_restart_count(u32::MAX) != Err(SERVICE_RESTART_COUNT_OVERFLOW) {
            return Err("restart counter overflow did not return its stable code");
        }
        let failure = ServiceFailure::<()>::CounterOverflow {
            code: SERVICE_RESTART_COUNT_OVERFLOW,
            last_error: None,
        };
        if failure.code() != SERVICE_RESTART_COUNT_OVERFLOW {
            return Err("service failure did not preserve its stable code");
        }
        Ok(())
    }
}
