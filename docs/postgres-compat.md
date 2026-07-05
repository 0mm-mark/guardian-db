# PostgreSQL compatibility

GuardianDB ships a **PostgreSQL-compatible relational layer** on top of its
local-first, P2P document model. Existing PostgreSQL clients — `psql`,
node-postgres, TypeORM, DBeaver — connect to GuardianDB over the standard
PostgreSQL wire protocol and run ordinary SQL, with no GuardianDB-specific
client code.

```
TypeORM / node-postgres / psql / DBeaver
        │  PostgreSQL wire protocol (v3)
        ▼
guardian_db::pgwire   ── startup/auth, simple + extended query, prepared statements
        │
        ▼
guardian_db::sql      ── parser (sqlparser) → planner → executor; DDL/DML;
        │                 transactions; information_schema / pg_catalog
        ▼
guardian_db::relational ── types, values, catalog, indexes, RelationalStorage
        │
        ▼
GuardianDB document / key-value storage  ──►  Iroh-backed replication
```

The relational core lives in the `guardian-db` crate as the feature-gated
`relational`, `sql`, and `pgwire` modules (under `src/`, enabled by the `sql` /
`pgwire` features). The engine itself is storage-agnostic — it is driven entirely
through the `RelationalStorage` trait. The default gateway uses an in-memory
store; the GuardianDB-backed store maps rows onto replicated `iroh-docs`
documents while preserving the local-first model.

---

## 1. Starting the gateway

```bash
cargo run --features pgwire --bin guardian-pgwire        # listens on 127.0.0.1:15432, database "app"

# options:
cargo run --features pgwire --bin guardian-pgwire -- --addr 127.0.0.1:15432 --database app --username guardian
```

| Flag             | Default            | Notes                                            |
| ---------------- | ------------------ | ------------------------------------------------ |
| `--addr`         | `127.0.0.1:15432`  | Bind address                                     |
| `--database`     | `app`              | Logical database name                            |
| `--username`     | `guardian`         | Reported role                                    |
| `--path`         | —                  | Data directory (GuardianDB-backed gateway)       |
| `--consistency`  | `local`            | `local` or `strict` (GuardianDB-backed gateway)  |
| `--peer`         | —                  | Iroh peer to replicate with (repeatable)         |

Authentication is **trust** by default (any username/password connects); the
configured username is reported to clients. The server answers SSL negotiation
requests with "SSL not supported" and continues in cleartext.

### In-memory vs. Iroh-backed gateway

The `guardian-pgwire` binary above serves an **in-memory** relational store —
ideal for development and the TypeORM conformance tests, but not persistent or
replicated (it ignores `--path`/`--consistency`/`--peer`).

For the real **local-first, replicated** path — *PostgreSQL on a GuardianDB /
Iroh node* — use the `postgres_iroh_gateway` example. It opens an Iroh-backed
GuardianDB document store and serves SQL over it, so every table created through
TypeORM / `psql` / node-postgres becomes an ordinary GuardianDB document that
persists locally and replicates to peers over Iroh (Willow range reconciliation,
LWW CRDT):

```bash
cargo run --features pgwire --example postgres_iroh_gateway
#   options: -- --addr 127.0.0.1:15432 --database app --path ./guardian_pg_data
```

It prints the node's Iroh id; start a second instance elsewhere (different
`--addr`/`--path`) and the two replicate via mDNS / n0 discovery, so rows written
through TypeORM on one node converge to the other's gateway. Wiring it yourself
is three calls — open a GuardianDB node, open SQL over it, serve:

```rust
use guardian_db::guardian::GuardianDB;
use guardian_db::sql::open_sql;
use guardian_db::pgwire::serve;

// `db`: a live GuardianDB node (see the Quick Start in the README).
let database = open_sql(&db, "app").await?;            // RelationalStorage over GuardianDB docs
serve("127.0.0.1:15432", database, "guardian".to_string()).await?;
```

A client (TypeORM, `psql`, node-postgres, DBeaver) connects to this Iroh-backed
gateway **exactly** as to the in-memory one — same wire protocol, same SQL, no
GuardianDB-specific code. The full runnable source is
[`examples/postgres_iroh_gateway.rs`](../examples/postgres_iroh_gateway.rs).

## 2. Connection strings

Every standard client connects with an ordinary PostgreSQL connection string
(URI). With the default gateway flags the canonical string is:

```
postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable
```

