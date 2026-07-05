#![cfg(feature = "sql")]
//! Conformance tests that pin down GuardianDB's documented PostgreSQL gaps.
//!
//! Two kinds of test live here:
//!   * **Clean-failure tests** assert that an unsupported feature fails with a
//!     precise SQLSTATE rather than silently misbehaving. These pass today and
//!     guard against accidental "fake success".
//!   * **`#[ignore]` tests** describe features that are intentionally not yet
//!     implemented; they encode the intended behaviour for when they are. Run
//!     them with `cargo test -- --ignored` to see what remains.
//!
//! Every gap listed in `docs/postgres-compat.md` has a corresponding test here.

use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::{ExecResult, MemoryStorage};
use std::sync::Arc;

async fn session() -> Session<MemoryStorage> {
    let db = Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"));
    Session::new(db, "guardian")
}

/// Execute SQL and return the SQLSTATE of the resulting error (panics if it
/// unexpectedly succeeds).
async fn err_code(s: &mut Session<MemoryStorage>, sql: &str) -> String {
    match s.execute(sql).await {
        Ok(_) => panic!("expected `{sql}` to fail, but it succeeded"),
        Err(e) => e.sqlstate().to_string(),
    }
}

async fn ok(s: &mut Session<MemoryStorage>, sql: &str) -> Vec<ExecResult> {
    s.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
}

/// First column of the first row, as i64 (for `SELECT count(*) ...`).
async fn scalar_i64(s: &mut Session<MemoryStorage>, sql: &str) -> i64 {
    let r = ok(s, sql).await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => rows[0][0]
            .to_text()
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(|| panic!("`{sql}` did not return an integer scalar")),
        _ => panic!("expected rows from `{sql}`"),
    }
}

// ---------------------------------------------------------------------------
// Clean-failure gaps (these tests PASS — the feature fails with a clear code).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn window_functions_unsupported() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (id INT)").await;
    ok(&mut s, "INSERT INTO t VALUES (1), (2)").await;
    // 0A000 = feature_not_supported — for OVER *anywhere* in the query, not
    // just the top-level projection. In particular, OVER in HAVING used to be
    // silently evaluated as a plain aggregate.
    for sql in [
        "SELECT row_number() OVER (ORDER BY id) FROM t",
        "SELECT * FROM (SELECT row_number() OVER (ORDER BY id) AS rn FROM t) q",
        "SELECT id FROM t GROUP BY id HAVING count(*) OVER () > 0",
        "SELECT id FROM t WHERE row_number() OVER () = 1",
        "SELECT id FROM t GROUP BY row_number() OVER ()",
        "SELECT id FROM t ORDER BY row_number() OVER ()",
        "SELECT sum(id) OVER w FROM t WINDOW w AS (ORDER BY id)",
    ] {
        assert_eq!(err_code(&mut s, sql).await, "0A000", "for `{sql}`");
    }
}

#[tokio::test]
async fn with_recursive_unsupported() {
    let mut s = session().await;
    // Self-referencing: must fail 0A000 up front — not with the internal
    // 42P01 on the self-reference (which sidecar routing could forward).
    assert_eq!(
        err_code(
            &mut s,
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) \
             SELECT sum(n) FROM c",
        )
        .await,
        "0A000"
    );
    // Non-self-referencing: CTEs materialize non-recursively, so this used to
    // silently succeed with base-case rows only — forbidden degraded
    // semantics. The RECURSIVE keyword itself is the rejection trigger.
    assert_eq!(
        err_code(
            &mut s,
            "WITH RECURSIVE c AS (SELECT 1 AS n) SELECT * FROM c"
        )
        .await,
        "0A000"
    );
    // Inside a subquery.
    assert_eq!(
        err_code(
            &mut s,
            "SELECT * FROM (WITH RECURSIVE c AS (SELECT 1 AS n) SELECT * FROM c) q",
        )
        .await,
        "0A000"
    );
}

#[tokio::test]
async fn set_returning_function_in_from_unsupported() {
    let mut s = session().await;
    assert_eq!(
        err_code(&mut s, "SELECT * FROM generate_series(1, 5)").await,
        "0A000"
    );
}

