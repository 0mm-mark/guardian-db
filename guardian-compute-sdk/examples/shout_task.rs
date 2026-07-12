//! A Guardian Compute task with real (if tiny) business logic and a failure
//! path: uppercases UTF-8 text, refusing empty input.

use guardian_compute_sdk::{TaskFailure, guardian_task};

#[guardian_task]
fn shout(input: &[u8]) -> Result<Vec<u8>, TaskFailure> {
    if input.is_empty() {
        return Err(TaskFailure::new("nothing to shout"));
    }
    let text = str::from_utf8(input).map_err(|e| TaskFailure::new(format!("not UTF-8: {e}")))?;
    Ok(text.to_uppercase().into_bytes())
}

#[allow(dead_code)]
fn main() {}
