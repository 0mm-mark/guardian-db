//! # Compute execution protocol (Phase 2)
//!
//! Direct 1-to-1 task delegation over the dedicated [`COMPUTE_ALPN`], modeled
//! on the ticket exchange protocol:
//!
//! - **Requester** ([`ComputeClient::execute_on`]): opens a connection on the
//!   ALPN, sends one [`ComputeRequest`] — either `Execute` (two frames back:
//!   [`ExecuteAck`], then [`ExecuteReply`]) or a Contract-Net `Probe`
//!   (one [`ProbeReply`] frame with fresh readiness, Phase 5).
//! - **Executor** ([`ComputeProtocolHandler`]): applies its admission policy
//!   (task class + concurrency slots), fetches the `.wasm` blob by hash (from
//!   the requester itself via iroh-blobs, so integrity is verified by
//!   construction), compiles it (LRU-cached), runs it in the
//!   [`WasmRuntime`](super::runtime::WasmRuntime) sandbox and replies.
//!
//! Frames are `u32`-length-prefixed postcard messages. Input and output are
//! opaque byte strings (RFC §8.2). The requester is authenticated by the QUIC
//! TLS handshake (its `EndpointId`), which is what lets the executor fetch the
//! wasm blob *from the requester* without further ceremony.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use iroh::EndpointId as NodeId;
use iroh::endpoint::{Connection, Endpoint, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh_blobs::Hash;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use uuid::Uuid;

use super::runtime::{CompiledTask, ExecMetrics, HostGrants, TaskError, WasmRuntime};
use super::{COMPUTE_ALPN, ResourceLimits, TaskClass};

/// Upper bound of a request frame (envelope + input bytes).
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound of a reply frame (envelope + output bytes).
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;
/// Compiled modules kept per executor (keyed by blob hash).
const COMPILED_CACHE_ENTRIES: usize = 32;

// ─── Wire messages ───────────────────────────────────────────────────────────

/// Requester → executor: the first (and only) request frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComputeRequest {
    /// Run a task (two response frames: [`ExecuteAck`], then [`ExecuteReply`]).
    Execute(ExecuteRequest),
    /// Contract-Net probe (Phase 5): "would you take a task of this class,
    /// and how ready are you right now?" One response frame: [`ProbeReply`].
    Probe(ProbeRequest),
}

/// Auction probe: asks for a *fresh* readiness sample, bypassing possibly
/// stale gossip vectors, before committing an expensive task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub class: TaskClass,
}

/// Executor → requester: fresh readiness (the "bid" of the Contract Net).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReply {
    /// Whether the admission policy would accept this class right now.
    pub accepts_class: bool,
    /// Free concurrency slots at sampling time.
    pub free_slots: u32,
    /// Fresh CPU load (0-100) and free memory, sampled for this reply.
    pub cpu_load_pct: u8,
    pub ram_free_mb: u32,
}

/// Requester → executor: run this task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteRequest {
    /// Correlation id chosen by the requester (traces, and the Phase 4 ledger).
    pub task_id: Uuid,
    /// BLAKE3 hash of the `.wasm` blob; the executor fetches and verifies the
    /// code by this hash, so it cannot run anything else.
    pub wasm_hash: Hash,
    /// Exported function to invoke (ABI: `(ptr: i32, len: i32) -> i64`).
    pub entrypoint: String,
    /// Admission class, matched against the executor's policy.
    pub class: TaskClass,
    /// Resource ceiling the executor enforces on the run.
    pub limits: ResourceLimits,
    /// Opaque input bytes (§8.2: meaning is app-defined; the SDK convention
    /// is CBOR, invisible at this layer).
    pub input: Vec<u8>,
    /// NN model the task needs (`load_by_name`, phase NN-3). The scheduler
    /// only routes to nodes advertising it, and the executor rejects at
    /// admission when it cannot serve it.
    pub required_model: Option<String>,
}

/// Executor → requester, first frame: fast admission verdict, sent before the
/// (potentially slow) blob fetch + compile + run so the requester can tell
/// "queued behind a real executor" apart from "unreachable".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecuteAck {
    Accepted,
    Rejected(RejectReason),
}

/// Why the executor's admission policy refused a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum RejectReason {
    #[error("executor does not accept tasks of this class")]
    ClassNotAccepted,
    #[error("executor has no free concurrency slot")]
    Busy,
    #[error("executor does not serve the required NN model: {0}")]
    ModelNotAvailable(String),
    #[error("malformed request: {0}")]
    Malformed(String),
}

