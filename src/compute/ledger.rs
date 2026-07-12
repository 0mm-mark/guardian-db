//! # Task ledger (Phase 4)
//!
//! The recorded lifecycle of every compute task (RFC §5.6): who claimed it,
//! where it ran, what it cost, and — crucially — enough state for *another*
//! node (or a restarted one) to notice an expired task and requeue it.
//!
//! The ledger is written through the [`LedgerStore`] abstraction so the same
//! lifecycle logic runs over any backing store:
//!
//! - [`MemoryLedger`] — process-local, with truly atomic conditional writes;
//!   the default for single-node use and tests.
//! - A replicated GuardianDB store (KV/document) — the app implements the
//!   four methods over its store, gaining replicated auditability. Note the
//!   RFC §5.7 caveat: over an LWW store, `create_if_absent` is only
//!   best-effort across the network — rare races yield duplicate execution,
//!   which is why triggered tasks should be idempotent. (Under RFC 0001's
//!   `Strict` mode the conditional write becomes exact.)
//!
//! State machine: `Pending → Running → Done | Failed`, with
//! `Running` past its deadline treated as abandoned and requeued to
//! `Pending` until [`TaskRecord::attempts`] exhausts the retry budget.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use iroh::EndpointId as NodeId;
use iroh_blobs::Hash;
use serde::{Deserialize, Serialize};

use super::protocol::CompletedTask;
use super::{ResourceLimits, TaskClass};

/// Stable identifier of a ledger entry. For triggered tasks this is the
/// deterministic dedup key (hash of rule + event), which is exactly what
/// makes the conditional write idempotent across replicas.
pub type TaskKey = String;

/// Where a task is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// Claimed but not yet dispatched (or requeued after an expiry/failure).
    Pending,
    /// Dispatched; if `deadline_unix` passes without completion, the task is
    /// considered abandoned and eligible for requeue.
    Running { deadline_unix: u64 },
    /// Completed successfully.
    Done {
        /// The node that executed it.
        executor: NodeId,
    },
    /// Terminal failure (deterministic task error, or retry budget spent).
    Failed { error: String },
}

/// One task's full record in the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub key: TaskKey,
    /// BLAKE3 hash of the `.wasm` blob to run.
    pub wasm_hash: Hash,
    pub entrypoint: String,
    pub class: TaskClass,
    pub limits: ResourceLimits,
    /// Input bytes, kept in the record so a requeue can re-dispatch without
    /// the original event still being around.
    pub input: Vec<u8>,
    /// NN model the task needs (phase NN-3); threaded from the trigger rule's
    /// [`TaskSpec`](super::TaskSpec) into the dispatched `ExecuteRequest`.
    pub required_model: Option<String>,
    pub state: TaskState,
    /// Dispatch attempts so far (0 while still Pending for the first time).
    pub attempts: u32,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    /// Output + metrics once `Done`.
    pub result: Option<CompletedTask>,
}

impl TaskRecord {
    /// A fresh `Pending` record.
    #[allow(clippy::too_many_arguments)]
    pub fn pending(
        key: TaskKey,
        wasm_hash: Hash,
        entrypoint: String,
        class: TaskClass,
        limits: ResourceLimits,
        input: Vec<u8>,
        required_model: Option<String>,
    ) -> Self {
        let now = unix_now();
        Self {
            key,
            wasm_hash,
            entrypoint,
            class,
            limits,
            input,
            required_model,
            state: TaskState::Pending,
            attempts: 0,
            created_at_unix: now,
            updated_at_unix: now,
            result: None,
        }
    }

    /// Whether this record should be (re)dispatched at `now`: `Pending`, or
    /// `Running` past its deadline (the dispatcher vanished mid-flight).
    pub fn needs_dispatch(&self, now_unix: u64) -> bool {
        match &self.state {
            TaskState::Pending => true,
            TaskState::Running { deadline_unix } => *deadline_unix < now_unix,
            TaskState::Done { .. } | TaskState::Failed { .. } => false,
        }
    }
}

/// Minimal storage surface the ledger needs. Implement it over a replicated
/// GuardianDB store to make the ledger network-visible; [`MemoryLedger`] is
/// the process-local reference implementation.
#[async_trait]
pub trait LedgerStore: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    async fn put(&self, key: &str, value: Vec<u8>) -> Result<(), String>;
    /// Writes `value` only if `key` is absent; returns whether it was written.
    /// This is the trigger-dedup primitive (RFC §5.7): the claim on a task.
    async fn create_if_absent(&self, key: &str, value: Vec<u8>) -> Result<bool, String>;
    /// Writes `value` only if the current stored bytes equal `expected`;
    /// returns whether the swap happened. The dispatch-ownership primitive:
    /// two dispatchers racing to claim the same task both read the same
    /// `expected`, but only one CAS succeeds. Exact on [`MemoryLedger`];
    /// best-effort over an LWW store (same §5.7 caveat as `create_if_absent`).
    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<bool, String>;
    /// All stored records (the ledger is small; scans are fine at this scale).
    async fn list(&self) -> Result<Vec<Vec<u8>>, String>;
}

