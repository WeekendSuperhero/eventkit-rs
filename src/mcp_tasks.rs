//! MCP task surface (SEP-1686) for the EventKit server.
//!
//! Some EventKit operations are genuinely slow — a full-calendar scan, a
//! multi-hundred-item `batch_*` — slow enough that a host may time the call out
//! before it finishes. A task-augmented `tools/call` returns a task id
//! immediately and runs the work in the background; the caller polls
//! `tasks/get` / `tasks/result`, or waits for the status notification the
//! server pushes on every transition.
//!
//! Deliberately leaner than `agentmail-mcp`'s equivalent. That server needs
//! per-account serialization of destructive work and opaque paging cursors
//! because IMAP mutations race across accounts. EventKit has one local store
//! and no accounts, so this keeps the parts that matter — bounded memory, a
//! result that survives repeated polls, real cancellation — and skips the rest.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock, Task, TaskStatus};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How long a terminal task stays queryable before pruning.
pub(super) const TASK_TTL_MS: u64 = 3_600_000; // 1 hour

/// Concurrently RUNNING tasks. EventKit work is I/O against a single local
/// store; letting an agent queue hundreds of scans would thrash it.
pub(super) const MAX_ACTIVE_TASKS: usize = 16;

/// Total tracked tasks, running plus terminal-but-not-yet-pruned.
pub(super) const MAX_TRACKED_TASKS: usize = 256;

/// Max tasks returned by one `tasks/list`.
pub(super) const TASK_PAGE_SIZE: usize = 50;

/// Current time as an RFC 3339 string, matching `Task`'s timestamp fields.
pub(super) fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// A task's result slot plus a wake-up for anyone awaiting it.
#[derive(Default)]
pub(super) struct TaskCompletion {
    result: parking_lot::Mutex<Option<Result<CallToolResult, McpError>>>,
    changed: Notify,
}

impl TaskCompletion {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Publish the worker's result — first write wins.
    ///
    /// First-write-wins matters for cancellation: `cancel` publishes a
    /// cancelled result immediately, and a worker that finishes a moment later
    /// must not overwrite it with a success the caller was never promised.
    pub(super) fn complete(&self, result: Result<CallToolResult, McpError>) {
        let mut slot = self.result.lock();
        if slot.is_none() {
            *slot = Some(result);
            drop(slot);
            self.changed.notify_waiters();
        }
    }

    fn cancel(&self, task_id: &str) {
        self.complete(Ok(CallToolResult::error(vec![ContentBlock::text(
            format!("Task {task_id} was cancelled before completion."),
        )])));
    }

    fn snapshot(&self) -> Option<Result<CallToolResult, McpError>> {
        self.result.lock().clone()
    }

    /// Block until the task reaches a result.
    ///
    /// `request_cancel` cancels THIS `tasks/result` request, not the task —
    /// a caller giving up on waiting must not kill work another caller may
    /// still want.
    pub(super) async fn wait(
        &self,
        request_cancel: &CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Arm BEFORE snapshotting, or a result published in between would
            // be missed and this would wait forever.
            changed.as_mut().enable();
            if let Some(result) = self.snapshot() {
                return result;
            }
            tokio::select! {
                () = &mut changed => {}
                () = request_cancel.cancelled() => {
                    return Err(McpError::internal_error(
                        "tasks/result request was cancelled",
                        None,
                    ));
                }
            }
        }
    }
}

