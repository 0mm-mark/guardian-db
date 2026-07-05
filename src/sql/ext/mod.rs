//! PostgreSQL extension support: the `CREATE EXTENSION` mechanism and the
//! native implementations of the supported extension set.
//!
//! GuardianDB's SQL engine is a from-scratch Rust engine, so PostgreSQL's
//! binary extension ABI (C shared libraries loaded into the server) cannot
//! apply. Instead, a fixed registry of extensions is implemented natively.
//! `CREATE EXTENSION` flips a per-database catalog flag (persisted in the
//! replicated catalog document, so installs replicate like any other DDL) and
//! gates the extension's functions, operators, and types. Anything not in the
//! registry fails `CREATE EXTENSION` with a typed error naming
//! `pg_available_extensions` — never silently.
//!
//! Registry lookups are linear over a fixed, single-digit-sized set; every
//! function-dispatch miss costs one short scan, which is noise next to the
//! per-statement table loads.

pub mod citext;
pub mod fuzzystrmatch;
pub mod pg_trgm;
pub mod pgcrypto;
pub mod unaccent;
pub mod uuid_ossp;
pub mod vector;

use crate::relational::catalog::Catalog;
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use chrono::{DateTime, Utc};
use std::cell::RefCell;
use std::collections::HashMap;

/// A configuration variable owned by an extension (set via `SET name = value`).
pub struct GucSpec {
    pub name: &'static str,
    pub default: &'static str,
}

/// Context passed to extension function calls.
pub struct ExtCtx<'a> {
    /// Statement timestamp (same instant `now()` reports).
    pub now: DateTime<Utc>,
    /// Session variables (`SET x.y = v`); extensions read their GUCs and may
    /// write them (e.g. `set_limit`). Written values persist for the session.
    pub vars: &'a RefCell<HashMap<String, String>>,
}

impl ExtCtx<'_> {
    /// Read a session variable, falling back to the registered GUC default.
    pub fn get_var(&self, name: &str) -> Option<String> {
        if let Some(v) = self.vars.borrow().get(name) {
            return Some(v.clone());
        }
        default_guc(name).map(str::to_string)
    }

    pub fn set_var(&self, name: &str, value: impl Into<String>) {
        self.vars
            .borrow_mut()
            .insert(name.to_string(), value.into());
    }

    /// Read a float-valued GUC with a hard fallback.
    pub fn get_f32(&self, name: &str, fallback: f32) -> f32 {
        self.get_var(name)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(fallback)
    }
}

type CallFn = fn(&ExtCtx, &str, &[SqlValue]) -> Result<SqlValue>;

/// A natively implemented (or accepted no-op) extension.
pub struct ExtensionDef {
    pub name: &'static str,
    pub default_version: &'static str,
    pub comment: &'static str,
    /// Extensions that must be installed first (`CASCADE` installs them).
    pub requires: &'static [&'static str],
    /// Scalar function names this extension provides (lower-case).
    pub functions: &'static [&'static str],
    /// SQL type names this extension provides (lower-case).
    pub types: &'static [&'static str],
    /// Configuration variables this extension owns.
    pub gucs: &'static [GucSpec],
    /// `pg_available_extension_versions.trusted`.
    pub trusted: bool,
    /// Function-call entry point; `None` for extensions that contribute no
    /// callable objects (index-method shims, procedural languages).
    pub call: Option<CallFn>,
}

/// The `plpgsql` shim: PostgreSQL ships every database with it installed. Our
/// engine has no procedural language, but reporting it installed keeps ORMs
/// and migration tools (which assume its presence) working; `DROP EXTENSION
/// plpgsql` is honoured like in PostgreSQL.
static PLPGSQL: ExtensionDef = ExtensionDef {
    name: "plpgsql",
    default_version: "1.0",
    comment: "PL/pgSQL procedural language (accepted for compatibility; \
              function bodies are not executable in GuardianDB)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
};

/// Index-method shims: GuardianDB indexes are engine-native, so GIN/GiST
/// operator-class extensions install as no-ops purely so migrations that
/// `CREATE EXTENSION btree_gin` succeed. `CREATE INDEX` behaviour is unchanged.
static BTREE_GIN: ExtensionDef = ExtensionDef {
    name: "btree_gin",
    default_version: "1.3",
    comment: "no-op compatibility shim (GuardianDB indexes are engine-native)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
};

