//! Proc-macro half of the Guardian Compute SDK: `#[guardian_task]`.
//!
//! Turns an ordinary function taking `&[u8]` and returning `Vec<u8>` (or
//! `Result<Vec<u8>, E: Display>`) into an entrypoint conforming to the
//! Guardian Compute guest ABI: an exported `(ptr: i32, len: i32) -> i64`
//! wrapper whose wasm export name is the function's own name — exactly what
//! `ExecuteRequest::entrypoint` refers to.
//!
//! The user's function is left untouched and callable; the wrapper is a
//! sibling named `__guardian_task_<name>` carrying
//! `#[unsafe(export_name = "<name>")]`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Error, FnArg, ItemFn, parse_macro_input};

/// The task's I/O convention, chosen by the attribute argument.
enum Mode {
    /// `#[guardian_task]` — raw bytes in, raw bytes out.
    Bytes,
    /// `#[guardian_task(cbor)]` — typed parameter and result, CBOR-encoded
    /// (requires the SDK's `cbor` feature).
    Cbor,
}

/// Marks a function as a Guardian Compute task entrypoint.
///
/// Accepted signatures:
/// - `#[guardian_task]` (raw bytes):
///   - `fn name(input: &[u8]) -> Vec<u8>`
///   - `fn name(input: &[u8]) -> Result<Vec<u8>, E>` where `E: Display`
/// - `#[guardian_task(cbor)]` (typed, SDK feature `cbor`):
///   - `fn name(input: In) -> Result<Out, E>` where `In: DeserializeOwned`,
///     `Out: Serialize`, `E: Display`
///
/// An `Err` panics with the message, surfacing as a trap on the executor.
#[proc_macro_attribute]
pub fn guardian_task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mode = if attr.is_empty() {
        Mode::Bytes
    } else {
        match syn::parse::<syn::Ident>(attr) {
            Ok(ident) if ident == "cbor" => Mode::Cbor,
            Ok(ident) => {
                return Error::new_spanned(
                    &ident,
                    "unknown #[guardian_task] argument; expected `cbor` or nothing",
                )
                .to_compile_error()
                .into();
            }
            Err(e) => return e.to_compile_error().into(),
        }
    };

    let task_fn = parse_macro_input!(item as ItemFn);
    if let Err(error) = validate_signature(&task_fn) {
        return error.to_compile_error().into();
    }

    let name = &task_fn.sig.ident;
    let export_name = name.to_string();
    let wrapper = format_ident!("__guardian_task_{}", name);

    let body = match mode {
        Mode::Bytes => quote! {
            let __input: &[u8] = unsafe { ::guardian_compute_sdk::abi::input(ptr, len) };
            let __output = ::guardian_compute_sdk::abi::IntoTaskOutput::into_task_output(
                #name(__input),
            );
            ::guardian_compute_sdk::abi::emit(__output)
        },
        Mode::Cbor => quote! {
            let __input: &[u8] = unsafe { ::guardian_compute_sdk::abi::input(ptr, len) };
            let __arg = ::guardian_compute_sdk::cbor::decode(__input);
            ::guardian_compute_sdk::cbor::emit_result(#name(__arg))
        },
    };

    quote! {
        #task_fn

        #[unsafe(export_name = #export_name)]
        pub extern "C" fn #wrapper(ptr: i32, len: i32) -> i64 {
            #body
        }
    }
    .into()
}

/// The wrapper hard-codes how the function is called, so reject shapes that
/// would generate confusing type errors deep inside the expansion.
fn validate_signature(task_fn: &ItemFn) -> Result<(), Error> {
    let sig = &task_fn.sig;
    if sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            sig.asyncness,
            "#[guardian_task] functions are synchronous: the sandbox has no async runtime",
        ));
    }
    if sig.inputs.len() != 1 {
        return Err(Error::new_spanned(
            &sig.inputs,
            "#[guardian_task] expects exactly one parameter: the input bytes (`&[u8]`)",
        ));
    }
    if matches!(sig.inputs.first(), Some(FnArg::Receiver(_))) {
        return Err(Error::new_spanned(
            sig.inputs.first(),
            "#[guardian_task] applies to free functions, not methods",
        ));
    }
    if sig.generics.params.iter().next().is_some() {
        return Err(Error::new_spanned(
            &sig.generics,
            "#[guardian_task] functions cannot be generic: the export is a concrete symbol",
        ));
    }
    Ok(())
}
