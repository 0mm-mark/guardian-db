//! # Blob-backed NN model registry (feature `compute-nn`, RFC 0003 phase NN-2)
//!
//! The executor's owner registers `name → blob hash` pairs; the models
//! themselves are ordinary iroh blobs — big files addressed by BLAKE3 hash,
//! exactly what iroh-blobs is for. When an `Inference`-class task arrives,
//! the registry resolves every registered model:
//!
//! - **bytes**: from the local blob store, falling back to a P2P download
//!   from the requester (the [`WasmFetcher`] the compute handler already uses
//!   for task code — the `FsStore` doubles as the on-disk model cache);
//! - **sessions**: loaded ONNX Runtime sessions are cached here by
//!   `(name, hash)`, so only the first task after a (re-)registration pays
//!   the load; re-registering a name with a new hash invalidates its entry.
//!
//! Wired through
//! [`ComputeProtocolHandler::set_nn_models`](super::protocol::ComputeProtocolHandler::set_nn_models).

use std::collections::HashMap;
use std::sync::Arc;

use iroh::EndpointId as NodeId;
use iroh_blobs::Hash;

use super::protocol::WasmFetcher;
use super::runtime::{NnGrant, NnTarget, TaskError, load_onnx_graph};

/// Owner-curated catalog of the NN models this executor serves, keyed by the
/// name guests use in `load_by_name`.
#[derive(Default)]
pub struct NnModelRegistry {
    /// What the owner registered: name → model blob hash.
    models: parking_lot::RwLock<HashMap<String, Hash>>,
    /// Where sessions execute (phase NN-4); CPU by default.
    target: parking_lot::RwLock<NnTarget>,
    /// Loaded-session cache: name → (hash it was loaded from, graph).
    graphs: parking_lot::Mutex<HashMap<String, (Hash, wasmtime_wasi_nn::Graph)>>,
}

impl std::fmt::Debug for NnModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NnModelRegistry")
            .field("models", &self.model_names())
            .field("cached", &self.cached_model_names())
            .finish()
    }
}

impl NnModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers a model under `name`: guests reach it via `load_by_name(name)`.
    /// Registering an existing name with a different hash replaces the model
    /// (its cached session is invalidated on the next resolution).
    pub fn register_model(&self, name: impl Into<String>, model_blob: Hash) {
        self.models.write().insert(name.into(), model_blob);
    }

    /// Stops offering `name` and drops its cached session.
    pub fn unregister_model(&self, name: &str) {
        self.models.write().remove(name);
        self.graphs.lock().remove(name);
    }

    /// Names currently offered.
    pub fn model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.models.read().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn is_empty(&self) -> bool {
        self.models.read().is_empty()
    }

    /// Whether `name` is currently offered.
    pub fn has_model(&self, name: &str) -> bool {
        self.models.read().contains_key(name)
    }

    /// Selects where sessions execute (phase NN-4). Requesting
    /// [`NnTarget::Gpu`] without the `compute-nn-cuda` feature (or without a
    /// working CUDA setup) falls back to CPU inside the backend. Changing the
    /// target drops every cached session so the next task reloads on it.
    pub fn set_execution_target(&self, target: NnTarget) {
        *self.target.write() = target;
        self.graphs.lock().clear();
    }

    pub fn execution_target(&self) -> NnTarget {
        *self.target.read()
    }

    /// Names whose sessions are currently loaded (observability/tests).
    pub fn cached_model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.graphs.lock().keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolves every registered model into a ready [`NnGrant`], fetching
    /// missing blobs (with `provider` — the requester — as the P2P source)
    /// and loading sessions not yet cached.
    pub(crate) async fn grant_for(
        &self,
        fetcher: &Arc<dyn WasmFetcher>,
        provider: NodeId,
    ) -> Result<Arc<NnGrant>, TaskError> {
        let wanted: Vec<(String, Hash)> = self
            .models
            .read()
            .iter()
            .map(|(name, hash)| (name.clone(), *hash))
            .collect();

        let mut ready = HashMap::new();
        let mut missing = Vec::new();
        {
            let cache = self.graphs.lock();
            for (name, hash) in &wanted {
                match cache.get(name) {
                    Some((cached_hash, graph)) if cached_hash == hash => {
                        ready.insert(name.clone(), graph.clone());
                    }
                    _ => missing.push((name.clone(), *hash)),
                }
            }
        }

        let target = self.execution_target();
        for (name, hash) in missing {
            let bytes = fetcher.fetch_wasm(&hash, provider).await.map_err(|e| {
                TaskError::WasmUnavailable(format!("NN model `{name}` ({hash}): {e}"))
            })?;
            let graph = load_onnx_graph(&name, &bytes, target)?;
            self.graphs
                .lock()
                .insert(name.clone(), (hash, graph.clone()));
            ready.insert(name, graph);
        }

        Ok(Arc::new(NnGrant::from_graphs(ready)))
    }
}

/// Whether a working CUDA setup is present (phase NN-4). Asked once and
/// cached: execution-provider availability does not change mid-process.
#[cfg(feature = "compute-nn-cuda")]
pub fn cuda_available() -> bool {
    use ort::execution_providers::{CUDAExecutionProvider, ExecutionProvider};
    static DETECTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| {
        CUDAExecutionProvider::default()
            .is_available()
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// In-memory blob source counting fetches, to observe the session cache.
    struct CountingFetcher {
        blobs: HashMap<Hash, Vec<u8>>,
        fetches: AtomicU32,
    }

    #[async_trait]
    impl WasmFetcher for CountingFetcher {
        async fn fetch_wasm(&self, hash: &Hash, _provider: NodeId) -> Result<Vec<u8>, String> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.blobs
                .get(hash)
                .cloned()
                .ok_or_else(|| "not found".into())
        }
    }

    fn provider() -> NodeId {
        iroh::SecretKey::generate().public()
    }

    #[test]
    fn registration_bookkeeping() {
        let registry = NnModelRegistry::new();
        assert!(registry.is_empty());
        registry.register_model("a", Hash::new(b"model-a"));
        registry.register_model("b", Hash::new(b"model-b"));
        assert_eq!(registry.model_names(), vec!["a", "b"]);
        registry.unregister_model("a");
        assert_eq!(registry.model_names(), vec!["b"]);
    }

    #[test]
    fn execution_target_defaults_to_cpu_and_is_settable() {
        let registry = NnModelRegistry::new();
        assert_eq!(registry.execution_target(), NnTarget::Cpu);
        registry.set_execution_target(NnTarget::Gpu);
        assert_eq!(registry.execution_target(), NnTarget::Gpu);
        // Changing the target drops cached sessions (none here, but the
        // call must be safe) so the next task reloads on the new target.
        assert!(registry.cached_model_names().is_empty());
    }

    #[tokio::test]
    async fn missing_blob_is_reported_as_unavailable() {
        let registry = NnModelRegistry::new();
        registry.register_model("ghost", Hash::new(b"nowhere"));
        let fetcher: Arc<dyn WasmFetcher> = Arc::new(CountingFetcher {
            blobs: HashMap::new(),
            fetches: AtomicU32::new(0),
        });
        let err = registry.grant_for(&fetcher, provider()).await.unwrap_err();
        assert!(matches!(err, TaskError::WasmUnavailable(_)), "got: {err:?}");
    }
}
