//! Guardian Compute task using the opt-in host capabilities (SDK-3): logs
//! through `gdb.log` and reads the executor's local store via
//! `gdb.store_get`. Only instantiates on executors whose owner granted those
//! capabilities (`HostGrants`); everywhere else the task is rejected with
//! `HostCapabilityDenied` before running.
//!
//! Build: `cargo build -p guardian-compute-sdk --example lookup_task
//!         --features host --target wasm32-unknown-unknown --release`

use guardian_compute_sdk::{TaskFailure, guardian_task, host};

#[guardian_task]
fn lookup(key: &[u8]) -> Result<Vec<u8>, TaskFailure> {
    host::log("lookup task: consulting the executor's local store");
    host::store_get(key).ok_or_else(|| TaskFailure::new("key not found in the executor's store"))
}

#[allow(dead_code)]
fn main() {}
