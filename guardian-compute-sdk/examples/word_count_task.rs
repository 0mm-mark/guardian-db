//! Typed Guardian Compute task (SDK-3): CBOR-encoded parameter and result
//! via `#[guardian_task(cbor)]` — the RFC 0002 §8.2 convention ("opaque bytes
//! on the protocol, CBOR as the SDK convention").
//!
//! Build: `cargo build -p guardian-compute-sdk --example word_count_task
//!         --features cbor --target wasm32-unknown-unknown --release`

use guardian_compute_sdk::{TaskFailure, guardian_task};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Document {
    text: String,
}

#[derive(Serialize)]
struct Stats {
    words: usize,
    chars: usize,
}

#[guardian_task(cbor)]
fn word_count(doc: Document) -> Result<Stats, TaskFailure> {
    if doc.text.is_empty() {
        return Err(TaskFailure::new("empty document"));
    }
    Ok(Stats {
        words: doc.text.split_whitespace().count(),
        chars: doc.text.chars().count(),
    })
}

#[allow(dead_code)]
fn main() {}