- **user/password** — `guardian`/`guardian` by default (`--username` to change;
  the gateway accepts the password without enforcing it).
- **database** — `app` by default (`--database`).
- **`sslmode=disable`** — the gateway is a plaintext loopback TCP socket and
  does not negotiate TLS. libpq-based clients (`psql`, DBeaver, Python's
  `psycopg`) may try SSL first, so pass `sslmode=disable` explicitly;
  node-postgres and TypeORM default to no-SSL and work without it.

### `psql`

```bash
psql 'postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable'
```

```sql
CREATE TABLE users (id SERIAL PRIMARY KEY, email TEXT UNIQUE NOT NULL, data JSONB);
INSERT INTO users (email, data) VALUES ('a@x.com', '{"plan":"pro"}') RETURNING id;
SELECT count(*) FROM users;
```

### node-postgres (`pg`)

```ts
import pg from "pg";

const client = new pg.Client({
  connectionString: "postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable",
});
await client.connect();
const { rows } = await client.query("SELECT count(*) FROM users");
await client.end();
```

## 3. Connecting with TypeORM (`type: "postgres"`)

This is the required, primary path. No custom code:

```ts
import { DataSource } from "typeorm";

const ds = new DataSource({
  type: "postgres",
  host: "127.0.0.1",
  port: 15432,
  username: "guardian",
  password: "guardian",
  database: "app",
  synchronize: true,         // or run migrations
  entities: [User, Post, Org],
});
await ds.initialize();
```

Or equivalently with a single connection string via `url`:

```ts
const ds = new DataSource({
  type: "postgres",
  url: process.env.DATABASE_URL ?? "postgres://guardian:guardian@127.0.0.1:15432/app",
  ssl: false,
  synchronize: true,
  entities: [User, Post, Org],
});
```

`synchronize`, schema **re-introspection**, migrations (`QueryRunner`),
repositories, `EntityManager`, QueryBuilder, transactions and relation metadata
all work. See `examples/postgres-typeorm` for a complete app and
`tests/postgres-compat` for the conformance suite.

## 4. Native GuardianDB TypeORM driver (optional)

`@guardiandb/postgres-typeorm` provides `GuardianDataSource`, which manages an embedded
gateway and otherwise behaves like a normal `DataSource`:

```ts
import { GuardianDataSource } from "@guardiandb/postgres-typeorm";

const ds = new GuardianDataSource({
  path: "./data",
  database: "app",
  peers: [],
  consistency: "strict",
  entities: [User, Post, Org],
});
await ds.initialize();
```

---

## 5. Supported SQL

### DDL
- `CREATE SCHEMA`, `DROP SCHEMA [CASCADE]`
- `CREATE TABLE` / `CREATE TABLE IF NOT EXISTS`
- `ALTER TABLE ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN`
- `ALTER TABLE ALTER COLUMN SET/DROP DEFAULT`, `SET/DROP NOT NULL`, `SET DATA TYPE`
- `ALTER TABLE ADD/DROP CONSTRAINT`, `RENAME TO`
- `DROP TABLE` / `DROP TABLE IF EXISTS`, `TRUNCATE`
- `CREATE INDEX`, `CREATE UNIQUE INDEX`, `DROP INDEX`
- `CREATE VIEW`, `DROP VIEW`

### DML
- `INSERT`, multi-row `INSERT`, `INSERT ... RETURNING`, `DEFAULT VALUES`
- `UPDATE`, `UPDATE ... RETURNING`
- `DELETE`, `DELETE ... RETURNING`
- `INSERT ... ON CONFLICT DO NOTHING` / `DO UPDATE` (with `excluded`)