/// Executor → requester, second frame (only after `Accepted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteReply {
    pub outcome: Result<CompletedTask, TaskError>,
}

/// Successful completion: output bytes plus what the run actually cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedTask {
    pub output: Vec<u8>,
    pub metrics: ExecMetrics,
}

// ─── Executor side ───────────────────────────────────────────────────────────

/// The executor's admission policy — the owner's local word is final
/// (RFC §8.3: the reciprocity term never overrides local policy).
#[derive(Debug, Clone)]
pub struct ExecutorPolicy {
    /// Task classes this node runs.
    pub accepts: Vec<TaskClass>,
    /// Concurrency ceiling; `0` disables execution entirely.
    pub max_concurrent: u32,
}

impl Default for ExecutorPolicy {
    /// Reciprocity-by-default (RFC §8.3): pure-wasm classes are accepted with
    /// modest concurrency. `Inference` stays off until its opt-in host
    /// functions exist (Phase 5).
    fn default() -> Self {
        Self {
            accepts: vec![TaskClass::General, TaskClass::Media, TaskClass::Analytics],
            max_concurrent: 2,
        }
    }
}

/// Where the executor gets `.wasm` bytes from. Production uses the iroh-blobs
/// store ([`BlobStore`](crate::p2p::network::core::blobs::BlobStore)); tests
/// inject an in-memory source.
#[async_trait]
pub trait WasmFetcher: Send + Sync + 'static {
    /// Returns the verified bytes of `hash`, fetching from `provider` (the
    /// requester) when not available locally.
    async fn fetch_wasm(&self, hash: &Hash, provider: NodeId) -> Result<Vec<u8>, String>;
}

#[async_trait]
impl WasmFetcher for crate::p2p::network::core::blobs::BlobStore {
    async fn fetch_wasm(&self, hash: &Hash, provider: NodeId) -> Result<Vec<u8>, String> {
        self.get_or_download(hash, &[provider])
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| e.to_string())
    }
}

/// Decides admission and, when accepted, reserves a concurrency slot that is
/// released when the returned guard drops.
fn admit(
    policy: &ExecutorPolicy,
    class: TaskClass,
    running: &Arc<AtomicU32>,
) -> Result<SlotGuard, RejectReason> {
    if !policy.accepts.contains(&class) {
        return Err(RejectReason::ClassNotAccepted);
    }
    // Optimistic reservation: increment, then back off if over the ceiling —
    // race-free without a lock around the check.
    let prev = running.fetch_add(1, Ordering::AcqRel);
    if prev >= policy.max_concurrent {
        running.fetch_sub(1, Ordering::AcqRel);
        return Err(RejectReason::Busy);
    }
    Ok(SlotGuard {
        running: running.clone(),
    })
}

/// RAII release of a reserved concurrency slot.
struct SlotGuard {
    running: Arc<AtomicU32>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.running.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Protocol handler (executor side) registered on the Router via [`COMPUTE_ALPN`].
#[derive(Clone)]
pub struct ComputeProtocolHandler {
    runtime: Arc<WasmRuntime>,
    fetcher: Arc<dyn WasmFetcher>,
    policy: Arc<parking_lot::RwLock<ExecutorPolicy>>,
    /// Host capabilities the owner grants to tasks (Phase 5); default: none.
    grants: Arc<parking_lot::RwLock<HostGrants>>,
    /// Blob-backed NN model catalog (RFC 0003 phase NN-2), attached to
    /// `Inference`-class tasks when set.
    #[cfg(feature = "compute-nn")]
    nn_models: Arc<parking_lot::RwLock<Option<Arc<crate::compute::nn::NnModelRegistry>>>>,
    running: Arc<AtomicU32>,
    compiled: Arc<parking_lot::Mutex<lru::LruCache<Hash, CompiledTask>>>,
    /// Fresh machine sampling for auction probes.
    probe_system: Arc<parking_lot::Mutex<sysinfo::System>>,
}

impl std::fmt::Debug for ComputeProtocolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputeProtocolHandler")
            .field("running", &self.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ComputeProtocolHandler {
    pub fn new(fetcher: Arc<dyn WasmFetcher>, policy: ExecutorPolicy) -> Result<Self, TaskError> {
        Ok(Self {
            runtime: Arc::new(WasmRuntime::new()?),
            fetcher,
            policy: Arc::new(parking_lot::RwLock::new(policy)),
            grants: Arc::new(parking_lot::RwLock::new(HostGrants::default())),
            #[cfg(feature = "compute-nn")]
            nn_models: Arc::new(parking_lot::RwLock::new(None)),
            running: Arc::new(AtomicU32::new(0)),
            compiled: Arc::new(parking_lot::Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(COMPILED_CACHE_ENTRIES).expect("nonzero"),
            ))),
            // Primed with one refresh so the first auction probe reports a real
            // CPU delta: sysinfo computes usage between two refreshes, so a
            // virgin System would report 0% on the first probe.
            probe_system: Arc::new(parking_lot::Mutex::new({
                let mut system = sysinfo::System::new();
                system.refresh_cpu_usage();
                system.refresh_memory();
                system
            })),
        })
    }