#[tokio::test]
async fn nested_with_in_subquery_unsupported() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (n INT)").await;
    let code = err_code(
        &mut s,
        "SELECT * FROM (WITH x AS (SELECT 1) SELECT * FROM x) q",
    )
    .await;
    assert_eq!(code, "0A000");
}

#[tokio::test]
async fn copy_not_supported_by_engine() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (id INT)").await;
    // COPY requires wire-protocol CopyIn/CopyOut framing, which is not
    // implemented. The engine rejects it rather than pretending.
    assert_eq!(err_code(&mut s, "COPY t FROM STDIN").await, "0A000");
}

#[tokio::test]
async fn create_function_unsupported() {
    let mut s = session().await;
    // Stable 0A000 for every spelling, including the ones sqlparser cannot
    // parse (keyword-prefix detection runs before the parser).
    for sql in [
        "CREATE FUNCTION add(a int, b int) RETURNS int AS 'select a + b' LANGUAGE sql",
        "CREATE OR REPLACE FUNCTION add(a int, b int) RETURNS int AS 'select a + b' LANGUAGE sql",
        "CREATE FUNCTION f() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
    ] {
        assert_eq!(err_code(&mut s, sql).await, "0A000", "for `{sql}`");
    }
}

#[tokio::test]
async fn create_procedure_unsupported() {
    let mut s = session().await;
    // sqlparser 0.62 cannot parse the PostgreSQL form of CREATE PROCEDURE at
    // all — without prefix detection this would leak a 42601 syntax error
    // instead of the stable feature rejection.
    for sql in [
        "CREATE PROCEDURE p() LANGUAGE sql AS 'SELECT 1'",
        "CREATE OR REPLACE PROCEDURE p() LANGUAGE sql AS 'SELECT 1'",
        "CREATE PROCEDURE p2() AS BEGIN SELECT 1; END",
    ] {
        assert_eq!(err_code(&mut s, sql).await, "0A000", "for `{sql}`");
    }
}

#[tokio::test]
async fn create_trigger_unsupported() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (id INT)").await;
    for sql in [
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE CONSTRAINT TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE TRIGGER trg AFTER UPDATE ON t FOR EACH ROW WHEN (1 = 1) EXECUTE PROCEDURE f()",
        "DROP TRIGGER trg ON t",
    ] {
        assert_eq!(err_code(&mut s, sql).await, "0A000", "for `{sql}`");
    }
}

#[tokio::test]
async fn materialized_view_unsupported() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (id INT)").await;
    let code = err_code(&mut s, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM t").await;
    // Either feature-not-supported or a parser-level rejection is acceptable;
    // what matters is that it does not silently "succeed".
    assert!(code == "0A000" || code == "42601", "got {code}");
}

#[tokio::test]
async fn full_text_search_unsupported() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (body TEXT)").await;
    ok(&mut s, "INSERT INTO t VALUES ('a cat sat')").await;
    // The FTS function family is *named*-unsupported: stable 0A000. It must
    // never be 42883/"does not exist" — these are PostgreSQL core functions,
    // and 42883 is also what triggers sidecar fallback-routing, which would
    // make FTS semantics differ per deployment.
    for sql in [
        "SELECT * FROM t WHERE to_tsvector(body) @@ to_tsquery('cat')",
        "SELECT to_tsvector('a cat sat')",
        "SELECT to_tsquery('cat')",
        "SELECT plainto_tsquery('cat')",
        "SELECT phraseto_tsquery('the cat')",
        "SELECT websearch_to_tsquery('cat -dog')",
        "SELECT ts_rank('a', 'b')",
        "SELECT ts_rank_cd('a', 'b')",
        "SELECT ts_headline('doc', 'query')",
        "SELECT setweight('a', 'A')",
        "SELECT ts_delete('a', 'b')",
        "SELECT tsvector_to_array('a')",
        // The @@ operator itself lands on the unsupported-binary-operator
        // rejection when no FTS function is involved.
        "SELECT body @@ 'cat' FROM t",
    ] {
        assert_eq!(err_code(&mut s, sql).await, "0A000", "for `{sql}`");
    }
    // The tsvector/tsquery *types* do not exist in the engine: 42704
    // (undefined_object), like any unknown type name — truthful, and
    // deliberately distinct from the 0A000 feature rejection above.
    assert_eq!(err_code(&mut s, "SELECT 'a'::tsvector").await, "42704");
    assert_eq!(err_code(&mut s, "SELECT 'a'::tsquery").await, "42704");
}

