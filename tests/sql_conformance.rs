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
// Foreign keys: declared, introspectable AND enforced (MATCH SIMPLE).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn foreign_keys_enforced_on_insert_and_child_update() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    )
    .await;

    // INSERT with no matching parent: 23503 with the PostgreSQL message shape.
    let err = s
        .execute("INSERT INTO child VALUES (1, 999)")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "23503");
    assert_eq!(
        err.to_string(),
        "insert or update on table \"child\" violates foreign key constraint \"child_pid_fkey\""
    );

    // MATCH SIMPLE: a NULL FK column satisfies the constraint.
    ok(&mut s, "INSERT INTO child VALUES (1, NULL)").await;

    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO child VALUES (2, 1)").await;

    // UPDATE of the referencing column re-checks: dangling value fails ...
    assert_eq!(
        err_code(&mut s, "UPDATE child SET pid = 12345 WHERE id = 2").await,
        "23503"
    );
    // ... NULL and an existing parent pass.
    ok(&mut s, "UPDATE child SET pid = NULL WHERE id = 2").await;
    ok(&mut s, "UPDATE child SET pid = 1 WHERE id = 2").await;
}

#[tokio::test]
async fn foreign_keys_restrict_and_no_action_block_parent_delete() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE child_r (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE RESTRICT)",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1), (2)").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 1)").await;
    ok(&mut s, "INSERT INTO child_r VALUES (1, 2)").await;

    // Default NO ACTION: deleting a referenced parent is 23503 with the
    // PostgreSQL message shape.
    let err = s
        .execute("DELETE FROM parent WHERE id = 1")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "23503");
    assert_eq!(
        err.to_string(),
        "update or delete on table \"parent\" violates foreign key constraint \
         \"child_pid_fkey\" on table \"child\""
    );
    // RESTRICT: same outcome (per-statement checking; no deferral).
    assert_eq!(
        err_code(&mut s, "DELETE FROM parent WHERE id = 2").await,
        "23503"
    );
    // Removing the referencing rows unblocks the delete — including within
    // one statement (child rows deleted by the same DELETE's cascade set).
    ok(&mut s, "DELETE FROM child WHERE id = 1").await;
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    // UPDATE of the referenced key is guarded the same way.
    assert_eq!(
        err_code(&mut s, "UPDATE parent SET id = 9 WHERE id = 2").await,
        "23503"
    );
}

#[tokio::test]
async fn foreign_key_on_delete_cascade_multi_level_and_self_referential() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE a (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE b (id INT PRIMARY KEY, aid INT REFERENCES a(id) ON DELETE CASCADE)",
    )
    .await;
    // The second level declares its own action; cascading into `b` applies
    // b's children's actions recursively.
    ok(
        &mut s,
        "CREATE TABLE c (id INT PRIMARY KEY, bid INT REFERENCES b(id) ON DELETE CASCADE)",
    )
    .await;
    ok(&mut s, "INSERT INTO a VALUES (1), (2)").await;
    ok(&mut s, "INSERT INTO b VALUES (10, 1), (20, 2)").await;
    ok(&mut s, "INSERT INTO c VALUES (100, 10), (200, 20)").await;

    ok(&mut s, "DELETE FROM a WHERE id = 1").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM a").await, 1);
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM b").await, 1);
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM c").await, 1);

    // Self-referential CASCADE terminates (chain and even mutual references).
    ok(
        &mut s,
        "CREATE TABLE tree (id INT PRIMARY KEY, \
         parent_id INT REFERENCES tree(id) ON DELETE CASCADE)",
    )
    .await;
    ok(
        &mut s,
        "INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2), (4, NULL)",
    )
    .await;
    ok(&mut s, "DELETE FROM tree WHERE id = 1").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM tree").await, 1);
}

#[tokio::test]
async fn foreign_key_on_delete_set_null() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE SET NULL)",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 1)").await;
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    let r = ok(&mut s, "SELECT pid FROM child WHERE id = 1").await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0].to_text(), None),
        _ => panic!("expected rows"),
    }

    // SET NULL into a NOT NULL column surfaces the usual 23502.
    ok(
        &mut s,
        "CREATE TABLE strict_child (id INT PRIMARY KEY, \
         pid INT NOT NULL REFERENCES parent(id) ON DELETE SET NULL)",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (2)").await;
    ok(&mut s, "INSERT INTO strict_child VALUES (1, 2)").await;
    assert_eq!(
        err_code(&mut s, "DELETE FROM parent WHERE id = 2").await,
        "23502"
    );
}