/// Marks a task terminal if its worker dies without reporting.
///
/// **Why this exists.** A task is moved out of `Working` in exactly one place:
/// the worker itself, *after* `call_tool` returns. If that call unwinds, neither
/// the result nor the transition is ever published, and the task is stranded:
/// `prune_expired` skips it (it only prunes tasks with a `terminal_at_ms`), it
/// holds an active slot forever, and any `tasks/result` waiter blocks on a slot
/// that will never be filled. After [`MAX_ACTIVE_TASKS`] such panics `admit`
/// refuses *every* future task for the life of the process.
///
/// That is not hypothetical for this crate. `objc2` declares most EventKit
/// accessors non-null while EventKit returns NULL to an unauthorized process,
/// which panics — the failure mode `AUTHORIZATION_PLAN.md` records as a live
/// outage — and several conversion paths unwrap on user data (a reminder URL
/// whose `absoluteString` is nil, an out-of-range date, a local time inside a
/// DST gap). Nothing catches those: this crate and rmcp both run without
/// `catch_unwind`, and there is no `panic = "abort"`, so the unwind stops at the
/// tokio task boundary and simply discards the rest of the worker.
///
/// `Drop` runs during that unwind, which is what makes a guard the right shape
/// here rather than a `catch_unwind` the async boundary cannot offer. It is also
/// correct for a plain early return, should the worker ever grow one.
///
/// The guard publishes through [`TaskCompletion::complete`], whose first-write-
/// wins rule means a cancellation that already landed is never overwritten.
pub(super) struct TaskFailureGuard {
    task_id: String,
    completion: Arc<TaskCompletion>,
    manager: Arc<parking_lot::Mutex<TaskManager>>,
    armed: bool,
}

impl TaskFailureGuard {
    pub(super) fn new(
        task_id: String,
        completion: Arc<TaskCompletion>,
        manager: Arc<parking_lot::Mutex<TaskManager>>,
    ) -> Self {
        Self {
            task_id,
            completion,
            manager,
            armed: true,
        }
    }

    /// The worker reported for itself — stand down.
    pub(super) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TaskFailureGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.completion
            .complete(Ok(CallToolResult::error(vec![ContentBlock::text(
                format!(
                    "Task {} failed: the tool panicked and produced no result.",
                    self.task_id
                ),
            )])));
        self.manager
            .lock()
            .transition(&self.task_id, TaskStatus::Failed);
    }
}

/// One tracked task.
pub(super) struct ManagedTask {
    pub(super) meta: Task,
    pub(super) completion: Arc<TaskCompletion>,
    pub(super) cancel: CancellationToken,
    pub(super) handle: JoinHandle<()>,
    /// When this task reached a terminal state, for TTL pruning.
    terminal_at_ms: Option<u64>,
}

impl ManagedTask {
    /// Build a running task. `terminal_at_ms` stays private so only
    /// [`TaskManager`] can mark a task terminal — the TTL clock must not be
    /// settable from outside, or pruning becomes unpredictable.
    pub(super) fn running(
        meta: Task,
        completion: Arc<TaskCompletion>,
        cancel: CancellationToken,
        handle: JoinHandle<()>,
    ) -> Self {
        Self {
            meta,
            completion,
            cancel,
            handle,
            terminal_at_ms: None,
        }
    }

    fn is_terminal(&self) -> bool {
        !matches!(
            self.meta.status,
            TaskStatus::Working | TaskStatus::InputRequired
        )
    }
}

/// Tracked tasks, oldest first.
///
/// A `Vec` rather than a map: `MAX_TRACKED_TASKS` is 256, so a linear scan by
/// id is trivially fast, and insertion order — which paging and oldest-first
/// eviction both need — comes for free. A map would have required a parallel
/// order vec that could drift out of sync with it.
#[derive(Default)]
pub(super) struct TaskManager {
    tasks: Vec<(String, ManagedTask)>,
}

impl TaskManager {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn active_count(&self) -> usize {
        self.tasks.iter().filter(|(_, t)| !t.is_terminal()).count()
    }

    fn find(&self, task_id: &str) -> Option<&ManagedTask> {
        self.tasks
            .iter()
            .find(|(id, _)| id == task_id)
            .map(|(_, t)| t)
    }

    fn find_mut(&mut self, task_id: &str) -> Option<&mut ManagedTask> {
        self.tasks
            .iter_mut()
            .find(|(id, _)| id == task_id)
            .map(|(_, t)| t)
    }

    fn unknown(task_id: &str) -> McpError {
        McpError::invalid_params(format!("unknown task: {task_id}"), None)
    }