// ---------------------------------------------------------------------------
// Foreign keys: declared + introspectable, NOT enforced (truthfulness pins).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn foreign_keys_declared_but_not_enforced() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE child_cascade (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .await;

    // The engine performs NO referential checks. These pins document the
    // actual behaviour so it cannot drift silently; if FK enforcement is ever
    // implemented, each `ok` below should flip to a 23503 assertion.
    //
    // No existence check on INSERT (PostgreSQL: 23503) ...
    ok(&mut s, "INSERT INTO child VALUES (1, 999)").await;
    // ... nor on UPDATE of the referencing column.
    ok(&mut s, "UPDATE child SET pid = 12345 WHERE id = 1").await;

    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO child VALUES (2, 1)").await;
    ok(&mut s, "INSERT INTO child_cascade VALUES (1, 1)").await;

    // Deleting a referenced parent succeeds (PostgreSQL: 23503 under the
    // default NO ACTION) ...
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    // ... and a declared ON DELETE CASCADE does NOT cascade: the child rows
    // must survive. (Half-implementing the cascade would be fabricated
    // semantics; the declared action is catalog metadata only.)
    assert_eq!(
        scalar_i64(&mut s, "SELECT count(*) FROM child_cascade").await,
        1
    );
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM child").await, 2);
}

#[tokio::test]
async fn foreign_keys_introspectable_with_declared_actions() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE child_cascade (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .await;
    // pg_constraint reports the declared referential actions even though they
    // are not executed: contype 'f'; confdeltype 'c' for the declared CASCADE
    // and 'a' (NO ACTION) otherwise; confupdtype 'a'.
    let r = ok(
        &mut s,
        "SELECT conname, contype, confupdtype, confdeltype \
         FROM pg_constraint WHERE contype = 'f' ORDER BY conname",
    )
    .await;
    let rows = match &r[0] {
        ExecResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    let text = |row: &[guardian_db::relational::SqlValue]| -> Vec<String> {
        row.iter()
            .map(|v| v.to_text().unwrap_or_default())
            .collect()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(
        text(&rows[0]),
        ["child_cascade_pid_fkey", "f", "a", "c"]
            .map(str::to_string)
            .to_vec()
    );
    assert_eq!(
        text(&rows[1]),
        ["child_pid_fkey", "f", "a", "a"]
            .map(str::to_string)
            .to_vec()
    );
}

// ---------------------------------------------------------------------------
// Intended-but-unimplemented features (ignored; encode the target behaviour).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "SAVEPOINT partial rollback is not implemented; ROLLBACK TO collapses to a full rollback"]
async fn savepoint_partial_rollback() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)").await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "INSERT INTO t VALUES (1)").await;
    ok(&mut s, "SAVEPOINT sp1").await;
    ok(&mut s, "INSERT INTO t VALUES (2)").await;
    ok(&mut s, "ROLLBACK TO SAVEPOINT sp1").await;
    ok(&mut s, "COMMIT").await;
    // Intended: only row 1 survives.
    let r = ok(&mut s, "SELECT count(*) FROM t").await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0].to_text().unwrap(), "1"),
        _ => panic!("expected rows"),
    }
}

#[tokio::test]
#[ignore = "SERIALIZABLE isolation is not implemented; local-atomic / read-committed only"]
async fn serializable_isolation() {
    // Intended: two concurrent transactions that would create a write skew are
    // serialized, with one aborting with 40001 (serialization_failure). The
    // strict-mode coordinator (single-writer) is the planned mechanism.
    let mut s = session().await;
    ok(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").await;
}

#[tokio::test]
#[ignore = "Generated/computed columns (GENERATED ALWAYS AS) are not implemented"]
async fn generated_columns() {
    let mut s = session().await;
    ok(
        &mut s,
        "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED)",
    )
    .await;
    ok(&mut s, "INSERT INTO t (a) VALUES (5)").await;
    let r = ok(&mut s, "SELECT b FROM t").await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0].to_text().unwrap(), "10"),
        _ => panic!("expected rows"),
    }
}