/// In-memory [`LedgerStore`] with exact conditional-write semantics.
#[derive(Debug, Default)]
pub struct MemoryLedger {
    entries: parking_lot::Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LedgerStore for MemoryLedger {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.entries.lock().get(key).cloned())
    }

    async fn put(&self, key: &str, value: Vec<u8>) -> Result<(), String> {
        self.entries.lock().insert(key.to_string(), value);
        Ok(())
    }

    async fn create_if_absent(&self, key: &str, value: Vec<u8>) -> Result<bool, String> {
        let mut entries = self.entries.lock();
        if entries.contains_key(key) {
            return Ok(false);
        }
        entries.insert(key.to_string(), value);
        Ok(true)
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<bool, String> {
        let mut entries = self.entries.lock();
        if entries.get(key).map(|v| v.as_slice()) == expected.as_deref() {
            entries.insert(key.to_string(), value);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn list(&self) -> Result<Vec<Vec<u8>>, String> {
        Ok(self.entries.lock().values().cloned().collect())
    }
}

/// Typed lifecycle operations over a [`LedgerStore`].
#[derive(Clone)]
pub struct TaskLedger {
    store: Arc<dyn LedgerStore>,
}

impl std::fmt::Debug for TaskLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskLedger").finish_non_exhaustive()
    }
}

impl TaskLedger {
    pub fn new(store: Arc<dyn LedgerStore>) -> Self {
        Self { store }
    }

    /// In-memory ledger, for single-node use and tests.
    pub fn in_memory() -> Self {
        Self::new(Arc::new(MemoryLedger::new()))
    }

    /// Claims a task: records it as `Pending` only if no record exists yet.
    /// Returns whether *this* call made the claim (the dedup verdict).
    pub async fn claim(&self, record: &TaskRecord) -> Result<bool, String> {
        let bytes = postcard::to_stdvec(record).map_err(|e| e.to_string())?;
        self.store.create_if_absent(&record.key, bytes).await
    }

    pub async fn get(&self, key: &str) -> Result<Option<TaskRecord>, String> {
        match self.store.get(key).await? {
            Some(bytes) => postcard::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }

    /// Atomically claims a task for dispatch: transitions it to `Running`
    /// (incrementing `attempts`) **only if** it is currently dispatchable
    /// (`Pending`, or `Running` past its deadline). Returns the now-`Running`
    /// record if this caller won the claim, or `None` if the task is absent,
    /// already terminal, or another dispatcher claimed it first.
    ///
    /// This is what stops the requeue loop and the reactive path from
    /// double-dispatching the same task: both read the same bytes but only
    /// one `compare_and_swap` succeeds.
    pub async fn claim_for_dispatch(
        &self,
        key: &str,
        deadline_unix: u64,
    ) -> Result<Option<TaskRecord>, String> {
        loop {
            let now = unix_now();
            let Some(bytes) = self.store.get(key).await? else {
                return Ok(None);
            };
            let record: TaskRecord = postcard::from_bytes(&bytes).map_err(|e| e.to_string())?;
            if !record.needs_dispatch(now) {
                return Ok(None); // terminal, or already Running within deadline
            }
            let mut running = record.clone();
            running.attempts += 1;
            running.state = TaskState::Running { deadline_unix };
            running.updated_at_unix = now;
            let new_bytes = postcard::to_stdvec(&running).map_err(|e| e.to_string())?;
            if self
                .store
                .compare_and_swap(key, Some(bytes), new_bytes)
                .await?
            {
                return Ok(Some(running));
            }
            // Lost the race (record changed under us): re-read and re-evaluate.
        }
    }

    /// Read-modify-write of one record. Returns the updated record, or
    /// `None` when the key does not exist.
    pub async fn update<F>(&self, key: &str, mutate: F) -> Result<Option<TaskRecord>, String>
    where
        F: FnOnce(&mut TaskRecord),
    {
        let Some(mut record) = self.get(key).await? else {
            return Ok(None);
        };
        mutate(&mut record);
        record.updated_at_unix = unix_now();
        let bytes = postcard::to_stdvec(&record).map_err(|e| e.to_string())?;
        self.store.put(key, bytes).await?;
        Ok(Some(record))
    }

    /// All records currently in the ledger.
    pub async fn all(&self) -> Result<Vec<TaskRecord>, String> {
        self.store
            .list()
            .await?
            .into_iter()
            .map(|bytes| postcard::from_bytes(&bytes).map_err(|e| e.to_string()))
            .collect()
    }

    /// Records that should be (re)dispatched now: `Pending` plus `Running`
    /// past deadline (abandoned by a vanished dispatcher).
    pub async fn needing_dispatch(&self) -> Result<Vec<TaskRecord>, String> {
        let now = unix_now();
        Ok(self
            .all()
            .await?
            .into_iter()
            .filter(|r| r.needs_dispatch(now))
            .collect())
    }
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str) -> TaskRecord {
        TaskRecord::pending(
            key.to_string(),
            Hash::new(b"wasm"),
            "gdb_run".into(),
            TaskClass::Media,
            ResourceLimits::default(),
            b"photo bytes".to_vec(),
            None,
        )
    }