#[tokio::test]
async fn foreign_key_on_delete_set_default() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, \
         pid INT DEFAULT 1 REFERENCES parent(id) ON DELETE SET DEFAULT)",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1), (2)").await;
    ok(&mut s, "INSERT INTO child VALUES (10, 2)").await;

    // Re-satisfying: the default (1) references a surviving parent.
    ok(&mut s, "DELETE FROM parent WHERE id = 2").await;
    assert_eq!(
        scalar_i64(&mut s, "SELECT pid FROM child WHERE id = 10").await,
        1
    );
    // Violating, default == deleted key: the internal update is a no-op, so
    // the post-SET DEFAULT re-check fires (PostgreSQL raises the parent-side
    // shape here).
    let err = s
        .execute("DELETE FROM parent WHERE id = 1")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "23503");
    assert_eq!(
        err.to_string(),
        "update or delete on table \"parent\" violates foreign key constraint \
         \"child_pid_fkey\" on table \"child\""
    );

    // Violating, default != any parent: the internal update re-checks the FK
    // and fails with the insert-or-update shape.
    ok(
        &mut s,
        "CREATE TABLE child_bad (id INT PRIMARY KEY, \
         pid INT DEFAULT 999 REFERENCES parent(id) ON DELETE SET DEFAULT)",
    )
    .await;
    ok(&mut s, "DELETE FROM child WHERE id = 10").await;
    ok(&mut s, "INSERT INTO child_bad VALUES (1, 1)").await;
    let err = s
        .execute("DELETE FROM parent WHERE id = 1")
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "23503");
    assert_eq!(
        err.to_string(),
        "insert or update on table \"child_bad\" violates foreign key constraint \
         \"child_bad_pid_fkey\""
    );
    ok(&mut s, "DELETE FROM child_bad").await;

    // A column without a default becomes NULL, which satisfies MATCH SIMPLE.
    ok(
        &mut s,
        "CREATE TABLE loose_child (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE SET DEFAULT)",
    )
    .await;
    ok(&mut s, "DELETE FROM child WHERE id = 10").await;
    ok(&mut s, "INSERT INTO loose_child VALUES (1, 1)").await;
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    let r = ok(&mut s, "SELECT pid FROM loose_child WHERE id = 1").await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0].to_text(), None),
        _ => panic!("expected rows"),
    }
}

#[tokio::test]
async fn foreign_key_on_update_actions() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON UPDATE CASCADE)",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1), (2)").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 1)").await;

    // CASCADE rewrites the child's FK value to the new key.
    ok(&mut s, "UPDATE parent SET id = 5 WHERE id = 1").await;
    assert_eq!(
        scalar_i64(&mut s, "SELECT pid FROM child WHERE id = 1").await,
        5
    );
    // Updating an unreferenced parent row (or a non-key column) fires nothing.
    ok(&mut s, "UPDATE parent SET id = 7 WHERE id = 2").await;
    assert_eq!(
        scalar_i64(&mut s, "SELECT pid FROM child WHERE id = 1").await,
        5
    );

    // SET NULL on update.
    ok(
        &mut s,
        "CREATE TABLE child_null (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON UPDATE SET NULL)",
    )
    .await;
    ok(&mut s, "INSERT INTO child_null VALUES (1, 7)").await;
    ok(&mut s, "UPDATE parent SET id = 8 WHERE id = 7").await;
    let r = ok(&mut s, "SELECT pid FROM child_null WHERE id = 1").await;
    match &r[0] {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0].to_text(), None),
        _ => panic!("expected rows"),
    }
}

#[tokio::test]
async fn foreign_key_cascade_is_atomic_and_rolls_back() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE c_cascade (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .await;
    ok(
        &mut s,
        "CREATE TABLE c_restrict (id INT PRIMARY KEY, \
         pid INT REFERENCES parent(id) ON DELETE RESTRICT)",
    )
    .await;
    ok(&mut s, "INSERT INTO parent VALUES (1)").await;
    ok(&mut s, "INSERT INTO c_cascade VALUES (1, 1)").await;
    ok(&mut s, "INSERT INTO c_restrict VALUES (1, 1)").await;

    // The RESTRICT sibling aborts the whole statement: the cascade into
    // c_cascade must not be half-applied.
    assert_eq!(
        err_code(&mut s, "DELETE FROM parent WHERE id = 1").await,
        "23503"
    );
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM parent").await, 1);
    assert_eq!(
        scalar_i64(&mut s, "SELECT count(*) FROM c_cascade").await,
        1
    );

    // A rolled-back transaction undoes the cascade with the delete.
    ok(&mut s, "DELETE FROM c_restrict").await;
    ok(&mut s, "BEGIN").await;
    ok(&mut s, "DELETE FROM parent WHERE id = 1").await;
    assert_eq!(
        scalar_i64(&mut s, "SELECT count(*) FROM c_cascade").await,
        0
    );
    ok(&mut s, "ROLLBACK").await;
    assert_eq!(scalar_i64(&mut s, "SELECT count(*) FROM parent").await, 1);
    assert_eq!(
        scalar_i64(&mut s, "SELECT count(*) FROM c_cascade").await,
        1
    );
}

