//! Golden test of the Guardian Compute SDK (RFC 0003, SDK-1/SDK-2): the
//! `.wasm` produced by `guardian-compute-sdk` + `#[guardian_task]` must run
//! under the real executor-side `WasmRuntime` — any drift between the SDK's
//! guest ABI and the runtime's host ABI breaks here.
//!
//! Requires the `wasm32-unknown-unknown` target (`rustup target add
//! wasm32-unknown-unknown`); the test skips itself with a message when the
//! target is missing, so plain dev machines are not forced to install it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use guardian_db::compute::{HostGrants, HostStoreReader, ResourceLimits, TaskError, WasmRuntime};
use serde::{Deserialize, Serialize};

/// Builds the SDK examples for wasm32 once per test process and returns the
/// artifact directory, or `None` (with a message) when the target is absent.
fn build_examples() -> Option<PathBuf> {
    static RESULT: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let output = Command::new(env!("CARGO"))
                .current_dir(&workspace)
                .args([
                    "build",
                    "-p",
                    "guardian-compute-sdk",
                    "--features",
                    "cbor,host",
                    "--example",
                    "echo_task",
                    "--example",
                    "shout_task",
                    "--example",
                    "word_count_task",
                    "--example",
                    "lookup_task",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--release",
                ])
                .output()
                .expect("spawn cargo");
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("wasm32-unknown-unknown") {
                    eprintln!(
                        "skipping SDK golden test: the wasm32-unknown-unknown target is not \
                         installed (rustup target add wasm32-unknown-unknown)"
                    );
                    return None;
                }
                panic!("building SDK wasm examples failed:\n{stderr}");
            }
            Some(workspace.join("target/wasm32-unknown-unknown/release/examples"))
        })
        .clone()
}

fn load_task(runtime: &WasmRuntime, artifact: &str) -> Option<guardian_db::compute::CompiledTask> {
    let dir = build_examples()?;
    let wasm = std::fs::read(dir.join(artifact)).expect("read wasm artifact");
    Some(runtime.compile(&wasm).expect("SDK wasm must compile"))
}

#[test]
fn sdk_echo_task_runs_under_the_real_runtime() {
    let runtime = WasmRuntime::new().expect("runtime");
    let Some(task) = load_task(&runtime, "echo_task.wasm") else {
        return;
    };

    let input = b"escrito com o sdk, executado na sandbox";
    let exec = runtime
        .execute(&task, "echo", input, &ResourceLimits::default())
        .expect("echo execution");
    assert_eq!(exec.output, input);
    assert!(exec.metrics.fuel_consumed > 0);

    // Empty input must round-trip too (gdb_alloc(0) path).
    let exec = runtime
        .execute(&task, "echo", b"", &ResourceLimits::default())
        .expect("empty echo");
    assert!(exec.output.is_empty());
}

#[test]
fn sdk_result_task_returns_ok_and_traps_on_err() {
    let runtime = WasmRuntime::new().expect("runtime");
    let Some(task) = load_task(&runtime, "shout_task.wasm") else {
        return;
    };

    // Ok path: real logic (uppercase), through the typed Result signature.
    let exec = runtime
        .execute(
            &task,
            "shout",
            "guardian compute".as_bytes(),
            &ResourceLimits::default(),
        )
        .expect("shout execution");
    assert_eq!(exec.output, b"GUARDIAN COMPUTE");

    // Err path: the SDK turns Err into a panic, which reaches the executor
    // as a clean trap — never as garbage output.
    let err = runtime
        .execute(&task, "shout", b"", &ResourceLimits::default())
        .unwrap_err();
    assert!(
        matches!(err, TaskError::Trapped(_)),
        "expected a trap from the Err path, got: {err:?}"
    );
}

/// Mirrors the guest-side `Document`/`Stats` of `word_count_task.rs` — the
/// CBOR contract between requester and task author (RFC 0002 §8.2).
#[derive(Serialize)]
struct Document {
    text: String,
}

#[derive(Deserialize, Debug, PartialEq)]
struct Stats {
    words: usize,
    chars: usize,
}

#[test]
fn sdk_cbor_task_speaks_typed_cbor_end_to_end() {
    let runtime = WasmRuntime::new().expect("runtime");
    let Some(task) = load_task(&runtime, "word_count_task.wasm") else {
        return;
    };

    let mut input = Vec::new();
    ciborium::into_writer(
        &Document {
            text: "computacao distribuida com tipos".into(),
        },
        &mut input,
    )
    .expect("encode input");

    let exec = runtime
        .execute(&task, "word_count", &input, &ResourceLimits::default())
        .expect("word_count execution");
    let stats: Stats = ciborium::from_reader(exec.output.as_slice()).expect("decode output");
    assert_eq!(
        stats,
        Stats {
            words: 4,
            chars: 32
        }
    );

    // Garbage input must trap (the SDK panics on malformed CBOR), never
    // produce a bogus decoded value.
    let err = runtime
        .execute(&task, "word_count", b"\xff\xff", &ResourceLimits::default())
        .unwrap_err();
    assert!(matches!(err, TaskError::Trapped(_)), "got: {err:?}");
}

struct FixtureStore;

impl HostStoreReader for FixtureStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        (key == b"config:tema").then(|| b"escuro".to_vec())
    }
}

#[test]
fn sdk_host_bindings_match_the_runtime_host_abi() {
    let runtime = WasmRuntime::new().expect("runtime");
    let Some(task) = load_task(&runtime, "lookup_task.wasm") else {
        return;
    };

    // Ungranted executor: the SDK-built module imports gdb.log/store_get, so
    // instantiation is refused before any code runs.
    let err = runtime
        .execute(&task, "lookup", b"config:tema", &ResourceLimits::default())
        .unwrap_err();
    assert!(
        matches!(err, TaskError::HostCapabilityDenied(_)),
        "got: {err:?}"
    );

    // Granted executor: the guest reads the local store through the SDK's
    // two-call binding. (`..default()` covers the wasi-nn field under
    // `compute-nn`.)
    #[cfg_attr(not(feature = "compute-nn"), allow(clippy::needless_update))]
    let grants = HostGrants {
        log: true,
        store: Some(std::sync::Arc::new(FixtureStore)),
        ..HostGrants::default()
    };
    let exec = runtime
        .execute_with_host(
            &task,
            "lookup",
            b"config:tema",
            &ResourceLimits::default(),
            &grants,
        )
        .expect("granted lookup");
    assert_eq!(exec.output, b"escuro");

    // Missing key: the task's Err path becomes a trap.
    let err = runtime
        .execute_with_host(
            &task,
            "lookup",
            b"nao-existe",
            &ResourceLimits::default(),
            &grants,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Trapped(_)), "got: {err:?}");
}

#[test]
fn sdk_task_respects_resource_limits() {
    let runtime = WasmRuntime::new().expect("runtime");
    let Some(task) = load_task(&runtime, "echo_task.wasm") else {
        return;
    };

    // A starved fuel budget must abort even a trivial SDK task cleanly.
    let limits = ResourceLimits {
        fuel: 10,
        ..ResourceLimits::default()
    };
    let err = runtime.execute(&task, "echo", b"x", &limits).unwrap_err();
    assert_eq!(err, TaskError::FuelExhausted);
}