    #[tokio::test]
    async fn claim_is_idempotent() {
        let ledger = TaskLedger::in_memory();
        let rec = record("task-1");
        assert!(ledger.claim(&rec).await.unwrap(), "first claim wins");
        assert!(!ledger.claim(&rec).await.unwrap(), "second claim loses");
        // The stored record is the first one, untouched.
        let stored = ledger.get("task-1").await.unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Pending);
        assert_eq!(stored.attempts, 0);
    }

    #[tokio::test]
    async fn lifecycle_transitions_and_result() {
        let ledger = TaskLedger::in_memory();
        ledger.claim(&record("t")).await.unwrap();

        let running = ledger
            .update("t", |r| {
                r.attempts += 1;
                r.state = TaskState::Running {
                    deadline_unix: unix_now() + 60,
                };
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(running.attempts, 1);
        assert!(
            !running.needs_dispatch(unix_now()),
            "running within deadline"
        );

        let executor = iroh::SecretKey::generate().public();
        let done = ledger
            .update("t", |r| {
                r.state = TaskState::Done { executor };
                r.result = Some(CompletedTask {
                    output: b"thumb".to_vec(),
                    metrics: crate::compute::ExecMetrics {
                        fuel_consumed: 42,
                        duration_ms: 7,
                        peak_memory_bytes: 65_536,
                    },
                });
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            !done.needs_dispatch(unix_now() + 10_000),
            "done is terminal"
        );
        assert_eq!(done.result.unwrap().output, b"thumb");
    }

    #[tokio::test]
    async fn expired_running_needs_dispatch_again() {
        let ledger = TaskLedger::in_memory();
        ledger.claim(&record("t")).await.unwrap();
        ledger
            .update("t", |r| {
                r.attempts = 1;
                r.state = TaskState::Running {
                    deadline_unix: unix_now() - 5, // already past
                };
            })
            .await
            .unwrap();

        let due = ledger.needing_dispatch().await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].key, "t");
    }

    #[tokio::test]
    async fn update_of_missing_key_is_none() {
        let ledger = TaskLedger::in_memory();
        assert!(ledger.update("ghost", |_| {}).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn only_one_dispatcher_claims_a_pending_task() {
        // Two concurrent claim_for_dispatch on the same Pending task: exactly
        // one wins (the atomic CAS), the other gets None — this is what stops
        // the requeue loop and the reactive path from double-dispatching.
        let ledger = TaskLedger::in_memory();
        ledger.claim(&record("t")).await.unwrap();

        let deadline = unix_now() + 60;
        let (a, b) = tokio::join!(
            ledger.claim_for_dispatch("t", deadline),
            ledger.claim_for_dispatch("t", deadline),
        );
        let claimed = [a.unwrap(), b.unwrap()];
        assert_eq!(
            claimed.iter().filter(|c| c.is_some()).count(),
            1,
            "exactly one claim wins"
        );
        let winner = claimed.iter().find_map(|c| c.clone()).unwrap();
        assert_eq!(winner.attempts, 1);
        assert!(matches!(winner.state, TaskState::Running { .. }));

        // A third attempt now sees Running-within-deadline and does not claim.
        assert!(
            ledger
                .claim_for_dispatch("t", deadline)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn claim_for_dispatch_ignores_terminal_and_absent() {
        let ledger = TaskLedger::in_memory();
        assert!(
            ledger
                .claim_for_dispatch("ghost", unix_now() + 60)
                .await
                .unwrap()
                .is_none(),
            "absent key"
        );

        ledger.claim(&record("done")).await.unwrap();
        let executor = iroh::SecretKey::generate().public();
        ledger
            .update("done", |r| r.state = TaskState::Done { executor })
            .await
            .unwrap();
        assert!(
            ledger
                .claim_for_dispatch("done", unix_now() + 60)
                .await
                .unwrap()
                .is_none(),
            "terminal task is not re-claimed"
        );
    }
}