    /// Replaces the admission policy at runtime (owner control, RFC §8.3).
    pub fn set_policy(&self, policy: ExecutorPolicy) {
        *self.policy.write() = policy;
    }

    /// Sets the host capabilities granted to tasks (Phase 5, owner opt-in).
    /// Default is none: pure input→output sandbox, deterministic.
    pub fn set_host_grants(&self, grants: HostGrants) {
        *self.grants.write() = grants;
    }

    /// Sets the blob-backed NN model catalog (RFC 0003 phase NN-2): its
    /// models are attached (as the wasi-nn grant) to `Inference`-class tasks,
    /// fetched by hash and session-cached on first use. Unless a registry is
    /// set — or `HostGrants::nn` was granted directly — `Inference` tasks
    /// still run, but in the pure-wasm sandbox (RFC 0002 §6.1: small models
    /// can run entirely inside WASM).
    ///
    /// Offering models is the opt-in for inference, so this also adds
    /// [`TaskClass::Inference`] to the admission policy (it is off in
    /// [`ExecutorPolicy::default`]). Otherwise every inference task would be
    /// rejected as `ClassNotAccepted` before the catalog was ever consulted,
    /// and the node would advertise no inference capacity.
    #[cfg(feature = "compute-nn")]
    pub fn set_nn_models(&self, registry: Arc<crate::compute::nn::NnModelRegistry>) {
        *self.nn_models.write() = Some(registry);
        let mut policy = self.policy.write();
        if !policy.accepts.contains(&TaskClass::Inference) {
            policy.accepts.push(TaskClass::Inference);
        }
    }

    /// Answers an auction probe with a fresh local sample.
    ///
    /// The `sysinfo` refresh (a synchronous syscall) runs on the blocking pool
    /// so it never stalls the async worker serving the connection.
    async fn probe_reply(&self, class: TaskClass) -> ProbeReply {
        let policy = self.policy.read().clone();
        let running = self.running.load(Ordering::Relaxed);
        let system = self.probe_system.clone();
        let (cpu_load_pct, ram_free_mb) = tokio::task::spawn_blocking(move || {
            let mut system = system.lock();
            system.refresh_cpu_usage();
            system.refresh_memory();
            (
                system.global_cpu_usage().round().clamp(0.0, 100.0) as u8,
                (system.available_memory() / (1024 * 1024)).min(u32::MAX as u64) as u32,
            )
        })
        .await
        .unwrap_or((0, 0));
        ProbeReply {
            accepts_class: policy.accepts.contains(&class) && running < policy.max_concurrent,
            free_slots: policy.max_concurrent.saturating_sub(running),
            cpu_load_pct,
            ram_free_mb,
        }
    }

    pub fn policy(&self) -> ExecutorPolicy {
        self.policy.read().clone()
    }

    /// Tasks currently holding a concurrency slot.
    pub fn tasks_running(&self) -> u32 {
        self.running.load(Ordering::Relaxed)
    }

    /// NN model names this executor serves (empty without `compute-nn`);
    /// advertised in the capability vector (phase NN-3).
    pub fn nn_model_names(&self) -> Vec<String> {
        #[cfg(feature = "compute-nn")]
        {
            self.nn_models
                .read()
                .as_ref()
                .map(|registry| registry.model_names())
                .unwrap_or_default()
        }
        #[cfg(not(feature = "compute-nn"))]
        {
            Vec::new()
        }
    }