#[tokio::test]
async fn composite_foreign_key_match_simple() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE cp (a INT, b INT, PRIMARY KEY (a, b))").await;
    ok(
        &mut s,
        "CREATE TABLE cc (id INT PRIMARY KEY, x INT, y INT, \
         FOREIGN KEY (x, y) REFERENCES cp (a, b))",
    )
    .await;
    ok(&mut s, "INSERT INTO cp VALUES (1, 2)").await;
    ok(&mut s, "INSERT INTO cc VALUES (1, 1, 2)").await;
    // MATCH SIMPLE: one NULL component passes, even if the rest dangles.
    ok(&mut s, "INSERT INTO cc VALUES (2, 9, NULL)").await;
    // A fully non-NULL key must match a parent row.
    assert_eq!(
        err_code(&mut s, "INSERT INTO cc VALUES (3, 1, 3)").await,
        "23503"
    );
    // The composite key is guarded on the parent side too.
    assert_eq!(err_code(&mut s, "DELETE FROM cp").await, "23503");
}

#[tokio::test]
async fn deferrable_and_match_full_foreign_keys_rejected() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE dp (id INT PRIMARY KEY)").await;
    // Deferred checking is not implemented: accepting DEFERRABLE and then
    // checking immediately anyway would be a lie — stable 0A000 instead.
    for sql in [
        "CREATE TABLE dc (id INT PRIMARY KEY, pid INT REFERENCES dp(id) DEFERRABLE)",
        "CREATE TABLE dc (id INT PRIMARY KEY, \
         pid INT REFERENCES dp(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TABLE dc (id INT PRIMARY KEY, pid INT REFERENCES dp(id) INITIALLY DEFERRED)",
        "CREATE TABLE dc (id INT PRIMARY KEY, pid INT, \
         FOREIGN KEY (pid) REFERENCES dp(id) DEFERRABLE)",
        "CREATE TABLE dc (id INT PRIMARY KEY, pid INT UNIQUE DEFERRABLE)",
        // Only MATCH SIMPLE is enforced; other MATCH kinds must not be
        // silently downgraded.
        "CREATE TABLE dc (id INT PRIMARY KEY, pid INT REFERENCES dp(id) MATCH FULL)",
    ] {
        assert_eq!(err_code(&mut s, sql).await, "0A000", "for `{sql}`");
    }
    // The defaults the engine implements are accepted.
    ok(
        &mut s,
        "CREATE TABLE dc_ok (id INT PRIMARY KEY, \
         pid INT REFERENCES dp(id) NOT DEFERRABLE INITIALLY IMMEDIATE)",
    )
    .await;
}

#[tokio::test]
async fn referenced_parent_guarded_on_drop_and_truncate() {
    let mut s = session().await;
    ok(&mut s, "CREATE TABLE parent (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    )
    .await;
    // TRUNCATE of a referenced parent is rejected (PostgreSQL 0A000) unless
    // the referencing table is truncated in the same statement.
    assert_eq!(err_code(&mut s, "TRUNCATE parent").await, "0A000");
    ok(&mut s, "TRUNCATE parent, child").await;
    // A referencing constraint blocks DROP TABLE (PostgreSQL 2BP01) ...
    assert_eq!(err_code(&mut s, "DROP TABLE parent").await, "2BP01");
    // ... and CASCADE drops the dependent constraint instead, after which the
    // child accepts previously-dangling values.
    ok(&mut s, "DROP TABLE parent CASCADE").await;
    ok(&mut s, "INSERT INTO child VALUES (1, 999)").await;
    // Dropping parent and child together also works.
    ok(&mut s, "CREATE TABLE p2 (id INT PRIMARY KEY)").await;
    ok(
        &mut s,
        "CREATE TABLE c2 (id INT PRIMARY KEY, pid INT REFERENCES p2(id))",
    )
    .await;
    ok(&mut s, "DROP TABLE p2, c2").await;
    // Foreign keys referencing a missing table are rejected at DDL time now
    // that they are enforced (PostgreSQL 42P01).
    assert_eq!(
        err_code(
            &mut s,
            "CREATE TABLE orphan (id INT PRIMARY KEY, pid INT REFERENCES nowhere(id))"
        )
        .await,
        "42P01"
    );
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
    // pg_constraint reports the declared (and enforced) referential actions:
    // contype 'f'; confdeltype 'c' for the declared CASCADE and 'a'
    // (NO ACTION) otherwise; confupdtype 'a'.
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
