# Guardian Compute

GuardianDB ships **Guardian Compute** — a decentralized edge-computing layer on
top of its local-first, P2P data model. Nodes delegate the execution of
business logic (compiled to WebAssembly) to *other* nodes, and a
capability-aware scheduler routes each task to the peer with the most spare
capacity. Results flow back through ordinary GuardianDB replication.

It turns GuardianDB from "a database that replicates data" into "a database
that also runs work where there is capacity to run it" — without a central
scheduler, over the same Iroh P2P fabric the database already uses.

```
        ┌──────────── Requester ────────────┐        ┌──────────── Executor ────────────┐
        │                                    │        │                                   │
        │ 1. publish task.wasm as a blob     │        │ 0. gossip capability vector       │
        │    (BLAKE3 hash = code identity)   │◄───────│    (cores, RAM, load, models…)    │
        │ 2. scheduler ranks peers by the    │        │                                   │
        │    capability vectors it heard     │───────►│ 4. fetch task.wasm by hash        │
        │ 3. ExecuteRequest over the         │  QUIC  │ 5. run in wasmtime sandbox        │
        │    /guardian-db/compute/1 ALPN     │        │    (fuel, memory cap, timeout)    │
        │ 7. receive output + metrics        │◄───────│ 6. reply with result + metrics    │
        │ 8. store result → replicates       │        │                                   │
        └────────────────────────────────────┘        └───────────────────────────────────┘
```

The whole layer lives in the `guardian-db` crate as the feature-gated `compute`
module (`src/compute/`), plus two small companion crates for authoring tasks
(`guardian-compute-sdk`, `guardian-compute-sdk-macros`).

---

## 1. Why this is a natural fit

Guardian Compute reuses the infrastructure the database already has, rather than
building a parallel stack:

| Need | Reused from GuardianDB / Iroh |
|---|---|
| Secure P2P transport + node identity | `iroh` QUIC + public-key identity |
| Custom protocols | the Iroh `Router`'s ALPN multiplexing (alongside gossip/blobs/docs) |
| Distributing WASM binaries and NN models | `iroh-blobs` — content-addressed by BLAKE3 hash, integrity verified on fetch |
| Broadcasting capability telemetry | `iroh-gossip` |
| Reactive triggers ("when data lands, run X") | the store `EventBus` (the same pattern as `reactive_synchronizer`) |
| Auditable, replicated task ledger | a GuardianDB store |
| Who may participate | the store `AccessController` (permissioned networks) |

What Guardian Compute adds on top: a **WASM sandbox**, **capability telemetry**,
a **scheduler**, a **task ledger**, and **reactive triggers**.

---

## 2. Feature flags

Everything is off by default and adds nothing to a plain build.

| Feature | Enables |
|---|---|
| `compute` | The full runtime, protocol, telemetry, scheduler, ledger, and triggers. Pulls in `wasmtime` (runtime + Cranelift JIT, **no** WASI) and `sysinfo`. |
| `compute-nn` | Implies `compute`. Links the `wasi-nn` API into the sandbox for Edge AI, backed by ONNX Runtime. Heavier build (downloads the ONNX Runtime binaries). |
| `compute-nn-cuda` | Implies `compute-nn`. Loads models with the CUDA execution provider and verifies advertised GPU against real CUDA availability. |

```toml
[dependencies]
guardian-db = { version = "0.18", features = ["compute"] }
# or, for Edge AI:
guardian-db = { version = "0.18", features = ["compute-nn"] }
```

When the `compute` feature is enabled, the executor is registered automatically
during node initialization; you interact with it through accessors on the
Iroh backend (see §9).

---

## 3. Core concepts

- **Task** — a WebAssembly module plus an entrypoint name and opaque input
  bytes. The module is distributed as an iroh blob; the request names it by
  BLAKE3 hash, so it is impossible for an executor to run anything other than
  the code the requester named.
- **Sandbox** — every task runs in a fresh wasmtime `Store` with three hard
  limits: a linear-memory ceiling, a CPU budget (wasmtime *fuel*), and a
  wall-clock deadline (epoch interruption). No WASI is linked by default: a
  task sees nothing but its input and output bytes — no filesystem, no
  network, no clock, no randomness.
- **Task class** — a coarse label (`General`, `Media`, `Analytics`,
  `Inference`) matched against the executor's admission policy.
- **Capability vector** — what a node advertises over gossip: cores,
  architecture, RAM, current load, battery state, free slots, accepted
  classes, and served NN models. Vectors are *hints, not contracts* — the
  executor's local policy always has the final word.