    /// Whether this executor can serve the named NN model — via the blob
    /// registry or a directly granted `HostGrants::nn`.
    fn serves_model(&self, name: &str) -> bool {
        #[cfg(feature = "compute-nn")]
        {
            let in_registry = self
                .nn_models
                .read()
                .as_ref()
                .is_some_and(|registry| registry.has_model(name));
            let in_grants = self
                .grants
                .read()
                .nn
                .as_ref()
                .is_some_and(|grant| grant.has_model(name));
            in_registry || in_grants
        }
        #[cfg(not(feature = "compute-nn"))]
        {
            let _ = name;
            false
        }
    }

    /// Returns the compiled module for `hash`, fetching + compiling on miss.
    async fn compiled_task(
        &self,
        hash: &Hash,
        provider: NodeId,
    ) -> Result<CompiledTask, TaskError> {
        if let Some(task) = self.compiled.lock().get(hash).cloned() {
            debug!(hash = %hash.fmt_short(), "compute: compiled-module cache hit");
            return Ok(task);
        }
        let wasm = self
            .fetcher
            .fetch_wasm(hash, provider)
            .await
            .map_err(TaskError::WasmUnavailable)?;
        let task = self.runtime.compile(&wasm)?;
        self.compiled.lock().put(*hash, task.clone());
        Ok(task)
    }

    /// Serves one request end-to-end and writes both response frames.
    async fn serve(
        &self,
        requester: NodeId,
        send: &mut SendStream,
        recv: &mut RecvStream,
    ) -> Result<(), AcceptError> {
        let raw = read_frame(recv, MAX_REQUEST_BYTES)
            .await
            .map_err(AcceptError::from_err)?;

        let request: ComputeRequest = match postcard::from_bytes(&raw) {
            Ok(req) => req,
            Err(e) => {
                write_frame(
                    send,
                    &encode(&ExecuteAck::Rejected(RejectReason::Malformed(
                        e.to_string(),
                    )))?,
                )
                .await
                .map_err(AcceptError::from_err)?;
                return Ok(());
            }
        };

        let request = match request {
            ComputeRequest::Execute(request) => request,
            ComputeRequest::Probe(probe) => {
                let reply = self.probe_reply(probe.class).await;
                write_frame(send, &encode(&reply)?)
                    .await
                    .map_err(AcceptError::from_err)?;
                return Ok(());
            }
        };

        // NN-3: a task naming a required model is only admitted when this
        // executor can actually serve it — a clean rejection here beats a
        // trap at `load_by_name` later.
        if let Some(model) = &request.required_model
            && !self.serves_model(model)
        {
            write_frame(
                send,
                &encode(&ExecuteAck::Rejected(RejectReason::ModelNotAvailable(
                    model.clone(),
                )))?,
            )
            .await
            .map_err(AcceptError::from_err)?;
            return Ok(());
        }

        // Admission: fast verdict before any expensive work.
        let slot = {
            let policy = self.policy.read().clone();
            admit(&policy, request.class, &self.running)
        };
        let slot = match slot {
            Ok(slot) => slot,
            Err(reason) => {
                debug!(task = %request.task_id, peer = %requester.fmt_short(),
                       %reason, "compute: task rejected");
                write_frame(send, &encode(&ExecuteAck::Rejected(reason))?)
                    .await
                    .map_err(AcceptError::from_err)?;
                return Ok(());
            }
        };
        write_frame(send, &encode(&ExecuteAck::Accepted)?)
            .await
            .map_err(AcceptError::from_err)?;

        // Fetch + compile (cached), then run in the sandbox off the async pool.
        let outcome = match self.compiled_task(&request.wasm_hash, requester).await {
            Ok(task) => {
                let runtime = self.runtime.clone();
                let entrypoint = request.entrypoint.clone();
                let limits = request.limits;
                let input = request.input;
                #[allow(unused_mut)]
                let mut grants = self.grants.read().clone();

                // NN-2: attach the blob-backed model catalog to tasks that
                // need NN (Inference class, or any task naming a required
                // model), unless the owner already granted models directly.
                // Model blobs are fetched with the requester as provider and
                // sessions are cached in the registry across runs. Covering
                // `required_model` here (not just the Inference class) stops a
                // non-Inference task naming a model — already admitted by
                // `serves_model` — from failing at instantiation with the
                // wasi-nn import unlinked.
                #[cfg(feature = "compute-nn")]
                let nn_registry = self.nn_models.read().clone();
                #[cfg(feature = "compute-nn")]
                if (request.class == TaskClass::Inference || request.required_model.is_some())
                    && grants.nn.is_none()
                    && let Some(registry) = nn_registry
                    && !registry.is_empty()
                {
                    match registry.grant_for(&self.fetcher, requester).await {
                        Ok(grant) => grants.nn = Some(grant),
                        Err(e) => {
                            warn!(task = %request.task_id, error = %e,
                                  "compute: NN model preparation failed");
                            write_frame(send, &encode(&ExecuteReply { outcome: Err(e) })?)
                                .await
                                .map_err(AcceptError::from_err)?;
                            return Ok(());
                        }
                    }
                }

                // Response-level deadline: epoch interruption aborts wasm
                // code at `timeout_ms`, but a guest stuck inside a *native*
                // host call (e.g. NN inference) cannot be interrupted — this
                // outer timeout guarantees the requester still gets a reply,
                // abandoning the blocking thread until the native call ends.
                let response_deadline = Duration::from_millis(
                    limits.timeout_ms.saturating_mul(2).saturating_add(1_000),
                );
                let run = tokio::task::spawn_blocking(move || {
                    runtime.execute_with_host(&task, &entrypoint, &input, &limits, &grants)
                });
                match tokio::time::timeout(response_deadline, run).await {
                    Err(_elapsed) => Err(TaskError::DeadlineExceeded),
                    Ok(join) => join
                        .map_err(|e| TaskError::Runtime(format!("executor task panicked: {e}")))
                        .and_then(|r| r)
                        .map(|exec| CompletedTask {
                            output: exec.output,
                            metrics: exec.metrics,
                        }),
                }
            }
            Err(e) => Err(e),
        };
        drop(slot);

        match &outcome {
            Ok(done) => debug!(task = %request.task_id, peer = %requester.fmt_short(),
                fuel = done.metrics.fuel_consumed, ms = done.metrics.duration_ms,
                "compute: task completed"),
            Err(e) => warn!(task = %request.task_id, peer = %requester.fmt_short(),
                error = %e, "compute: task failed"),
        }

        write_frame(send, &encode(&ExecuteReply { outcome })?)
            .await
            .map_err(AcceptError::from_err)?;
        Ok(())
    }
}