    /// Admit a new task, or refuse with a descriptive error.
    ///
    /// Refusing loudly beats silently queueing: an agent that has saturated the
    /// server should learn now, not after N pending scans finish.
    pub(super) fn admit(&mut self) -> Result<(), McpError> {
        self.prune_expired();
        if self.active_count() >= MAX_ACTIVE_TASKS {
            return Err(McpError::internal_error(
                format!(
                    "too many tasks already running (limit {MAX_ACTIVE_TASKS}); \
                         wait for one to finish or cancel it with tasks/cancel"
                ),
                None,
            ));
        }
        if self.tasks.len() >= MAX_TRACKED_TASKS {
            self.evict_oldest_terminal();
        }
        Ok(())
    }

    pub(super) fn insert(&mut self, task_id: String, task: ManagedTask) {
        self.tasks.push((task_id, task));
    }

    /// Record a status transition. Returns the updated `Task` when the status
    /// ACTUALLY changed, so callers push a notification exactly once.
    ///
    /// **Terminal is final.** A worker that finishes a moment after
    /// `tasks/cancel` must not downgrade `Cancelled` to `Completed`/`Failed`,
    /// and neither must [`TaskFailureGuard`] when that worker panics instead.
    /// The caller was already told the task was cancelled, and
    /// [`TaskCompletion::complete`] has already frozen the matching result —
    /// letting the status drift would contradict both. Refusing here is also
    /// what makes the "exactly once" promise above true for the cancel race,
    /// which is otherwise the one case that would push a second notification.
    pub(super) fn transition(&mut self, task_id: &str, status: TaskStatus) -> Option<Task> {
        let entry = self.find_mut(task_id)?;
        if entry.is_terminal() || entry.meta.status == status {
            return None;
        }
        entry.meta.status = status;
        entry.meta.last_updated_at = now_iso8601();
        if entry.is_terminal() {
            entry.terminal_at_ms = Some(now_millis());
        }
        Some(entry.meta.clone())
    }

    pub(super) fn task_info(&self, task_id: &str) -> Result<Task, McpError> {
        self.find(task_id)
            .map(|t| t.meta.clone())
            .ok_or_else(|| Self::unknown(task_id))
    }

    pub(super) fn completion(&self, task_id: &str) -> Result<Arc<TaskCompletion>, McpError> {
        self.find(task_id)
            .map(|t| Arc::clone(&t.completion))
            .ok_or_else(|| Self::unknown(task_id))
    }

    /// Cancel a task: signal the worker, publish a cancelled result so any
    /// waiter unblocks, and mark it terminal.
    pub(super) fn cancel_task(&mut self, task_id: &str) -> Result<Task, McpError> {
        let entry = self
            .find_mut(task_id)
            .ok_or_else(|| Self::unknown(task_id))?;
        if entry.is_terminal() {
            // Already finished — cancelling is a no-op, not an error. Repeat
            // cancels are idempotent (see the tool's `idempotent_hint`).
            return Ok(entry.meta.clone());
        }
        entry.cancel.cancel();
        entry.completion.cancel(task_id);
        // Abort the worker too. The token asks it to stop COOPERATIVELY, which
        // most EventKit work cannot honour — the objc calls are synchronous and
        // never observe the token — so without this the future would keep
        // running to completion after the caller was told it was cancelled.
        entry.handle.abort();
        entry.meta.status = TaskStatus::Cancelled;
        entry.meta.last_updated_at = now_iso8601();
        entry.terminal_at_ms = Some(now_millis());
        Ok(entry.meta.clone())
    }

    /// One page of tasks, newest first.
    pub(super) fn list_page(&mut self) -> Vec<Task> {
        self.prune_expired();
        self.tasks
            .iter()
            .rev()
            .map(|(_, t)| t.meta.clone())
            .take(TASK_PAGE_SIZE)
            .collect()
    }

    /// Drop terminal tasks past their TTL.
    pub(super) fn prune_expired(&mut self) {
        let now = now_millis();
        self.tasks.retain(|(_, t)| {
            !t.terminal_at_ms
                .is_some_and(|at| now.saturating_sub(at) > TASK_TTL_MS)
        });
    }

