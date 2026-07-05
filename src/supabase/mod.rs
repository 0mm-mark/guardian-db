//! # Supabase-compatible HTTP gateway
//!
//! A Kong-shaped HTTP surface (`/rest/v1`, `/auth/v1`, ...) in front of the
//! GuardianDB [`sql`](crate::sql) engine, so that Supabase client libraries
//! (`supabase-js`, PostgREST clients, GoTrue clients) can talk to a GuardianDB
//! node with no GuardianDB-specific code. Enabled by the `supabase` feature
//! (which implies `sql`). Default builds are entirely unaffected.
//!
//! This module is a *foundation* slice: **REST** (PostgREST-compatible) and
//! **Auth** (GoTrue-compatible) are implemented end-to-end; the other Kong
//! services (realtime, storage, functions, graphql, pg-meta) return typed
//! `501 Not Implemented` errors — never a bare 404 and never fake success.
//!
//! ## Scouted seams (Stage 0)
//!
//! Everything here is built strictly on the engine's public surface; no file
//! under `src/sql/**` or `src/relational/**` is modified.
//!
//! * [`Database<S>`](crate::sql::engine::Database) — the shared, storage-backed
//!   database. Built with `Database::new(Arc<S>, name)` where `S:
//!   RelationalStorage`. Backends: [`MemoryStorage`](crate::relational::MemoryStorage)
//!   (tests / in-memory binary) and
//!   [`GuardianRelationalStorage`](crate::sql::GuardianRelationalStorage) via
//!   [`open_sql`](crate::sql::open_sql) (persistent, Iroh-replicated).
//! * [`Session<S>`](crate::sql::engine::Session) — a connection-scoped session.
//!   `Session::new(Arc<Database<S>>, username)`; the `username` is the role the
//!   statement runs as. We open **one session per HTTP request**, bound to the
//!   request's resolved Postgres role (`anon` / `authenticated` /
//!   `service_role`) — the seam an RLS-enforcement slice will hook into.
//! * SQL execution: `Session::prepare(sql) -> Prepared` then
//!   `Session::execute_one(&Prepared.statement, &[SqlValue])` runs a **single**
//!   parameterised statement (`$1`, `$2`, ...). This is the injection-safe path
//!   we use for REST/Auth data operations. `Session::execute(sql)` runs a
//!   multi-statement string (no params) — used only for the auth-schema
//!   bootstrap DDL.
//! * [`ExecResult`](crate::sql::ExecResult): `Rows { fields: Vec<OutField>,
//!   rows: Vec<Vec<SqlValue>> }` or `Command { tag }`. `OutField` carries the
//!   column `name` and [`SqlType`](crate::relational::SqlType); rows are
//!   rendered to JSON in [`rest`] via [`rest::value_to_json`].
//! * [`SqlValue`](crate::relational::SqlValue) / [`SqlType`]: value model with
//!   `to_text()` / `from_text(text, ty)`. REST coerces filter/body string values
//!   to the *declared column type* (read from the [`Catalog`](crate::relational::Catalog))
//!   via `SqlValue::from_text`, so numeric/temporal comparisons are typed rather
//!   than lexical.
//! * [`RelError`](crate::relational::RelError)`::sqlstate()` — every engine error
//!   carries a PostgreSQL SQLSTATE, mapped to PostgREST/GoTrue error shapes in
//!   [`error`].
//! * Crypto: `bcrypt` (from the pgcrypto work) hashes/verifies passwords;
//!   `hmac` + `sha2` + `base64` (already in-tree for `sql`) implement HS256 JWTs
//!   from scratch in [`jwt`] — no `jsonwebtoken` dependency added.
//!
//! ## Architecture
//!
//! ```text
//!   HTTP request
//!     │
//!     ├─ request_id middleware        (x-request-id: read or generate)
//!     │
//!     ├─ apikey middleware            (verify `apikey` JWT against project keys,
//!     │     (rest + auth only)         verify optional `Authorization: Bearer`,
//!     │                                resolve effective Postgres role,
//!     │                                attach AuthContext extension)
//!     │
//!     ├─ /rest/v1/*   → rest.rs   → Session(role) → SQL → PostgREST JSON
//!     ├─ /auth/v1/*   → auth.rs   → Session(service_role) → auth.* tables
//!     └─ /storage|/realtime|/functions|/graphql|/pg-meta → 501 typed
//! ```
//!
//! Each request opens a fresh [`Session`] bound to the resolved role. A single
//! [`SupabaseCompatProject`] is served per gateway instance (the "single-project
//! shell").

pub mod auth;
pub mod error;
pub mod gateway;
pub mod jwt;
pub mod project;
pub mod rest;

pub use error::SupaError;
pub use gateway::{AppState, build_router};
pub use jwt::{Claims, JwtError};
pub use project::{ProjectKeys, Secret, ServiceConfig, SupabaseCompatProject};