impl ProtocolHandler for ComputeProtocolHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // The requester's identity is authenticated by the QUIC TLS handshake;
        // it doubles as the blob provider for the task's wasm.
        let requester = connection.remote_id();

        let (mut send, mut recv) = connection.accept_bi().await?;
        self.serve(requester, &mut send, &mut recv).await?;

        send.finish().map_err(AcceptError::from_err)?;
        // Ensure delivery before tearing the connection down.
        connection.closed().await;
        Ok(())
    }
}

// ─── Requester side ──────────────────────────────────────────────────────────

/// Why a delegated execution failed, from the requester's point of view.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComputeCallError {
    #[error("executor unreachable: {0}")]
    Unreachable(String),
    #[error("executor rejected the task: {0}")]
    Rejected(RejectReason),
    #[error("task failed on the executor: {0}")]
    Task(TaskError),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out waiting for the executor")]
    Timeout,
}

/// Requester-side client for the compute protocol (Phase 2: the caller picks
/// the executor; Phase 3 adds the capability-aware scheduler on top).
#[derive(Debug, Clone)]
pub struct ComputeClient {
    endpoint: Endpoint,
}

impl ComputeClient {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    /// Runs `request` on `executor`, waiting at most `total_timeout` for the
    /// whole round trip (connect, admission, blob fetch, compile, run, reply).
    ///
    /// The task's wasm blob must be resolvable by the executor — in the normal
    /// flow it was published to this node's blob store, and the executor
    /// fetches it from us by hash while we wait.
    pub async fn execute_on(
        &self,
        executor: impl Into<iroh::EndpointAddr>,
        request: ExecuteRequest,
        total_timeout: Duration,
    ) -> Result<CompletedTask, ComputeCallError> {
        tokio::time::timeout(total_timeout, self.call(executor.into(), request))
            .await
            .map_err(|_| ComputeCallError::Timeout)?
    }