### SELECT
- projection, aliases, `WHERE`, `AND`/`OR`/`NOT`
- `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `IN (list)`, `IN (subquery)`,
  `BETWEEN`, `LIKE`/`ILIKE` (with `ESCAPE`)
- `ORDER BY` (ASC/DESC, NULLS FIRST/LAST, by output alias or position),
  `LIMIT`/`OFFSET`, `DISTINCT`
- `GROUP BY`, `HAVING`, aggregates `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`
  (incl. `COUNT(DISTINCT ...)`), `bool_and`/`bool_or`, `string_agg`, `array_agg`
- `INNER`/`LEFT`/`RIGHT`/`FULL`/`CROSS JOIN`, `USING`, `NATURAL`
- scalar / `IN` / `EXISTS` subqueries (correlated supported)
- `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`
- non-recursive `WITH` (CTEs)
- parameterized queries (`$1`, ...)
- `x = ANY(array)` / `ALL(array)`, `UNNEST(array)` in projections

### Expressions & functions
- arithmetic (`+ - * / %`), integer vs numeric vs float semantics,
  division-by-zero errors
- boolean three-valued logic, string comparison, NULL semantics
- `CAST` / `::type`, including `::regclass` → OID resolution
- `CASE`, `COALESCE`, `NULLIF`, `GREATEST`, `LEAST`
- string: `upper`, `lower`, `length`, `trim`/`btrim`/`ltrim`/`rtrim`,
  `substring`, `position`, `replace`, `concat`, `concat_ws`, `left`, `right`
- numeric: `abs`, `ceil`, `floor`, `round`, `trunc`, `sign`, `sqrt`, `power`,
  `mod`
- temporal: `now()`, `current_timestamp`, `current_date`, `current_time`,
  `EXTRACT`
- identity: `gen_random_uuid()`, `uuid_generate_v4()`
- session: `current_schema()`, `current_database()`, `current_user`, `version()`

### Types
`boolean`, `smallint`/`int2`, `integer`/`int4`, `bigint`/`int8`,
`serial`/`bigserial`/`smallserial`, `real`/`float4`, `double precision`/`float8`,
`numeric`/`decimal`, `text`, `varchar(n)`, `char(n)`, `bytea`, `uuid`, `date`,
`time`, `timestamp`, `timestamptz`, `json`, `jsonb`, and one-dimensional arrays.

`numeric` preserves exact precision (decimal-backed). `bigint` is stored exactly;
values beyond JS `Number.MAX_SAFE_INTEGER` are transmitted as text (node-postgres
exposes them as strings — use `pg.types` parsers if you need `BigInt`).

### Constraints & indexes
- `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `DEFAULT`, `CHECK`
- `FOREIGN KEY` with `ON DELETE`/`ON UPDATE` actions (metadata + parsing;
  **local enforcement of cascade/restrict is a documented gap**, see below)
- real BTree indexes: primary-key, unique, secondary, composite; maintained on
  insert/update/delete; used for equality index scans (the planner chooses an
  index scan when a single base table is filtered by `col = const` on an indexed
  column, and falls back to a full scan otherwise — results are identical).

### Catalog / introspection
Queryable `information_schema` (`tables`, `columns`, `schemata`,
`table_constraints`, `key_column_usage`, `constraint_column_usage`,
`referential_constraints`, `views`) and `pg_catalog` (`pg_class`,
`pg_attribute`, `pg_type`, `pg_namespace`, `pg_index`, `pg_constraint`,
`pg_database`, `pg_indexes`, `pg_attrdef`, `pg_am`, `pg_roles`, and empty
`pg_description`/`pg_enum`/`pg_collation`/`pg_settings`). This is enough for
TypeORM schema sync, migrations and QueryRunner inspection, and for
node-postgres metadata.

---

## 6. Extensions

GuardianDB's SQL engine is a from-scratch Rust engine, so PostgreSQL's binary
extension ABI (C shared libraries loaded into the server) cannot apply.
`CREATE EXTENSION` is still fully supported, through a **fixed registry** of
extensions (`src/sql/ext/`): installing one flips a per-database flag in the
replicated catalog document (so installs replicate like any other DDL) and
gates that extension's functions, operators, types and GUCs. Anything outside
the registry fails with a typed `0A000` error pointing at
`pg_available_extensions` — never silently.

### Registry

Every registry entry declares a **runtime strategy**: `native` (implemented
inside the engine) or `sidecar` (delegated to a managed PostgreSQL sidecar
process, see below). The strategy is surfaced as the `runtime` column of
`pg_available_extensions` — a GuardianDB extension column that PostgreSQL does
not have.