- **Host grants** — opt-in capabilities the executor's owner may extend to
  tasks (logging, reading the local store, NN inference). Off by default,
  which is what keeps runs deterministic.

---

## 4. Execution models

All of these are reached through a `ComputeScheduler` (or, for the direct case,
a `ComputeClient`). The requester's node authenticates over QUIC and doubles as
the blob provider from which the executor fetches the task code.

### 4.1 Direct delegation — "run this on that node"

```rust
let client = backend.compute_client().await?;
let done = client.execute_on(executor_node_id, request, Duration::from_secs(60)).await?;
```

The lowest-level path: you name the executor. The client sends one
`ExecuteRequest`, receives an admission `ExecuteAck` (fast verdict), and — if
accepted — an `ExecuteReply` carrying the output and `ExecMetrics` (fuel spent,
duration, peak memory).

### 4.2 Capability-aware scheduling — the network decides

```rust
let scheduler = backend.compute_scheduler().await?;
let delegated = scheduler.execute(request).await?;   // picks the best node
```

The scheduler ranks the capability vectors it has heard and delegates to the
best node, **failing over** down the ranking when a candidate rejects the task,
is unreachable, or times out. Deterministic task failures (fuel/deadline/memory
limits, a guest trap, a bad module) are final; node-specific failures (blob not
available here, a capability not granted here, an executor hiccup) fail over to
the next candidate.

Scoring favors idle cores first, then free memory, with a battery penalty that
effectively disqualifies laptops on battery, and a reputation discount for nodes
that have produced divergent results (§4.5). A task may pin a **required NN
model**; the scheduler then only considers nodes advertising it (data gravity:
send the task to where the model already is).

### 4.3 Contract-Net auction — trust a fresh bid over stale gossip

```rust
let delegated = scheduler.execute_with_auction(request).await?;
```

For expensive tasks, the scheduler probes the top-ranked candidates for a
*fresh* readiness bid before committing, so a stale gossip vector can't send the
task to a node that is actually busy. Degrades gracefully to the gossip ranking
if nobody answers.

### 4.4 MapReduce fan-out

```rust
let results = scheduler.map(partitions).await;   // one Result per partition, in order
```

Fans partitions out across candidates in parallel, rotating the ranking per task
so the load spreads instead of piling on the top node. The *reduce* step is the
caller's.

### 4.5 Redundant k-of-n execution (untrusted networks)

```rust
let outcome = scheduler.execute_redundant(request, 3).await?;   // majority of 3
```

Runs the same **deterministic** task on up to `k` nodes and returns the majority
result, penalizing the reputation of any node whose output diverges from the
majority. Reputation persists across scheduling rounds, so a known liar is
de-prioritized (never hard-banned). Because it depends on determinism, this mode
and host grants do not mix.

---

## 5. Writing tasks

### 5.1 The SDK (recommended)

The `guardian-compute-sdk` crate hides the raw ABI behind one attribute:

```rust
use guardian_compute_sdk::guardian_task;

#[guardian_task]
fn shout(input: &[u8]) -> Vec<u8> {
    input.to_ascii_uppercase()
}
```

Returning `Result<Vec<u8>, E>` turns an `Err` into a clean trap on the executor:

```rust
use guardian_compute_sdk::{guardian_task, TaskFailure};

#[guardian_task]
fn thumbnail(input: &[u8]) -> Result<Vec<u8>, TaskFailure> {
    let img = image::load_from_memory(input).map_err(|e| TaskFailure::new(e.to_string()))?;
    Ok(img.thumbnail(128, 128).into_bytes())
}
```