    fn evict_oldest_terminal(&mut self) {
        if let Some(pos) = self.tasks.iter().position(|(_, t)| t.is_terminal()) {
            self.tasks.remove(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(status: TaskStatus) -> ManagedTask {
        let now = now_iso8601();
        ManagedTask::running(
            Task::new("t".to_string(), status, now.clone(), now),
            Arc::new(TaskCompletion::new()),
            CancellationToken::new(),
            tokio::runtime::Handle::try_current()
                .map(|h| h.spawn(async {}))
                .expect("tests run on a tokio runtime"),
        )
    }

    /// `transition` must report a change EXACTLY once — the caller pushes a
    /// `notifications/tasks/status` off the return value, so a repeat would
    /// duplicate the notification and a miss would lose it.
    #[tokio::test]
    async fn transition_reports_a_change_exactly_once() {
        let mut mgr = TaskManager::new();
        mgr.insert("t".into(), managed(TaskStatus::Working));

        assert!(
            mgr.transition("t", TaskStatus::Completed).is_some(),
            "first transition must report the change"
        );
        assert!(
            mgr.transition("t", TaskStatus::Completed).is_none(),
            "repeat transition to the SAME status must report nothing"
        );
        assert!(
            mgr.transition("unknown", TaskStatus::Completed).is_none(),
            "unknown id must report nothing"
        );
    }

    /// Cancelling must unblock a waiter rather than leave it hanging.
    #[tokio::test]
    async fn cancel_publishes_a_result_so_waiters_wake() {
        let mut mgr = TaskManager::new();
        mgr.insert("t".into(), managed(TaskStatus::Working));
        let completion = mgr.completion("t").expect("task exists");

        let cancelled = mgr.cancel_task("t").expect("cancel succeeds");
        assert_eq!(cancelled.status, TaskStatus::Cancelled);

        let result = completion
            .wait(&CancellationToken::new())
            .await
            .expect("a cancelled task still yields a result");
        assert_eq!(result.is_error, Some(true));
    }

    /// Cancelling an already-terminal task is a no-op, not an error — the
    /// tool advertises `idempotent_hint = true`.
    #[tokio::test]
    async fn cancelling_twice_is_idempotent() {
        let mut mgr = TaskManager::new();
        mgr.insert("t".into(), managed(TaskStatus::Working));
        mgr.cancel_task("t").expect("first cancel");
        let second = mgr.cancel_task("t").expect("second cancel must not error");
        assert_eq!(second.status, TaskStatus::Cancelled);
    }

    /// A worker result must not overwrite a cancellation that already landed.
    #[tokio::test]
    async fn cancellation_wins_a_race_with_a_late_worker() {
        let completion = TaskCompletion::new();
        completion.cancel("t");
        completion.complete(Ok(CallToolResult::success(vec![ContentBlock::text(
            "late",
        )])));
        let result = completion
            .wait(&CancellationToken::new())
            .await
            .expect("result present");
        assert_eq!(
            result.is_error,
            Some(true),
            "the late success must NOT overwrite the cancellation"
        );
    }

    /// Admission must refuse once too many tasks are RUNNING, while terminal
    /// ones don't count against the cap.
    #[tokio::test]
    async fn admit_refuses_only_on_too_many_running() {
        let mut mgr = TaskManager::new();
        for i in 0..MAX_ACTIVE_TASKS {
            mgr.insert(format!("run{i}"), managed(TaskStatus::Working));
        }
        assert!(mgr.admit().is_err(), "a full slate must refuse");

        for i in 0..MAX_ACTIVE_TASKS {
            mgr.transition(&format!("run{i}"), TaskStatus::Completed);
        }
        assert!(
            mgr.admit().is_ok(),
            "terminal tasks must not count against the running cap"
        );
    }

    #[tokio::test]
    async fn unknown_task_lookups_error() {
        let mgr = TaskManager::new();
        assert!(mgr.task_info("nope").is_err());
        assert!(mgr.completion("nope").is_err());
    }

    // ── Panic containment ────────────────────────────────────────────────

    /// Spawn a worker that panics, with a guard watching it, and wait for it
    /// to die — mirroring `enqueue_task`'s worker minus the tool call.
    async fn spawn_panicking_worker(
        mgr: &Arc<parking_lot::Mutex<TaskManager>>,
        id: &str,
        completion: Arc<TaskCompletion>,
    ) {
        let guard = TaskFailureGuard::new(id.to_string(), completion, Arc::clone(mgr));
        let handle = tokio::spawn(async move {
            let _guard = guard;
            panic!("objc2: unexpected NULL returned from -[EKEventStore ...]");
        });
        assert!(
            handle.await.is_err(),
            "the worker must actually have panicked"
        );
    }

    /// THE wedge guard. Without [`TaskFailureGuard`], a panicking worker never
    /// reaches its `transition`, so the task stays `Working` forever: TTL
    /// pruning skips it and it holds an active slot permanently. After
    /// `MAX_ACTIVE_TASKS` panics, `admit` refuses every future task for the life
    /// of the process — background work is dead until the app restarts.
    ///
    /// This is the test that fails if someone removes the guard.
    #[tokio::test]
    async fn panicking_workers_do_not_wedge_admission() {
        let mgr = Arc::new(parking_lot::Mutex::new(TaskManager::new()));
        for i in 0..MAX_ACTIVE_TASKS {
            let id = format!("p{i}");
            let completion = Arc::new(TaskCompletion::new());
            mgr.lock().insert(
                id.clone(),
                ManagedTask::running(
                    Task::new(
                        id.clone(),
                        TaskStatus::Working,
                        now_iso8601(),
                        now_iso8601(),
                    ),
                    Arc::clone(&completion),
                    CancellationToken::new(),
                    tokio::spawn(async {}),
                ),
            );
            spawn_panicking_worker(&mgr, &id, completion).await;
        }

        let mut mgr = mgr.lock();
        assert_eq!(
            mgr.active_count(),
            0,
            "every panicked worker must have been marked terminal by its guard"
        );
        assert!(
            mgr.admit().is_ok(),
            "admission must still work after {MAX_ACTIVE_TASKS} panics"
        );
    }

    /// A panicking worker must also unblock anyone waiting on `tasks/result`.
    /// The result slot is filled by the worker, so an unwind would otherwise
    /// leave a waiter parked on a slot nothing will ever fill.
    #[tokio::test]
    async fn a_panicking_worker_still_yields_a_result() {
        let mgr = Arc::new(parking_lot::Mutex::new(TaskManager::new()));
        let completion = Arc::new(TaskCompletion::new());
        mgr.lock().insert(
            "t".into(),
            ManagedTask::running(
                Task::new(
                    "t".into(),
                    TaskStatus::Working,
                    now_iso8601(),
                    now_iso8601(),
                ),
                Arc::clone(&completion),
                CancellationToken::new(),
                tokio::spawn(async {}),
            ),
        );

        spawn_panicking_worker(&mgr, "t", Arc::clone(&completion)).await;

        let result = completion
            .wait(&CancellationToken::new())
            .await
            .expect("a panicked task must still produce a result");
        assert_eq!(result.is_error, Some(true), "and it must be an error");
        assert_eq!(
            mgr.lock().task_info("t").expect("task exists").status,
            TaskStatus::Failed
        );
    }

    /// The guard must not overwrite a cancellation that already landed —
    /// `complete` is first-write-wins, and the task is already terminal.
    #[tokio::test]
    async fn the_guard_does_not_clobber_an_earlier_cancellation() {
        let mgr = Arc::new(parking_lot::Mutex::new(TaskManager::new()));
        let completion = Arc::new(TaskCompletion::new());
        mgr.lock().insert(
            "t".into(),
            ManagedTask::running(
                Task::new(
                    "t".into(),
                    TaskStatus::Working,
                    now_iso8601(),
                    now_iso8601(),
                ),
                Arc::clone(&completion),
                CancellationToken::new(),
                tokio::spawn(async {}),
            ),
        );
        mgr.lock().cancel_task("t").expect("cancel succeeds");

        spawn_panicking_worker(&mgr, "t", Arc::clone(&completion)).await;

        assert_eq!(
            mgr.lock().task_info("t").expect("task exists").status,
            TaskStatus::Cancelled,
            "a late panic must NOT downgrade Cancelled to Failed"
        );
    }
}