| Extension             | Version | Runtime | Provides                                                        |
| --------------------- | ------- | ------- | --------------------------------------------------------------- |
| `btree_gin`           | 1.3     | native  | no-op shim (GuardianDB indexes are engine-native)                |
| `btree_gist`          | 1.7     | native  | no-op shim                                                       |
| `citext`              | 1.6     | native  | case-insensitive `CITEXT` type (comparison, UNIQUE, output case) |
| `fuzzystrmatch`       | 1.2     | native  | `levenshtein`, `soundex`, `difference`, `metaphone`, `dmetaphone`|
| `pg_stat_statements`  | 1.10    | sidecar | statement planning/execution statistics                          |
| `pg_trgm`             | 1.6     | native  | `similarity`, `%`/`<->` operators, `pg_trgm.similarity_threshold`|
| `pgcrypto`            | 1.3     | native  | `digest`, `hmac`, `crypt`/`gen_salt`, `encode`/`decode`, ...     |
| `plpgsql`             | 1.0     | native  | pre-installed shim (function bodies are not executable)          |
| `postgis`             | 3.4.2   | sidecar | spatial types and functions                                      |
| `timescaledb`         | 2.15.2  | sidecar | time-series hypertables and queries                              |
| `unaccent`            | 1.1     | native  | `unaccent()` accent stripping                                    |
| `uuid-ossp`           | 1.1     | native  | `uuid_generate_v1/v3/v4/v5`, namespace constants                 |
| `vector`              | 0.8.1   | native  | pgvector: `VECTOR(n)` type, `<->`/`<#>`/`<=>`/`<+>`, distances   |

Introspection matches PostgreSQL: `pg_extension`, `pg_available_extensions`,
and `pg_available_extension_versions` are queryable; functions of a
not-installed extension fail with `42883` naming the extension to install, and
extension-owned types fail DDL with `42704` until installed. `DROP EXTENSION`
honours RESTRICT when table columns depend on an extension type (and refuses
CASCADE explicitly rather than destroying data).

