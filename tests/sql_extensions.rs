#![cfg(feature = "sql")]
//! End-to-end conformance tests for the PostgreSQL extension mechanism:
//! CREATE/DROP EXTENSION lifecycle, catalog views, function/type/operator
//! gating, GUC configuration, and persistence of installed state in the
//! catalog (the same document that replicates between peers).

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

fn db() -> Arc<Database<MemoryStorage>> {
    Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"))
}

async fn session() -> Session<MemoryStorage> {
    Session::new(db(), "guardian")
}

async fn ok(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<ExecResult> {
    s.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
}

async fn err_code(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match s.execute(sql).await {
        Ok(_) => panic!("expected `{sql}` to fail, but it succeeded"),
        Err(e) => e.sqlstate().to_string(),
    }
}

/// First row/column of a row-producing result, as PostgreSQL text output.
async fn scalar(s: &mut Session<MemoryStorage>, sql: &str) -> Option<String> {
    match ok(s, sql).await.pop() {
        Some(ExecResult::Rows { rows, .. }) => {
            rows.first().and_then(|r| r.first()).and_then(|v| v.to_text())
        }
        other => panic!("`{sql}` did not produce rows: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_extension_gates_functions() {
    let mut s = session().await;
    // Not installed: typed undefined-function error naming the extension.
    assert_eq!(&err_code(&mut s, "SELECT uuid_generate_v4()").await, "42883");
    ok(&mut s, "CREATE EXTENSION \"uuid-ossp\"").await;
    let u = scalar(&mut s, "SELECT uuid_generate_v4()").await.unwrap();
    assert_eq!(u.len(), 36);
}

#[tokio::test]
async fn create_extension_unknown_name_is_typed() {
    let mut s = session().await;
    assert_eq!(&err_code(&mut s, "CREATE EXTENSION postgis").await, "0A000");
}

#[tokio::test]
async fn create_extension_duplicate_and_if_not_exists() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION pgcrypto").await;
    assert_eq!(&err_code(&mut s, "CREATE EXTENSION pgcrypto").await, "42710");
    ok(&mut s, "CREATE EXTENSION IF NOT EXISTS pgcrypto").await;
}

#[tokio::test]
async fn create_extension_bad_version() {
    let mut s = session().await;
    assert_eq!(
        &err_code(&mut s, "CREATE EXTENSION pg_trgm WITH VERSION '9.9'").await,
        "42704"
    );
}

#[tokio::test]
async fn drop_extension_lifecycle() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION pg_trgm").await;
    ok(&mut s, "DROP EXTENSION pg_trgm").await;
    assert_eq!(&err_code(&mut s, "DROP EXTENSION pg_trgm").await, "42704");
    ok(&mut s, "DROP EXTENSION IF EXISTS pg_trgm").await;
    // Functions are gated again after drop.
    assert_eq!(
        &err_code(&mut s, "SELECT similarity('a','b')").await,
        "42883"
    );
}

#[tokio::test]
async fn plpgsql_is_preinstalled_like_postgres() {
    let mut s = session().await;
    let v = scalar(
        &mut s,
        "SELECT extversion FROM pg_extension WHERE extname = 'plpgsql'",
    )
    .await;
    assert_eq!(v.as_deref(), Some("1.0"));
    ok(&mut s, "DROP EXTENSION plpgsql").await;
}

#[tokio::test]
async fn drop_extension_with_dependent_table_is_blocked() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION citext").await;
    ok(&mut s, "CREATE TABLE users (email CITEXT PRIMARY KEY)").await;
    // RESTRICT (default) refuses; CASCADE refuses explicitly (no implicit
    // data destruction) — both typed.
    assert_eq!(&err_code(&mut s, "DROP EXTENSION citext").await, "0A000");
    assert_eq!(
        &err_code(&mut s, "DROP EXTENSION citext CASCADE").await,
        "0A000"
    );
    ok(&mut s, "DROP TABLE users").await;
    ok(&mut s, "DROP EXTENSION citext").await;
}

// ---------------------------------------------------------------------------
// Catalog views
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_available_extensions_lists_registry() {
    let mut s = session().await;
    let n = scalar(&mut s, "SELECT count(*) FROM pg_available_extensions").await;
    let count: i64 = n.unwrap().parse().unwrap();
    assert!(count >= 10, "registry should list all bundled extensions");
    // installed_version reflects state.
    ok(&mut s, "CREATE EXTENSION unaccent").await;
    let v = scalar(
        &mut s,
        "SELECT installed_version FROM pg_available_extensions WHERE name = 'unaccent'",
    )
    .await;
    assert_eq!(v.as_deref(), Some("1.1"));
}

#[tokio::test]
async fn pg_extension_reflects_installs() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION vector").await;
    let v = scalar(
        &mut s,
        "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
    )
    .await;
    assert_eq!(v.as_deref(), Some("0.8.1"));
    let installed = scalar(
        &mut s,
        "SELECT installed FROM pg_available_extension_versions WHERE name = 'vector'",
    )
    .await;
    assert_eq!(installed.as_deref(), Some("t"));
}