    /// Contract-Net probe (Phase 5): asks `executor` for a fresh readiness
    /// sample for `class`, without committing any work.
    pub async fn probe(
        &self,
        executor: impl Into<iroh::EndpointAddr>,
        class: TaskClass,
        timeout: Duration,
    ) -> Result<ProbeReply, ComputeCallError> {
        let executor = executor.into();
        tokio::time::timeout(timeout, async move {
            let connection = self
                .endpoint
                .connect(executor, COMPUTE_ALPN)
                .await
                .map_err(|e| ComputeCallError::Unreachable(e.to_string()))?;
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|e| ComputeCallError::Unreachable(e.to_string()))?;

            let raw = encode(&ComputeRequest::Probe(ProbeRequest { class }))
                .map_err(|e| ComputeCallError::Protocol(format!("encode: {e}")))?;
            write_frame(&mut send, &raw)
                .await
                .map_err(|e| ComputeCallError::Protocol(format!("send probe: {e}")))?;
            send.finish()
                .map_err(|e| ComputeCallError::Protocol(format!("finish stream: {e}")))?;

            let reply_raw = read_frame(&mut recv, 4096)
                .await
                .map_err(|e| ComputeCallError::Protocol(format!("read probe reply: {e}")))?;
            let reply: ProbeReply = postcard::from_bytes(&reply_raw)
                .map_err(|e| ComputeCallError::Protocol(format!("decode probe reply: {e}")))?;
            connection.close(0u32.into(), b"done");
            Ok(reply)
        })
        .await
        .map_err(|_| ComputeCallError::Timeout)?
    }

    async fn call(
        &self,
        executor: iroh::EndpointAddr,
        request: ExecuteRequest,
    ) -> Result<CompletedTask, ComputeCallError> {
        let connection = self
            .endpoint
            .connect(executor, COMPUTE_ALPN)
            .await
            .map_err(|e| ComputeCallError::Unreachable(e.to_string()))?;

        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|e| ComputeCallError::Unreachable(e.to_string()))?;

        let raw = encode(&ComputeRequest::Execute(request))
            .map_err(|e| ComputeCallError::Protocol(format!("encode: {e}")))?;
        write_frame(&mut send, &raw)
            .await
            .map_err(|e| ComputeCallError::Protocol(format!("send request: {e}")))?;
        send.finish()
            .map_err(|e| ComputeCallError::Protocol(format!("finish stream: {e}")))?;

        // Frame 1: admission verdict.
        let ack_raw = read_frame(&mut recv, 4096)
            .await
            .map_err(|e| ComputeCallError::Protocol(format!("read ack: {e}")))?;
        let ack: ExecuteAck = postcard::from_bytes(&ack_raw)
            .map_err(|e| ComputeCallError::Protocol(format!("decode ack: {e}")))?;
        if let ExecuteAck::Rejected(reason) = ack {
            connection.close(0u32.into(), b"rejected");
            return Err(ComputeCallError::Rejected(reason));
        }

        // Frame 2: outcome.
        let reply_raw = read_frame(&mut recv, MAX_REPLY_BYTES)
            .await
            .map_err(|e| ComputeCallError::Protocol(format!("read reply: {e}")))?;
        let reply: ExecuteReply = postcard::from_bytes(&reply_raw)
            .map_err(|e| ComputeCallError::Protocol(format!("decode reply: {e}")))?;

        connection.close(0u32.into(), b"done");
        reply.outcome.map_err(ComputeCallError::Task)
    }
}

// ─── Framing ─────────────────────────────────────────────────────────────────

fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, AcceptError> {
    postcard::to_stdvec(msg).map_err(AcceptError::from_err)
}

/// Writes one `u32`-LE-length-prefixed frame.
async fn write_frame(send: &mut SendStream, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    send.write_all(&len.to_le_bytes())
        .await
        .map_err(std::io::Error::other)?;
    send.write_all(payload)
        .await
        .map_err(std::io::Error::other)?;
    Ok(())
}