Typed I/O over CBOR (enable the SDK's `cbor` feature) — the SDK-level
convention; the wire protocol itself stays opaque bytes:

```rust
#[guardian_task(cbor)]
fn word_count(doc: Document) -> Result<Stats, TaskFailure> { /* In → Out, both serde types */ }
```

Build and publish:

```bash
cargo build -p my-tasks --target wasm32-unknown-unknown --release
# then add the resulting .wasm to the blob store; its BLAKE3 hash is the task id
```

The exported function's name is the `entrypoint` you put in the request.

### 5.2 The ABI (for hand-written modules)

A task module exports:

- `memory` — its linear memory;
- `gdb_alloc: (len: i32) -> i32` — returns an offset where the host writes the
  input bytes;
- the entrypoint `(ptr: i32, len: i32) -> i64` — receives the input location and
  returns the output location packed as `(out_ptr << 32) | out_len`.

Input and output are opaque byte strings; their meaning is a contract between
the module author and the requester.

### 5.3 Host functions (opt-in)

When the executor's owner grants them (`HostGrants`), a task may import, under
the `"gdb"` module:

- `log(ptr, len)` — emit a UTF-8 message into the executor's tracing;
- `store_get(key_ptr, key_len, dest_ptr, dest_cap) -> i32` — read a value from
  the executor's local store.

A module that imports a capability the executor did not grant is refused at
instantiation with `HostCapabilityDenied` — before any code runs. Modules that
import nothing run on any executor. The SDK exposes these behind its `host`
feature as `guardian::log` / `guardian::store_get`.

---

## 6. Edge AI (`compute-nn`)

With the `compute-nn` feature, an executor can serve ONNX models to inference
tasks via the standard `wasi-nn` API. Models are **owner-curated named models**:

```rust
let registry = Arc::new(NnModelRegistry::new());
registry.register_model("whisper-tiny", model_blob_hash);   // name → blob hash
handler.set_nn_models(registry);                            // also enables the Inference class
```

- Models are ordinary iroh blobs — big files addressed by hash, exactly what
  iroh-blobs is for. On the first inference task the executor downloads the
  model from the requester (or any peer that has it) and caches the loaded ONNX
  session; later tasks reuse it.
- The guest reaches models only by `load_by_name`; it cannot load arbitrary
  model bytes. The executor's owner decides what runs.
- The scheduler routes inference tasks by **model affinity**: a task naming a
  `required_model` only goes to nodes advertising it, and an executor asked for
  a model it does not serve rejects cleanly at admission.
- **GPU** (`compute-nn-cuda`): `NnTarget::Gpu` loads models on the CUDA
  execution provider, falling back to CPU safely when CUDA is absent. Advertised
  `Accel::Gpu` is verified against real CUDA availability, not merely declared.

An `Inference`-class task that imports no `wasi-nn` still runs — small models
can run entirely inside pure WASM.

---

## 7. Reactive triggers and the task ledger

Trigger rules run tasks automatically when data lands, using the store event
bus (the same pattern as `reactive_synchronizer`):

```rust
let engine = Arc::new(TriggerEngine::new(TaskLedger::in_memory(), scheduler, TriggerConfig::default()));
engine.on_replicated("/photos", TaskSpec {           // "when a photo replicates…"
    wasm_hash: THUMBNAILER, entrypoint: "thumbnail".into(),
    class: TaskClass::Media, limits: ResourceLimits::default(),
    placement: Placement::BestAvailable, required_model: None,
});
engine.attach_event_bus(store_event_bus);            // bridge EventReplicated → triggers
engine.spawn_requeue_loop(Duration::from_secs(30));  // retry abandoned/failed tasks
```

- **Deduplication**: a replication event fires on *every* replica, so each task's
  ledger key is `blake3(rule id ␟ event id)` — deterministic on every replica.
  Only the node whose atomic claim wins actually dispatches, so the task runs
  once, not once per replica.
- **Lifecycle**: every task is recorded in a `TaskLedger` —
  `Pending → Running → Done | Failed` — persisted through the `LedgerStore`
  abstraction (`MemoryLedger` for single-node/tests, or your own impl over a
  replicated GuardianDB store for network-wide auditability).
- **Requeue**: a `Running` task whose deadline passes (its dispatcher vanished)
  or a transiently-failed one is re-dispatched, up to a retry budget. The claim
  that transitions `Pending → Running` is atomic (compare-and-swap), so a
  requeue pass never double-dispatches a task another dispatcher already owns.

---

## 8. Security and trust model

- **The sandbox protects the executor.** A task cannot touch the executor's
  filesystem, network, environment, or wall clock, and cannot exceed its
  memory/CPU/time budget. Guest-controlled lengths (output size, host-function
  buffers) are bounds-checked against the guest's own memory *before* the host
  allocates, so a hostile module cannot OOM the executor.
- **Trust in the *result* is the requester's problem.** The sandbox does not
  stop an executor from returning a wrong answer. Two answers to this:
  - **Permissioned networks** (the recommended default): run among nodes you
    trust by identity — your own devices, or an organization's — where the
    `AccessController` governs participation. This is the target for most of the
    layer.
  - **Redundant k-of-n execution** (§4.5) for open networks: compare independent
    results and penalize divergence.
- **Participation is reciprocal, never paid.** By running Guardian Compute you
  agree — as a term of use — that your node may execute tasks for other nodes of
  the same network, in the same measure you may use theirs (the BitTorrent
  "who downloads, seeds" spirit). There is no credit/token economy. Local policy
  stays sovereign: concurrency, accepted classes, host grants, and the
  battery rule (a node on battery advertises no capacity by default) are always
  the owner's to set.
- **Determinism is required only where it matters.** In the 1-requester → 1-
  executor model the result is just another replicated datum; non-determinism is
  fine. Determinism is required only for redundant k-of-n execution, which is
  exactly why the default sandbox withholds the clock and randomness.

---

## 9. Public API map

Everything is under `guardian_db::compute` (the module) and the Iroh backend
accessors.

**On the backend** (available when the `compute` feature is on):

| Accessor | Purpose |
|---|---|
| `compute_handler()` | The executor handler — adjust the admission policy, host grants, NN models |
| `compute_client()` | Requester-side client for direct `execute_on` |
| `compute_scheduler()` | Capability-aware scheduler (`execute`, auction, map, redundant) |
| `compute_capability_gossip()` | The telemetry service feeding the scheduler directory |
| `compute_join_capability_mesh(peers)` | Add peers to the capability gossip mesh |

**Key types** (`guardian_db::compute`):

- Runtime: `WasmRuntime`, `CompiledTask`, `HostGrants`, `HostStoreReader`,
  `ExecMetrics`, `TaskError` (with `is_transient()`); `NnGrant`, `NnTarget`
  (with `compute-nn`).
- Protocol: `ComputeClient`, `ComputeProtocolHandler`, `ExecuteRequest`,
  `ExecutorPolicy`, `RejectReason`, `CompletedTask`, `ComputeCallError`,
  `WasmFetcher`.
- Scheduler: `ComputeScheduler`, `SchedulerConfig`, `ScoreWeights`,
  `CapabilityDirectory`, `ReputationBook`, `Delegated`, `RedundantOutcome`,
  `ScheduleError`.
- Telemetry: `CapabilityGossip`, `TelemetryConfig`, `TelemetrySampler`,
  `CAPABILITY_TOPIC`.
- Ledger & triggers: `TaskLedger`, `LedgerStore`, `MemoryLedger`, `TaskRecord`,
  `TaskState`, `TriggerEngine`, `TriggerRule`, `TriggerConfig`,
  `TaskDispatcher`.
- Vocabulary: `TaskSpec`, `CapabilityVector`, `TaskClass`, `Placement`,
  `CpuArch`, `Accel`, `ResourceLimits`, `NnModelRegistry` (with `compute-nn`).

**Wire identifiers** (versioned; a breaking change bumps the version, never a
silent change):

- Execution ALPN: `/guardian-db/compute/1`.
- Capability gossip topic: `guardian-db/compute/capabilities/2`.

---

## 10. Use cases

- **Edge AI / LLM inference** — a light device offloads inference to the beefy
  node on the same network; the result replicates back.
- **Decentralized media processing** — a new upload triggers thumbnailing,
  image conversion/compression, or metadata extraction on an idle node, so the
  uploader (often a phone) doesn't stall. Image work runs today in pure WASM via
  Rust crates like `image`/`zune`; **heavy video transcoding (ffmpeg-class)
  needs native codecs**, so it requires opt-in *media host functions* — a
  planned capability, not yet implemented. Until then, video transcoding is out
  of reach of the pure sandbox.
- **MapReduce analytics** — slice a heavy query across nodes; each computes its
  partition, the requester reduces.
- **Automation / ETL with failover** — periodic jobs registered in the ledger;
  if the node that usually runs them vanishes, another takes over.

---

## 11. Status

Implemented and tested (unit + end-to-end over real in-process Iroh endpoints):

- **RFC 0002** — Phases 0–5: sandbox, direct delegation, capability-aware
  orchestration (telemetry + scoring + failover + auction), reactive triggers +
  ledger, MapReduce, and k-of-n redundancy with reputation.
- **RFC 0003 Part A** (Edge AI) — NN-1…NN-4: `wasi-nn` grant, blob-backed model
  registry with session caching, model-affinity routing, and GPU with verified
  detection.
- **RFC 0003 Part B** (SDK) — SDK-1…SDK-3: `#[guardian_task]`, typed CBOR I/O,
  and host-function bindings.

Remaining follow-ups (operational, hardware-gated, or scoped for later):
publishing the SDK crates to crates.io, terminal-record retention/pruning in the
ledger, confirming GPU acceleration on real CUDA hardware, and **media host
functions** for native-codec work such as video transcoding (image processing
already runs in pure WASM; ffmpeg-class transcoding does not).