`pg_depend` reports the dependencies GuardianDB tracks, with PostgreSQL's
catalog class OIDs: one `deptype = 'n'` row per installed extension
(`classid = 3079` → `refclassid = 2615`, the `pg_catalog` namespace), and one
row per table column whose type is extension-owned (`classid = 1259`,
`objid` = table OID, `objsubid` = column attnum, `refclassid = 3079`,
`refobjid` = the extension's `pg_extension` row) — the same relationship that
blocks `DROP EXTENSION`.

### ALTER EXTENSION

sqlparser (0.62) has no `ALTER EXTENSION` AST, so the session recognizes it
*before* the general parser: input is split into top-level statements with a
quote-/comment-aware splitter, `ALTER EXTENSION` segments are hand-parsed, and
everything else flows through the normal parser unchanged, in order.

| Form                                   | Behaviour                                                        |
| -------------------------------------- | ---------------------------------------------------------------- |
| `ALTER EXTENSION x UPDATE`             | updates the stored version to the registry version (`ALTER EXTENSION` tag) |
| `ALTER EXTENSION x UPDATE TO 'v'`      | same if `v` is the available version; otherwise `42704` naming it |
| on a not-installed extension           | `42704` (`extension "x" does not exist`)                          |
| `ALTER EXTENSION x SET SCHEMA s`       | `0A000` — no registry extension is relocatable                    |
| `ALTER EXTENSION x ADD/DROP object`    | `0A000` — PostgreSQL reserves membership changes for extension scripts |

`ALTER EXTENSION` participates in transactions like any DDL (staged on the
open block, aborts it on error). It is **simple-protocol only**: preparing it
through the extended query protocol fails with a `42601` error saying so.

---

## 7. Unsupported SQL (documented gaps)

Each gap has a conformance test in `tests/sql_conformance.rs`
(clean-failure tests pass; intended-future features are `#[ignore]`d).

| Feature                              | Status | Behaviour                              |
| ------------------------------------ | ------ | -------------------------------------- |
| Window functions (`OVER`)            | ✗      | error `0A000`                          |
| `WITH RECURSIVE`                     | ✗      | ignored test (non-recursive CTEs only) |
| Set-returning funcs in `FROM`        | ✗      | error `0A000` (scalar table funcs ok)  |
| `WITH` inside a subquery             | ✗      | error `0A000` (top-level `WITH` ok)    |
| `COPY` (bulk load)                   | ✗      | error `0A000` (no CopyIn/Out framing)  |
| Materialized views                   | ✗      | error `0A000`                          |
| `CREATE FUNCTION` / procedures / triggers | ✗ | error `0A000`                          |
| Full-text search (`tsvector`/`@@`)   | ✗      | error                                  |
| Generated/computed columns           | ✗      | ignored test                           |
| `SAVEPOINT` partial rollback         | partial| `SAVEPOINT`/`RELEASE` no-op; `ROLLBACK TO` collapses to full rollback |
| FK cascade/restrict enforcement      | partial| constraints parsed + introspectable; referential actions not enforced |
| `SERIALIZABLE` isolation             | ✗      | ignored test (read-committed only)     |
| SSL/TLS transport                    | ✗      | negotiated-away, cleartext             |
| Binary result encoding               | ✗      | results sent as text (node-postgres/psql use text) |
| `LISTEN`/`NOTIFY` pub/sub            | no-op  | accepted, no delivery                  |

---

## 8. Consistency modes

GuardianDB is local-first; SQL does not change that. Two modes are defined.

### Local-first mode (default)
- A statement (and an explicit `BEGIN ... COMMIT`) is **atomic on the local
  replica**: it loads the touched tables, validates constraints and uniqueness,
  applies all writes in one batch, and only then flushes to storage. A failure
  flushes nothing.
- Replication remains **asynchronous**; peers converge under GuardianDB's
  CRDT/`iroh-docs` rules (last-writer-wins per key).
- This is **PostgreSQL-compatible API behaviour**, not globally serializable
  PostgreSQL storage behaviour. Two disconnected replicas can both insert the
  same primary key; on sync the documents converge by LWW and the relational
  layer surfaces the survivor. Cross-replica uniqueness is therefore *eventual*,
  not immediate.

### Strict SQL mode (`consistency: "strict"`)
- Intended to add stronger coordination where PostgreSQL semantics require it,
  via a **single-writer leader per database** (a transaction coordinator over
  GuardianDB/Iroh primitives). Writes route to the leader, giving a global
  serial order and immediate cross-replica uniqueness.
- Status: the API surface and routing flag exist; the leader/coordinator is a
  documented in-progress component — the accepted design is
  [RFC 0001: Fenced Shard Primaries](rfcs/0001-fenced-shard-primaries.md),
  which also adds table-group sharding and read/write replica routing. `SERIALIZABLE` isolation has an `#[ignore]`
  conformance test describing the target (one transaction aborts with `40001`
  on write-skew).

## 9. Transaction semantics

- `BEGIN` / `COMMIT` / `ROLLBACK` are supported. Within a transaction, writes
  buffer in an overlay; reads merge the overlay over storage; `COMMIT` flushes
  atomically; `ROLLBACK` discards.
- Isolation: **read committed** within a connection (a transaction sees its own
  uncommitted writes; other connections see committed state on their next
  statement). Autocommit wraps each statement in its own transaction.
- Constraint checks (NOT NULL, unique, CHECK) run before the write is staged, so
  a violating statement aborts without partial effects.
- Any error inside an explicit transaction **aborts** it: further statements
  fail with `25P02` until `ROLLBACK` (and `COMMIT` on an aborted block rolls
  back), matching PostgreSQL.

### Locking and concurrency

The single-node gateway is a single coordinator, so it implements a real
PostgreSQL-style lock manager (`src/sql/lock.rs`), shared across
all connections. Locks are held by a session and released at transaction end (or
session end for session-level advisory locks).

- **Table-level locks** — all eight modes (`ACCESS SHARE`, `ROW SHARE`,
  `ROW EXCLUSIVE`, `SHARE UPDATE EXCLUSIVE`, `SHARE`, `SHARE ROW EXCLUSIVE`,
  `EXCLUSIVE`, `ACCESS EXCLUSIVE`) with PostgreSQL's exact conflict matrix.
  Statements take them automatically (SELECT → `ACCESS SHARE`, INSERT/UPDATE/
  DELETE → `ROW EXCLUSIVE`, `CREATE INDEX` → `SHARE`, ALTER/DROP/TRUNCATE →
  `ACCESS EXCLUSIVE`). `LOCK TABLE ... IN <mode> MODE [NOWAIT]` is supported.
- **Row-level locks** — `SELECT ... FOR UPDATE` / `FOR SHARE` (the parser's
  granularity; `FOR NO KEY UPDATE`/`FOR KEY SHARE` map onto these), with
  `NOWAIT` and `SKIP LOCKED`. `UPDATE`/`DELETE` take `FOR UPDATE` row locks.
- **Advisory locks** — `pg_advisory_lock`/`unlock`, `pg_try_advisory_lock`, the
  `_xact_` (transaction-scoped) and `_shared` variants, single- and two-key
  forms, and `pg_advisory_unlock_all`.
- **Blocking & waiting** — a conflicting request blocks until release; `NOWAIT`
  fails immediately with `55P03`; `SKIP LOCKED` skips locked rows. `SET
  lock_timeout = '<n>[ms|s]'` bounds the wait (`55P03` on expiry).
- **Deadlock detection** — a wait-for-graph cycle aborts a victim with `40P01`.
- **Monitoring** — `pg_catalog.pg_locks` reports granted and waiting locks.

These are exercised by `tests/sql_locks.rs` (blocking, deadlock,
NOWAIT, SKIP LOCKED, advisory, LOCK TABLE, pg_locks, release-on-rollback).

> **Limitations.** Locking is per-node (the gateway is the coordinator); it does
> not span replicas — cross-replica serialization is the strict-mode work. There
> is no MVCC: isolation is read-committed, and an `UPDATE` that waits on a row
> lock does **not** re-read the row after acquiring it (no EvalPlanQual), so a
> blocked writer can still overwrite based on its original snapshot once it
> proceeds. `SERIALIZABLE` is not implemented.

## 10. Replication semantics

- Each table maps to a GuardianDB document collection; each row is a JSON
  document with a stable id (`__gdb_sql_rows_<oid>`), carrying internal fields
  `_id`, `__schema`, `__table`, `__version`, `__deleted`.
- The catalog is a single replicated document (`__gdb_sql_catalog`); schema
  changes (DDL) replicate like data.
- Convergence follows GuardianDB/`iroh-docs` semantics (range-based
  reconciliation, LWW per key). The relational layer reads a synchronous,
  locally-mirrored view (exactly like the existing DocumentStore index) and
  re-derives indexes from the live rows on each statement.
- The local view updates on local writes and on `load`/`sync`, not automatically
  when documents arrive from peers. `GuardianRelationalStorage::refresh()`
  re-syncs the index from replicated state; a gateway serving a replicating node
  should call it periodically or before reads to observe remote writes.
- Single-node SQL over the GuardianDB document store (including persistence
  across reopening the backend) is verified by `guardian_db::sql` tests. Making
  the *relational* view converge **across peers** additionally needs the two
  `open_sql` stores to share an iroh-docs namespace plus a background refresh;
  this is the in-progress distributed-coordination work, captured by the
  `#[ignore]`d `tests/sql_replication.rs` conformance target (raw document
  replication between peers already works — see `tests/integration_replication.rs`).

## 11. Index behaviour

- Indexes are real ordered (BTree) structures built from live rows and
  maintained incrementally within a statement/transaction.
- Unique indexes enforce uniqueness on the local replica (NULLs are distinct,
  matching PostgreSQL).
- The planner performs an **index scan** for `col = const` on a single
  single-column-indexed base table, otherwise a sequential scan. A conformance
  test asserts indexed lookups return the same rows as a full scan.
- `REINDEX` is implicit: indexes are rebuilt from storage on load.

## 12. Error codes

Errors carry standard PostgreSQL SQLSTATE codes, surfaced to clients in the
`code` field:

| SQLSTATE | Meaning                         |
| -------- | ------------------------------- |
| `42P01`  | undefined table                 |
| `42703`  | undefined column                |
| `42P07`  | duplicate table/index           |
| `42601`  | syntax error                    |
| `23505`  | unique violation                |
| `23502`  | not-null violation              |
| `23503`  | foreign-key violation           |
| `23514`  | check violation                 |
| `22P02`  | invalid text representation     |
| `22003`  | numeric value out of range      |
| `22012`  | division by zero                |
| `42804`  | datatype mismatch               |
| `3F000`  | undefined schema                |
| `40P01`  | deadlock detected               |
| `55P03`  | lock not available (NOWAIT / lock_timeout) |
| `25P02`  | in failed SQL transaction       |
| `0A000`  | feature not supported           |

## 13. Examples

- `examples/postgres-typeorm` — a complete TypeORM app (entities, migration,
  seed, queries, transactions). Run `npm run demo`.
- `tests/postgres-compat` — node-postgres and TypeORM conformance tests.
- `tests/pgwire_wire.rs` — a `tokio-postgres` client driving the
  gateway over TCP.

## 14. Testing summary

| Layer                | Tests                                                  |
| -------------------- | ------------------------------------------------------ |
| `src/relational`     | types, values, encoding, catalog, indexes, storage     |
| `src/sql`            | engine (DDL/DML/SELECT/joins/aggregates/txn/index) + conformance gaps |
| `src/pgwire`         | `tokio-postgres` over TCP (startup, query, errors, txn)|
| `tests/postgres-compat` | node-postgres + TypeORM (synchronize, migrations, relations, QueryBuilder, transactions) |