/// Reads one `u32`-LE-length-prefixed frame, refusing frames larger than `max`.
async fn read_frame(recv: &mut RecvStream, max: usize) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(std::io::Error::other)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {max}-byte limit"),
        ));
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload)
        .await
        .map_err(std::io::Error::other)?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> ExecuteRequest {
        ExecuteRequest {
            task_id: Uuid::new_v4(),
            wasm_hash: Hash::new(b"some wasm"),
            entrypoint: "gdb_run".into(),
            class: TaskClass::General,
            limits: ResourceLimits::default(),
            input: b"payload".to_vec(),
            required_model: None,
        }
    }

    #[test]
    fn request_roundtrips_through_postcard() {
        let req = ComputeRequest::Execute(sample_request());
        let bytes = postcard::to_stdvec(&req).expect("serialize");
        let back: ComputeRequest = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(req, back);

        let probe = ComputeRequest::Probe(ProbeRequest {
            class: TaskClass::Analytics,
        });
        let bytes = postcard::to_stdvec(&probe).expect("serialize");
        let back: ComputeRequest = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(probe, back);
    }

    #[tokio::test]
    async fn probe_reply_reflects_policy_and_slots() {
        struct NoFetch;
        #[async_trait]
        impl WasmFetcher for NoFetch {
            async fn fetch_wasm(&self, _h: &Hash, _p: NodeId) -> Result<Vec<u8>, String> {
                Err("unused".into())
            }
        }
        let handler = ComputeProtocolHandler::new(Arc::new(NoFetch), ExecutorPolicy::default())
            .expect("handler");

        let reply = handler.probe_reply(TaskClass::General).await;
        assert!(reply.accepts_class);
        assert_eq!(reply.free_slots, 2);
        assert!(reply.cpu_load_pct <= 100);

        // Class outside the policy is refused even with free slots.
        let reply = handler.probe_reply(TaskClass::Inference).await;
        assert!(!reply.accepts_class);

        // Zero slots refuses everything.
        handler.set_policy(ExecutorPolicy {
            max_concurrent: 0,
            ..ExecutorPolicy::default()
        });
        let reply = handler.probe_reply(TaskClass::General).await;
        assert!(!reply.accepts_class);
        assert_eq!(reply.free_slots, 0);
    }

    #[test]
    fn reply_roundtrips_including_task_error() {
        let reply = ExecuteReply {
            outcome: Err(TaskError::FuelExhausted),
        };
        let bytes = postcard::to_stdvec(&reply).expect("serialize");
        let back: ExecuteReply = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(reply, back);
    }

    #[cfg(feature = "compute-nn")]
    #[tokio::test]
    async fn set_nn_models_enables_inference_admission() {
        struct NoFetch;
        #[async_trait]
        impl WasmFetcher for NoFetch {
            async fn fetch_wasm(&self, _h: &Hash, _p: NodeId) -> Result<Vec<u8>, String> {
                Err("unused".into())
            }
        }
        let handler = ComputeProtocolHandler::new(Arc::new(NoFetch), ExecutorPolicy::default())
            .expect("handler");
        // Default policy keeps Inference off.
        assert!(!handler.policy().accepts.contains(&TaskClass::Inference));

        // Offering models is the opt-in and must flip admission on, or every
        // inference task would be rejected before the catalog is consulted.
        handler.set_nn_models(Arc::new(crate::compute::nn::NnModelRegistry::new()));
        assert!(handler.policy().accepts.contains(&TaskClass::Inference));
        assert!(
            handler
                .probe_reply(TaskClass::Inference)
                .await
                .accepts_class
        );
    }

    #[test]
    fn admission_rejects_class_not_in_policy() {
        let running = Arc::new(AtomicU32::new(0));
        let policy = ExecutorPolicy::default();
        assert!(matches!(
            admit(&policy, TaskClass::Inference, &running),
            Err(RejectReason::ClassNotAccepted)
        ));
        // A rejection must not leak a slot.
        assert_eq!(running.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn admission_enforces_concurrency_and_releases_slots() {
        let running = Arc::new(AtomicU32::new(0));
        let policy = ExecutorPolicy {
            accepts: vec![TaskClass::General],
            max_concurrent: 2,
        };

        let a = admit(&policy, TaskClass::General, &running).expect("slot 1");
        let _b = admit(&policy, TaskClass::General, &running).expect("slot 2");
        assert!(matches!(
            admit(&policy, TaskClass::General, &running),
            Err(RejectReason::Busy)
        ));
        // The failed attempt must not leak its optimistic increment.
        assert_eq!(running.load(Ordering::Relaxed), 2);

        drop(a);
        assert_eq!(running.load(Ordering::Relaxed), 1);
        let _c = admit(&policy, TaskClass::General, &running).expect("slot freed");
    }

    #[test]
    fn zero_concurrency_disables_execution() {
        let running = Arc::new(AtomicU32::new(0));
        let policy = ExecutorPolicy {
            accepts: vec![TaskClass::General],
            max_concurrent: 0,
        };
        assert!(matches!(
            admit(&policy, TaskClass::General, &running),
            Err(RejectReason::Busy)
        ));
        assert_eq!(running.load(Ordering::Relaxed), 0);
    }
}