static BTREE_GIST: ExtensionDef = ExtensionDef {
    name: "btree_gist",
    default_version: "1.7",
    comment: "no-op compatibility shim (GuardianDB indexes are engine-native)",
    requires: &[],
    functions: &[],
    types: &[],
    gucs: &[],
    trusted: true,
    call: None,
};

/// Every extension that `CREATE EXTENSION` accepts, in `pg_available_extensions`
/// order. Names not in this list are rejected with a typed error.
static AVAILABLE: [&ExtensionDef; 10] = [
    &BTREE_GIN,
    &BTREE_GIST,
    &citext::DEF,
    &fuzzystrmatch::DEF,
    &pg_trgm::DEF,
    &pgcrypto::DEF,
    &PLPGSQL,
    &unaccent::DEF,
    &uuid_ossp::DEF,
    &vector::DEF,
];

pub fn available() -> &'static [&'static ExtensionDef] {
    &AVAILABLE
}

pub fn find(name: &str) -> Option<&'static ExtensionDef> {
    let lower = name.to_ascii_lowercase();
    available().iter().copied().find(|d| d.name == lower)
}

/// The extension that provides scalar function `func`, if any.
pub fn function_owner(func: &str) -> Option<&'static ExtensionDef> {
    available()
        .iter()
        .copied()
        .find(|d| d.functions.contains(&func))
}

/// The GUC default registered by any extension, if `name` is a known GUC.
pub fn default_guc(name: &str) -> Option<&'static str> {
    available()
        .iter()
        .flat_map(|d| d.gucs.iter())
        .find(|g| g.name == name)
        .map(|g| g.default)
}

/// The extension name that provides SQL type `ty`, if it is extension-owned.
fn type_owner(ty: &SqlType) -> Option<&'static str> {
    match ty {
        SqlType::Citext => Some("citext"),
        SqlType::Vector(_) => Some("vector"),
        SqlType::Array(inner) => type_owner(inner),
        _ => None,
    }
}

/// DDL gate: error (like PostgreSQL's `type "citext" does not exist`) when a
/// column uses an extension type whose extension is not installed.
pub fn check_type_usable(catalog: &Catalog, ty: &SqlType) -> Result<()> {
    if let Some(owner) = type_owner(ty)
        && !catalog.extension_installed(owner)
    {
        return Err(SqlError::UndefinedType(format!(
            "{} — provided by extension \"{owner}\"; run CREATE EXTENSION {owner}",
            ty.name()
        )));
    }
    Ok(())
}

/// Tables whose columns depend on `ext` (blocks `DROP EXTENSION ... RESTRICT`).
pub fn dependent_tables(catalog: &Catalog, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    for table in catalog.tables() {
        if table.columns.iter().any(|c| type_owner(&c.ty) == Some(ext)) {
            out.push(format!("{}.{}", table.schema, table.name));
        }
    }
    out
}

/// Scalar-function dispatch, called from the engine's unknown-function
/// fallthrough. `None` = the name belongs to no known extension (caller keeps
/// its generic error). `Some(Err)` = owned but not installed, or the call
/// itself failed.
pub fn dispatch_function(
    catalog: &Catalog,
    ctx: &ExtCtx,
    name: &str,
    args: &[SqlValue],
) -> Option<Result<SqlValue>> {
    let def = function_owner(name)?;
    if !catalog.extension_installed(def.name) {
        return Some(Err(SqlError::UndefinedFunction(format!(
            "{name}({}) — provided by extension \"{}\"; run CREATE EXTENSION \"{}\"",
            args.iter()
                .map(|a| a.type_of().name())
                .collect::<Vec<_>>()
                .join(", "),
            def.name,
            def.name
        ))));
    }
    let call = def.call?;
    Some(call(ctx, name, args))
}

