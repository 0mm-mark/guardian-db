//! The simplest possible Guardian Compute task: echoes its input.
//!
//! Build: `cargo build -p guardian-compute-sdk --example echo_task
//!         --target wasm32-unknown-unknown --release`
//! Then publish the resulting `.wasm` to the blob store and delegate with
//! `entrypoint: "echo"`.

use guardian_compute_sdk::guardian_task;

#[guardian_task]
fn echo(input: &[u8]) -> Vec<u8> {
    input.to_vec()
}

// `crate-type = ["cdylib"]` examples still need a `main` on host targets;
// the wasm build ignores it.
#[allow(dead_code)]
fn main() {}
