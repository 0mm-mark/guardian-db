#![cfg(feature = "sql")]
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use guardian_db::sql::RelationalStorage;
use guardian_db::sql::engine::{Database, Session};
use guardian_db::sql::MemoryStorage;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::runtime::Runtime;

fn rt() -> Runtime {
    Runtime::new().unwrap()
}

async fn new_session() -> Session<MemoryStorage> {
    let db = Arc::new(Database::new(Arc::new(MemoryStorage::new()), "bench"));
    Session::new(db, "bench_user")
}

// ---------------------------------------------------------------------------
// Benchmark 1: Single-row INSERT into a table with 5 columns + B-tree index
//              on PK.  The PRIMARY KEY constraint implicitly creates a B-tree
//              index; every insert exercises index maintenance.
// ---------------------------------------------------------------------------
fn bench_insert(c: &mut Criterion) {
    let rt = rt();
    let mut s = rt.block_on(new_session());
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_insert (
                 id    INT PRIMARY KEY,
                 col1  TEXT NOT NULL,
                 col2  INT,
                 col3  FLOAT,
                 col4  BOOL
             )",
        )
        .await
        .unwrap();
    });

    let counter = AtomicU64::new(0);
    c.bench_function("insert_single_row_5col_btree", |b| {
        b.iter(|| {
            let id = counter.fetch_add(1, Ordering::Relaxed) as i64;
            rt.block_on(async {
                s.execute(&format!(
                    "INSERT INTO bench_insert VALUES ({id}, 'text_{id}', {id}, 1.5, true)"
                ))
                .await
                .unwrap()
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 2: SELECT * FROM table WHERE pk = N  (point lookup via index)
// ---------------------------------------------------------------------------
fn bench_point_lookup(c: &mut Criterion) {
    let rt = rt();
    let mut s = rt.block_on(new_session());
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_lookup (
                 id   INT PRIMARY KEY,
                 col1 TEXT,
                 col2 INT,
                 col3 FLOAT,
                 col4 BOOL
             )",
        )
        .await
        .unwrap();
        for i in 0..100i64 {
            let flag = if i % 2 == 0 { "true" } else { "false" };
            s.execute(&format!(
                "INSERT INTO bench_lookup VALUES ({i}, 'val_{i}', {i}, {i}.5, {flag})"
            ))
            .await
            .unwrap();
        }
    });

    let counter = AtomicU64::new(0);
    c.bench_function("select_point_lookup_pk", |b| {
        b.iter(|| {
            let pk = (counter.fetch_add(1, Ordering::Relaxed) % 100) as i64;
            rt.block_on(async {
                s.execute(&format!(
                    "SELECT * FROM bench_lookup WHERE id = {}",
                    black_box(pk)
                ))
                .await
                .unwrap()
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 3: SELECT * FROM table WHERE text_col LIKE 'prefix%'
//              (full table scan with a string-filter predicate)
// ---------------------------------------------------------------------------
fn bench_full_scan(c: &mut Criterion) {
    let rt = rt();
    let mut s = rt.block_on(new_session());
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_scan (
                 id    INT PRIMARY KEY,
                 label TEXT,
                 value INT
             )",
        )
        .await
        .unwrap();
        for i in 0..100i64 {
            let pfx = if i % 3 == 0 { "prefix" } else { "other" };
            s.execute(&format!(
                "INSERT INTO bench_scan VALUES ({i}, '{pfx}_row_{i}', {i})"
            ))
            .await
            .unwrap();
        }
    });

    c.bench_function("select_full_scan_like_prefix", |b| {
        b.iter(|| {
            rt.block_on(async {
                s.execute(black_box(
                    "SELECT * FROM bench_scan WHERE label LIKE 'prefix%'",
                ))
                .await
                .unwrap()
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 4: UPDATE table SET col = val WHERE pk = N  (single-row update)
// ---------------------------------------------------------------------------
fn bench_update(c: &mut Criterion) {
    let rt = rt();
    let mut s = rt.block_on(new_session());
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_update (
                 id  INT PRIMARY KEY,
                 val INT,
                 txt TEXT
             )",
        )
        .await
        .unwrap();
        for i in 0..100i64 {
            s.execute(&format!(
                "INSERT INTO bench_update VALUES ({i}, {i}, 'init_{i}')"
            ))
            .await
            .unwrap();
        }
    });

    let counter = AtomicU64::new(0);
    c.bench_function("update_single_row_by_pk", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            let pk = (n % 100) as i64;
            let new_val = n as i64;
            rt.block_on(async {
                s.execute(&format!(
                    "UPDATE bench_update SET val = {} WHERE id = {}",
                    black_box(new_val),
                    black_box(pk)
                ))
                .await
                .unwrap()
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 5: Bulk INSERT — 1 000 rows in a single benchmark iteration.
//              Each iteration gets a fresh session + table via iter_batched so
//              the measurement covers only the 1 000 INSERT statements.
// ---------------------------------------------------------------------------
fn bench_bulk_insert(c: &mut Criterion) {
    let rt = rt();
    c.bench_function("bulk_insert_1000_rows", |b| {
        b.iter_batched(
            || {
                rt.block_on(async {
                    let mut s = new_session().await;
                    s.execute(
                        "CREATE TABLE bench_bulk (
                             id   INT PRIMARY KEY,
                             col1 TEXT,
                             col2 INT,
                             col3 FLOAT
                         )",
                    )
                    .await
                    .unwrap();
                    s
                })
            },
            |mut s| {
                rt.block_on(async move {
                    for i in 0..1000i64 {
                        s.execute(&format!(
                            "INSERT INTO bench_bulk VALUES ({i}, 'row_{i}', {i}, {i}.0)"
                        ))
                        .await
                        .unwrap();
                    }
                    black_box(s)
                })
            },
            BatchSize::LargeInput,
        )
    });
}

// ---------------------------------------------------------------------------
// Benchmark 6: JOIN two tables on FK  (10 rows in each table)
// ---------------------------------------------------------------------------
fn bench_join(c: &mut Criterion) {
    let rt = rt();
    let mut s = rt.block_on(new_session());
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_dept (
                 id   INT PRIMARY KEY,
                 name TEXT NOT NULL
             )",
        )
        .await
        .unwrap();
        s.execute(
            "CREATE TABLE bench_emp (
                 id      INT PRIMARY KEY,
                 dept_id INT,
                 name    TEXT NOT NULL
             )",
        )
        .await
        .unwrap();
        for i in 0..10i64 {
            s.execute(&format!("INSERT INTO bench_dept VALUES ({i}, 'dept_{i}')"))
                .await
                .unwrap();
        }
        for i in 0..10i64 {
            s.execute(&format!(
                "INSERT INTO bench_emp VALUES ({i}, {}, 'emp_{i}')",
                i % 10
            ))
            .await
            .unwrap();
        }
    });

    c.bench_function("join_two_tables_10_rows_each", |b| {
        b.iter(|| {
            rt.block_on(async {
                s.execute(black_box(
                    "SELECT e.name, d.name \
                     FROM bench_emp e \
                     INNER JOIN bench_dept d ON e.dept_id = d.id \
                     ORDER BY e.id",
                ))
                .await
                .unwrap()
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 7: Expression evaluation — complex WHERE with AND/OR/NOT on
//              integer columns (50-row table; measures evaluator throughput).
// ---------------------------------------------------------------------------
fn bench_expression_eval(c: &mut Criterion) {
    let rt = rt();
    let mut s = rt.block_on(new_session());
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_expr (
                 id INT PRIMARY KEY,
                 a  INT NOT NULL,
                 b  INT NOT NULL,
                 c  INT NOT NULL
             )",
        )
        .await
        .unwrap();
        for i in 0..50i64 {
            s.execute(&format!(
                "INSERT INTO bench_expr VALUES ({i}, {i}, {}, {})",
                i * 2,
                i * 3
            ))
            .await
            .unwrap();
        }
    });

    c.bench_function("expression_eval_and_or_not", |b| {
        b.iter(|| {
            rt.block_on(async {
                s.execute(black_box(
                    "SELECT * FROM bench_expr \
                     WHERE (a > 10 AND b < 80) \
                        OR (NOT (c = 0) AND a <= 5) \
                        OR (a = 25 AND b = 50 AND NOT (c > 100))",
                ))
                .await
                .unwrap()
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 8: Catalog lookup — resolve_table_name called 10 000 times per
//              Criterion iteration.  Measures catalog BTreeMap index
//              effectiveness; storage I/O is excluded (catalog pre-loaded).
// ---------------------------------------------------------------------------
fn bench_catalog_lookup(c: &mut Criterion) {
    let rt = rt();

    // Build a session that writes the catalog to storage, then load it back.
    let storage = Arc::new(MemoryStorage::new());
    let db = Arc::new(Database::new(storage.clone(), "bench"));
    let mut s = Session::new(Arc::clone(&db), "bench_user");
    rt.block_on(async {
        s.execute(
            "CREATE TABLE bench_cat_lookup (
                 id   INT PRIMARY KEY,
                 name TEXT
             )",
        )
        .await
        .unwrap();
    });

    // Deserialize the persisted catalog once; the loop below is pure in-memory.
    let catalog: guardian_db::relational::Catalog = rt.block_on(async {
        let json = storage.load_catalog().await.unwrap().unwrap();
        serde_json::from_value(json).unwrap()
    });

    c.bench_with_input(
        BenchmarkId::new("catalog_resolve_table_name", 10_000),
        &10_000usize,
        |b, &n| {
            b.iter(|| {
                for _ in 0..n {
                    let _ = black_box(
                        catalog.resolve_table_name(None, black_box("bench_cat_lookup")),
                    );
                }
            })
        },
    );
}

criterion_group!(
    benches,
    bench_insert,
    bench_point_lookup,
    bench_full_scan,
    bench_update,
    bench_bulk_insert,
    bench_join,
    bench_expression_eval,
    bench_catalog_lookup
);
criterion_main!(benches);