/// Operator dispatch for extension-owned operators (`%`, `<->`, `<=>`, `<#>`,
/// `<+>`, `<%`, `%>`). Returns `None` when neither operand shape nor installed
/// extensions claim the operator, so the caller's normal error path applies.
/// SQL NULL semantics: any NULL operand yields NULL.
pub fn dispatch_operator(
    catalog: &Catalog,
    ctx: &ExtCtx,
    op: &str,
    left: &SqlValue,
    right: &SqlValue,
) -> Option<Result<SqlValue>> {
    let text_pair = both_text(left, right);
    let vector_pair = matches!(
        (left, right),
        (SqlValue::Vector(_), SqlValue::Vector(_))
            | (SqlValue::Vector(_), SqlValue::Null)
            | (SqlValue::Null, SqlValue::Vector(_))
            | (SqlValue::Null, SqlValue::Null)
    );
    match op {
        // pg_trgm operators on text operands.
        "%" | "<%" | "%>" | "<<%" | "%>>"
            if text_pair && catalog.extension_installed("pg_trgm") =>
        {
            Some(pg_trgm::operator(ctx, op, left, right))
        }
        "<->" if text_pair && catalog.extension_installed("pg_trgm") => {
            Some(pg_trgm::operator(ctx, op, left, right))
        }
        // pgvector distance operators.
        "<->" | "<#>" | "<=>" | "<+>" if vector_pair && catalog.extension_installed("vector") => {
            Some(vector::operator(op, left, right))
        }
        _ => None,
    }
}

fn both_text(l: &SqlValue, r: &SqlValue) -> bool {
    let textual =
        |v: &SqlValue| matches!(v, SqlValue::Text(_) | SqlValue::Citext(_) | SqlValue::Null);
    textual(l) && textual(r)
}

/// NULL-propagation helper shared by the extension modules: if any argument is
/// SQL NULL the function result is NULL (all supported functions are strict).
pub(crate) fn any_null(args: &[SqlValue]) -> bool {
    args.iter().any(SqlValue::is_null)
}

/// Extract a text argument (Text or Citext) at `idx`.
pub(crate) fn arg_text(args: &[SqlValue], idx: usize, func: &str) -> Result<String> {
    match args.get(idx) {
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone()),
        Some(other) => other
            .to_text()
            .ok_or_else(|| bad_arg(func, idx, "text", other)),
        None => Err(SqlError::UndefinedFunction(format!(
            "{func}: missing argument {}",
            idx + 1
        ))),
    }
}

/// Extract a bytea argument at `idx` (text coerces to its UTF-8 bytes,
/// matching PostgreSQL's implicit text -> bytea behaviour in pgcrypto calls).
pub(crate) fn arg_bytes(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<u8>> {
    match args.get(idx) {
        Some(SqlValue::Bytea(b)) => Ok(b.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone().into_bytes()),
        Some(other) => Err(bad_arg(func, idx, "bytea", other)),
        None => Err(SqlError::UndefinedFunction(format!(
            "{func}: missing argument {}",
            idx + 1
        ))),
    }
}

pub(crate) fn arg_i64(args: &[SqlValue], idx: usize, func: &str) -> Result<i64> {
    args.get(idx)
        .and_then(SqlValue::as_i64)
        .ok_or_else(|| match args.get(idx) {
            Some(other) => bad_arg(func, idx, "integer", other),
            None => SqlError::UndefinedFunction(format!("{func}: missing argument {}", idx + 1)),
        })
}

pub(crate) fn arg_f64(args: &[SqlValue], idx: usize, func: &str) -> Result<f64> {
    args.get(idx)
        .and_then(SqlValue::as_f64)
        .ok_or_else(|| match args.get(idx) {
            Some(other) => bad_arg(func, idx, "double precision", other),
            None => SqlError::UndefinedFunction(format!("{func}: missing argument {}", idx + 1)),
        })
}

pub(crate) fn arg_vector(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<f32>> {
    match args.get(idx) {
        Some(SqlValue::Vector(v)) => Ok(v.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::Vector(None))? {
                SqlValue::Vector(v) => Ok(v),
                _ => unreachable!("from_text(vector) yields Vector"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "vector", other)),
        None => Err(SqlError::UndefinedFunction(format!(
            "{func}: missing argument {}",
            idx + 1
        ))),
    }
}

fn bad_arg(func: &str, idx: usize, want: &str, got: &SqlValue) -> SqlError {
    SqlError::CannotCoerce {
        from: got.type_of().name(),
        to: format!("{want} (argument {} of {func})", idx + 1),
    }
}

/// Unknown sub-function name inside an extension module: internal error,
/// because `functions` and the dispatch match must stay in sync.
pub(crate) fn no_such(func: &str) -> SqlError {
    SqlError::Internal(format!("extension function {func} not routed"))
}