// ---------------------------------------------------------------------------
// Persistence: installed state lives in the catalog document (the replicated
// unit), so a fresh session over the SAME storage sees it; a fresh storage
// does not.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extension_state_persists_across_sessions() {
    let storage = Arc::new(MemoryStorage::new());
    let database = Arc::new(Database::new(storage.clone(), "app"));
    let mut s1 = Session::new(database.clone(), "guardian");
    ok(&mut s1, "CREATE EXTENSION pgcrypto").await;
    drop(s1);
    // New session, same storage: still installed (catalog was saved).
    let database2 = Arc::new(Database::new(storage, "app"));
    let mut s2 = Session::new(database2, "guardian");
    let d = scalar(&mut s2, "SELECT encode(digest('abc','sha256'),'hex')").await;
    assert_eq!(
        d.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    // Fresh storage: not installed.
    let mut s3 = session().await;
    assert_eq!(&err_code(&mut s3, "SELECT digest('a','md5')").await, "42883");
}

// ---------------------------------------------------------------------------
// Functions, operators, types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_trgm_operators_and_gucs() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION pg_trgm").await;
    assert_eq!(
        scalar(&mut s, "SELECT similarity('word','two words')").await.as_deref(),
        Some("0.36363637")
    );
    // Default threshold 0.3: 'word' % 'two words' is true.
    assert_eq!(
        scalar(&mut s, "SELECT 'word' % 'two words'").await.as_deref(),
        Some("t")
    );
    // Raise the threshold via SET; the operator observes it.
    ok(&mut s, "SET pg_trgm.similarity_threshold = 0.5").await;
    assert_eq!(
        scalar(&mut s, "SELECT 'word' % 'two words'").await.as_deref(),
        Some("f")
    );
    // set_limit()/show_limit() round-trip and persist for the session.
    assert_eq!(
        scalar(&mut s, "SELECT set_limit(0.2)").await.as_deref(),
        Some("0.2")
    );
    assert_eq!(
        scalar(&mut s, "SELECT show_limit()").await.as_deref(),
        Some("0.2")
    );
    assert_eq!(
        scalar(&mut s, "SELECT 'word' % 'two words'").await.as_deref(),
        Some("t")
    );
}

#[tokio::test]
async fn citext_column_semantics() {
    let mut s = session().await;
    // Type is gated on the extension.
    assert_eq!(
        &err_code(&mut s, "CREATE TABLE t (e CITEXT)").await,
        "42704"
    );
    ok(&mut s, "CREATE EXTENSION citext").await;
    ok(&mut s, "CREATE TABLE t (e CITEXT UNIQUE)").await;
    ok(&mut s, "INSERT INTO t VALUES ('Alice@Example.COM')").await;
    // Case-insensitive UNIQUE.
    assert_eq!(
        &err_code(&mut s, "INSERT INTO t VALUES ('alice@example.com')").await,
        "23505"
    );
    // Case-insensitive comparison, original case preserved on output.
    assert_eq!(
        scalar(&mut s, "SELECT e FROM t WHERE e = 'ALICE@EXAMPLE.COM'")
            .await
            .as_deref(),
        Some("Alice@Example.COM")
    );
}

#[tokio::test]
async fn vector_column_and_distance_operators() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION vector").await;
    ok(&mut s, "CREATE TABLE items (id INT PRIMARY KEY, v VECTOR(2))").await;
    ok(&mut s, "INSERT INTO items VALUES (1,'[1,2]'), (2,'[4,6]')").await;
    // Dimension enforcement.
    assert_eq!(
        &err_code(&mut s, "INSERT INTO items VALUES (3,'[1,2,3]')").await,
        "42804"
    );
    assert_eq!(
        scalar(&mut s, "SELECT v <-> '[4,6]'::vector FROM items WHERE id = 1")
            .await
            .as_deref(),
        Some("5")
    );
    assert_eq!(
        scalar(&mut s, "SELECT l2_distance('[1,2]'::vector,'[4,6]'::vector)")
            .await
            .as_deref(),
        Some("5")
    );
    // ORDER BY nearest-neighbour, the canonical pgvector query shape.
    assert_eq!(
        scalar(
            &mut s,
            "SELECT id FROM items ORDER BY v <-> '[3.9,5.9]'::vector LIMIT 1"
        )
        .await
        .as_deref(),
        Some("2")
    );
}

#[tokio::test]
async fn fuzzystrmatch_and_unaccent_functions() {
    let mut s = session().await;
    ok(&mut s, "CREATE EXTENSION fuzzystrmatch").await;
    ok(&mut s, "CREATE EXTENSION unaccent").await;
    assert_eq!(
        scalar(&mut s, "SELECT levenshtein('kitten','sitting')").await.as_deref(),
        Some("3")
    );
    assert_eq!(
        scalar(&mut s, "SELECT soundex('Margaret')").await.as_deref(),
        Some("M626")
    );
    assert_eq!(
        scalar(&mut s, "SELECT unaccent('Hôtel')").await.as_deref(),
        Some("Hotel")
    );
}

// ---------------------------------------------------------------------------
// Session configuration (SHOW / current_setting)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn show_and_current_setting() {
    let mut s = session().await;
    ok(&mut s, "SET application_name = 'conformance'").await;
    assert_eq!(
        scalar(&mut s, "SHOW application_name").await.as_deref(),
        Some("conformance")
    );
    assert_eq!(
        scalar(&mut s, "SELECT current_setting('application_name')")
            .await
            .as_deref(),
        Some("conformance")
    );
    // Extension GUC default is visible without SET once registered.
    assert_eq!(
        scalar(&mut s, "SHOW pg_trgm.similarity_threshold").await.as_deref(),
        Some("0.3")
    );
    // Unknown parameter: typed.
    assert_eq!(&err_code(&mut s, "SHOW no_such_parameter").await, "42704");
    // current_setting(name, true) is NULL-forgiving.
    assert_eq!(
        scalar(&mut s, "SELECT current_setting('nope', true)").await,
        None
    );
}
