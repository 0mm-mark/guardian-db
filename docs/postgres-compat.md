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
- `WITH` (CTEs), including `WITH RECURSIVE` (iterate-to-fixpoint, PostgreSQL
  working-table semantics; guarded — see §7)
- window functions: `row_number`, `rank`, `dense_rank`, `percent_rank`,
  `cume_dist`, `ntile`, `lag`/`lead`, `first_value`/`last_value`/`nth_value`,
  and every regular aggregate as a window aggregate; `PARTITION BY`,
  `ORDER BY`, named `WINDOW` clauses (incl. refinement), `ROWS`/`RANGE`
  frames (subset — see §7)
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
- `FOREIGN KEY` with `ON DELETE`/`ON UPDATE` actions — declared,
  introspectable **and enforced** (`MATCH SIMPLE`), see the "Foreign keys"
  subsection below
- real BTree indexes: primary-key, unique, secondary, composite; maintained on
  insert/update/delete; used for equality index scans (the planner chooses an
  index scan when a single base table is filtered by `col = const` on an indexed
  column, and falls back to a full scan otherwise — results are identical).

#### Foreign keys: enforced (`MATCH SIMPLE`)

Foreign-key constraints are parsed (column-level `REFERENCES` and table-level
`FOREIGN KEY`, including `ON DELETE` / `ON UPDATE` actions), stored in the
replicated catalog, fully introspectable, **and enforced at runtime** with
PostgreSQL's default `MATCH SIMPLE` semantics (`src/sql/fk.rs`):

- `INSERT` into the referencing table — and any `UPDATE` that changes an FK
  column's value — requires a parent row matching **all** referenced columns,
  else SQLSTATE `23503`
  (`insert or update on table "child" violates foreign key constraint "..."`,
  with a PG-style `Key (...)=(...) is not present` detail). If **any** FK
  column is NULL the constraint is satisfied (`MATCH SIMPLE`).
- `DELETE` of a referenced row, or an `UPDATE` that actually changes a
  referenced key column's value, executes the declared action:

  | action | behaviour |
  | ------ | --------- |
  | `NO ACTION` (default), `RESTRICT` | `23503` (`update or delete on table "parent" violates foreign key constraint "..." on table "child"`) if a referencing row remains |
  | `CASCADE` | deletes the referencing rows (or rewrites their FK columns to the new key), recursively applying *their* declared FK actions; self-referential chains terminate |
  | `SET NULL` | sets the FK columns to NULL (a NOT NULL column surfaces the usual `23502`) |
  | `SET DEFAULT` | sets the FK columns to their column defaults, then re-checks the constraint against the remaining parent rows (`23503` if the defaults reference nothing; all-NULL defaults satisfy `MATCH SIMPLE`) |

- `NO ACTION` and `RESTRICT` are both checked **per statement** — after the
  statement's own writes and cascades have applied — never deferred to
  commit. Accordingly, `DEFERRABLE` / `INITIALLY DEFERRED` constraints are
  rejected with `0A000` (`DEFERRABLE constraints are not supported`), as are
  `MATCH FULL` / `MATCH PARTIAL` (only `MATCH SIMPLE` is implemented) and
  `NOT ENFORCED`.
- Referential actions run through the normal write path (row locks, staged
  storage mutations), so a failing statement leaves nothing behind and
  `ROLLBACK` undoes cascades atomically. Like PostgreSQL — which runs
  referential actions with the table owner's privileges — the internal
  child-row reads and writes **bypass row-level security**.
- `TRUNCATE` of a referenced table is rejected (`0A000`, as in PostgreSQL)
  unless every referencing table is truncated in the same statement. A plain
  `DROP TABLE` of a referenced table fails with `2BP01` (dependent objects
  still exist); `DROP TABLE ... CASCADE` drops the dependent constraints
  instead. Because enforcement needs a resolvable parent, a foreign key
  referencing a missing table or column is rejected at DDL time
  (`42P01`/`42703`), and `REFERENCES parent` without a column list binds to
  the parent's primary key.

Like uniqueness (§8), this is **local-replica** enforcement: checks and
actions see the locally materialized state, and cross-replica convergence
follows the same eventual rules as all other writes.

Deliberate simplifications vs PostgreSQL, kept honest here: the referenced
columns are not required to carry a `UNIQUE`/`PRIMARY KEY` constraint at DDL
time (PostgreSQL's `42830`) — if duplicate parent keys exist, a key counts as
still-present while any duplicate survives; and there is no deferral of any
kind (see the `0A000` rejections above).

Introspection is unchanged: `pg_constraint` reports `contype = 'f'` with
`confupdtype`/`confdeltype` reflecting the declared actions
(`a`/`r`/`c`/`n`/`d`), and `information_schema.table_constraints`,
`key_column_usage`, `constraint_column_usage` and `referential_constraints`
(with `update_rule`/`delete_rule`) all show them. `tests/sql_conformance.rs`
pins the enforcement matrix (`foreign_keys_enforced_on_insert_and_child_update`,
`foreign_keys_restrict_and_no_action_block_parent_delete`,
`foreign_key_on_delete_*`, `foreign_key_on_update_actions`,
`foreign_key_cascade_is_atomic_and_rolls_back`,
`composite_foreign_key_match_simple`,
`referenced_parent_guarded_on_drop_and_truncate`).

### Catalog / introspection
Queryable `information_schema` (`tables`, `columns`, `schemata`,
`table_constraints`, `key_column_usage`, `constraint_column_usage`,
`referential_constraints`, `views`) and `pg_catalog` (`pg_class`,
`pg_attribute`, `pg_type`, `pg_namespace`, `pg_index`, `pg_constraint`,
`pg_database`, `pg_indexes`, `pg_attrdef`, `pg_am`, `pg_roles`, `pg_tables`
(with `rowsecurity`), `pg_policies`, and empty
`pg_description`/`pg_enum`/`pg_collation`/`pg_settings`). This is enough for
TypeORM schema sync, migrations and QueryRunner inspection, and for
node-postgres metadata.

### Row-Level Security

Row security is implemented with PostgreSQL semantics (`src/sql/rls.rs`).

**Syntax**

```sql
ALTER TABLE t ENABLE ROW LEVEL SECURITY;   -- and DISABLE
ALTER TABLE t FORCE ROW LEVEL SECURITY;    -- and NO FORCE
CREATE POLICY name ON t
  [ AS { PERMISSIVE | RESTRICTIVE } ]
  [ FOR { ALL | SELECT | INSERT | UPDATE | DELETE } ]
  [ TO role [, ...] ]                       -- omitted / PUBLIC = every role
  [ USING (expression) ]
  [ WITH CHECK (expression) ];
DROP POLICY [ IF EXISTS ] name ON t;
```

Policies live in the replicated catalog document, so they replicate (and
persist) like any other DDL. Expressions are stored as SQL text and validated
at `CREATE POLICY` time (unparseable expressions are rejected with `42601`);
PostgreSQL's clause rules are enforced (`USING` is not allowed `FOR INSERT`;
`WITH CHECK` is not allowed `FOR SELECT`/`FOR DELETE`). A duplicate policy
name on the same table is `42710`; dropping a missing policy is `42704`.

**Semantics** (matching PostgreSQL)

- A row is visible/allowed iff **any** applicable PERMISSIVE policy passes
  **and all** applicable RESTRICTIVE policies pass. Expressions evaluating to
  false **or NULL** deny.
- `rls_enabled` with **no** applicable policy is **default-deny**: `SELECT`
  returns nothing, `UPDATE`/`DELETE` affect nothing, `INSERT` fails.
- Per command: `SELECT` and `DELETE` filter with `USING`; `UPDATE` filters
  old rows with `USING` and checks new rows with `WITH CHECK` (falling back
  to `USING`); `INSERT` checks `WITH CHECK`. `FOR ALL` matches every command.
  `INSERT ... ON CONFLICT DO UPDATE` additionally requires the conflicting
  row to pass the UPDATE policies' `USING`.
- Enforcement happens where table rows become visible to execution, so
  joins, subqueries, CTEs, index scans and `SELECT ... FOR UPDATE` all
  inherit the filtering. A denied new row raises
  `new row violates row-level security policy for table "t"` (SQLSTATE
  `42501`).
- **Bypass and FORCE**: the role `service_role` (the `BYPASSRLS` equivalent)
  bypasses row security entirely; `postgres` and `guardian` (the engine's
  owner names) bypass it like PostgreSQL table owners — until a table
  declares `ALTER TABLE t FORCE ROW LEVEL SECURITY`, which subjects the owner
  roles to its policies too (`NO FORCE` restores the exemption, and
  `BYPASSRLS` beats `FORCE`, as in PostgreSQL). FORCE only matters while row
  security is enabled, and the flag is introspectable as
  `pg_class.relforcerowsecurity`. Tables with row security disabled are never
  filtered.

**Policy helpers.** Supabase's `auth.*` helpers are built in:

- `auth.uid()` — the caller's user id: the `sub` claim of
  `request.jwt.claims` (or the `request.jwt.claim.sub` variable), as `uuid`;
  `NULL` when unset.
- `auth.role()` — the `role` claim, as `text`.
- `auth.jwt()` — the whole claims document, as `jsonb`.

Claims are ordinary session variables: `SET request.jwt.claims = '{"sub":
"...", "role": "authenticated"}'` (or `set_config(...)`) makes them visible
to `current_setting('request.jwt.claims')` and the helpers above. The
Supabase gateway injects them automatically per request.

### User-defined functions (`CREATE FUNCTION`)

`CREATE [OR REPLACE] FUNCTION` / `DROP FUNCTION` are implemented for two
languages (see `src/sql/udf.rs`; behavioral tests in `tests/sql_functions.rs`).
Trigger functions (`RETURNS trigger`) are out of scope for now — `CREATE
TRIGGER` itself is not implemented — and are rejected at `CREATE FUNCTION`
time with `0A000`.

**`LANGUAGE SQL`**: the body is one or more `;`-separated plain SQL
statements (`SELECT`/`INSERT`/`UPDATE`/`DELETE`). Arguments bind both
positionally (`$1`, `$2`, ... exactly like a prepared statement — the same
mechanism `Exec::param` already uses for bound query parameters) and by the
declared parameter name, matching PostgreSQL (a SQL-language body may use
either spelling, or both). All statements run in order; the function's
result is the *last* statement's result (its first row's first column, or
`NULL` if it produced no rows, or if the last statement was a
`INSERT`/`UPDATE`/`DELETE` without rows to return) — this matches
PostgreSQL, including running the earlier statements purely for their side
effects.

**`LANGUAGE plpgsql`**: a deliberately small, explicitly-bounded subset —
enough for a non-trivial function or (later) trigger body, not full
PL/pgSQL:

| Supported | Rejected (typed `0A000`, naming the construct) |
| --- | --- |
| `DECLARE` locals with optional `:=`/`DEFAULT` defaults | `CURSOR` declarations → `"cursors"` |
| `:=` assignment | — |
| `IF ... THEN [ELSIF ... THEN ...] [ELSE ...] END IF` | — |
| `RETURN [expr]` | — |
| `RAISE [NOTICE\|WARNING\|EXCEPTION] 'msg'[, args]` | `RAISE ... USING` → `"RAISE ... USING"` |
| Plain SQL statements (`SELECT`/`INSERT`/`UPDATE`/`DELETE`) | any other statement kind (DDL, ...) inside a body → named after the statement (e.g. `"CREATE TABLE is not supported inside a function body"`) |
| `IN` parameters (bound by name) | `OUT`/`INOUT`/`VARIADIC` parameters → `"OUT parameters"` / `"INOUT parameters"` / `"VARIADIC parameters"` |
| — | `FOR`/`WHILE` loops, bare `LOOP` → `"FOR loop"` / `"WHILE loop"` / `"LOOP"` |
| — | `EXCEPTION` handler blocks → `"EXCEPTION handler"` |
| — | dynamic SQL (`EXECUTE`) → `"dynamic SQL (EXECUTE)"` |
| — | nested `BEGIN`/`DECLARE` blocks → `"nested block"` |
| — | `PERFORM`, cursor `OPEN`/`FETCH`/`CLOSE`, `GET DIAGNOSTICS` → named individually |
| — | `RETURNS TABLE` / `RETURNS SETOF` → `"RETURNS TABLE"` / `"RETURNS SETOF"` |

`STRICT` / `RETURNS NULL ON NULL INPUT` *is* honored (any `NULL` argument
short-circuits to a `NULL` result without invoking the body); `SECURITY
DEFINER`/`INVOKER` and `PARALLEL` are accepted and ignored (no privilege
separation or parallel execution model exists to apply them to).

**Argument binding**, matching PostgreSQL: `LANGUAGE SQL` bodies may use
positional `$1`/`$2`/... (via `Exec::param`, the same mechanism a prepared
statement's placeholders use) *or* the declared parameter name; PL/pgSQL
bodies reference the declared parameter name (and `DECLARE`d locals) by name
only — there is no `$n` in PL/pgSQL. By-name binding (both languages) is
implemented by substituting each variable's *current value* as a literal
directly into the statement/expression AST before it reaches the normal
evaluator — so a variable always wins over a same-named table column.
PostgreSQL's actual default (`plpgsql.variable_conflict`) is stricter
(errors on the ambiguity); this repo does not model per-session GUCs for
it, so unqualified shadowing is a deliberate, documented simplification.

**Deliberate divergence from PostgreSQL — DDL-time validation.**
PostgreSQL does not validate a PL/pgSQL function body's structure at
`CREATE FUNCTION` time (that requires the separate `plpgsql_check`
extension); a function with a typo or an unsupported construct is only
discovered when it is first *called*. This repo's truthfulness contract
requires every construct to either work or fail typed immediately, so
GuardianDB parses and validates the full body — including the fixed
unsupported-construct list above, and a static check that every control-flow
path ends in `RETURN` (PostgreSQL's own "control reached end of function
without RETURN", `42P13`, which real PostgreSQL only raises when it is
actually reached at runtime) — at `CREATE FUNCTION` time instead. A broken
function can therefore never silently exist in the catalog.

**Overload resolution** is name+arity only, not PostgreSQL's full
per-argument-type resolution: two `CREATE FUNCTION`s with the same name and
argument *count* are the same signature regardless of declared argument
*types* (a second one without `OR REPLACE` is `42723`; call dispatch — see
`funcs::call_scalar` — can only disambiguate by arity anyway). User-defined
functions are looked up *after* every builtin and extension function, so a
UDF can never shadow a core function.

**Recursion.** Direct self-recursion is supported, with a bounded call-depth
guard (25 calls) shared across an entire statement's UDF call chain
(including calls through other user-defined functions, not just literal
self-calls) — exceeding it is SQLSTATE `54001` (`statement_too_complex`),
the same code PostgreSQL's own stack-depth guard reports and this engine's
`WITH RECURSIVE` iteration cap already reuses. Unlike that iteration cap
(which bounds a `loop`, not stack depth), each nested UDF call recurses on
the real Rust call stack (re-parsing and re-evaluating the callee) through
several stack frames per level (parser, evaluator, statement executor) —
measured empirically against unoptimized (`dev` profile) builds, since debug
frames are much larger than release ones — so 25 is sized to stay well
under a worker thread's stack budget (e.g. Tokio's 2 MiB default worker
stack) rather than to mirror PostgreSQL's own
`max_stack_depth` default.

**`pg_proc`** reflects every created function: `proname`, `pronamespace`,
`prolang` (`sql`/`plpgsql`), `provolatile` (`i`/`s`/`v`), `proisstrict`,
`prorettype`, `pronargs`, `proargtypes` (space-separated type OIDs), `prosrc`
(the raw body text). `IMMUTABLE`/`STABLE`/`VOLATILE` are parsed, stored and
introspectable, but — like the rest of this engine — nothing plans or
caches differently based on volatility; it is truthfully reported, not
acted on.

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
| `cube`                | 1.5     | native  | `CUBE` type, `cube_*` functions, `@>`/`<@`/`&&`/`<->` operators  |
| `earthdistance`       | 1.2     | native  | `ll_to_earth`, `earth_distance`, `earth_box` (requires `cube`)   |
| `fuzzystrmatch`       | 1.2     | native  | `levenshtein`, `soundex`, `difference`, `metaphone`, `dmetaphone`|
| `hstore`              | 1.8     | native  | `HSTORE` type, `->`/`\|\|`/`?`/`-`/`@>`/`<@` operators, functions|
| `intarray`            | 1.5     | native  | int[] functions + `&&`/`@>`/`<@`/`+`/`-`/`\|`/`&`/`#` operators  |
| `ltree`               | 1.3     | native  | `LTREE` type, lquery `~` matching, `@>`/`<@`, path functions     |
| `pg_stat_statements`  | 1.10    | sidecar | statement planning/execution statistics                          |
| `pg_trgm`             | 1.6     | native  | `similarity`, `%`/`<->` operators, `pg_trgm.similarity_threshold`|
| `pgcrypto`            | 1.3     | native  | `digest`, `hmac`, `crypt`/`gen_salt`, `encode`/`decode`, ...     |
| `plpgsql`             | 1.0     | native  | pre-installed shim (function bodies are not executable)          |
| `postgis`             | 3.4.2   | sidecar | spatial types and functions                                      |
| `timescaledb`         | 2.15.2  | sidecar | time-series hypertables and queries                              |
| `unaccent`            | 1.1     | native  | `unaccent()` accent stripping                                    |
| `uuid-ossp`           | 1.1     | native  | `uuid_generate_v1/v3/v4/v5`, namespace constants                 |
| `vector`              | 0.8.1   | native  | pgvector: `VECTOR(n)` type, `<->`/`<#>`/`<=>`/`<+>`, distances   |

The tier-2 contrib set (`hstore`, `intarray`, `ltree`, `cube`,
`earthdistance`) is implemented natively with semantics verified against live
PostgreSQL 16.13 contrib output (text formats, key ordering, operator truth
tables, function edge cases — the exact vectors live in the modules' unit
tests). Notes and deliberate deviations:

* **hstore** — full text syntax (quoting, escapes, `NULL` values, first
  duplicate key wins) and contrib's internal (length, bytes) key order in
  output and `akeys`/`avals`. Set-returning members (`each`, setof
  `skeys`/`svals`) need SRF machinery the engine lacks; `akeys`/`avals`
  return the same data as arrays. `?&`/`?|` and the record functions are not
  implemented. `hstore_to_json` output is compact JSON (PostgreSQL prints
  `{"a": "1"}` with spaces; the content is identical).
* **intarray** — no new types; functions reject non-integer arrays with
  `42846` and NULL elements with a typed error (PostgreSQL uses `22004`;
  GuardianDB reports `22023`). All binary operators route (`&&`, `@>`, `<@`,
  `+`, `-`, `|`, `&`, `#`); the `query_int` type with `@@`/`~~` is not
  implemented, and the *prefix* `#` count operator is function-only
  (`icount`).
* **ltree** — labels of alphanumerics/`_`/`-` (hyphens per PostgreSQL 16),
  the empty zero-level path, label-wise ordering (also in index keys). `~`
  implements the documented lquery language: `@`/`*`/`%` modifiers, `|`
  alternation, `!` negation, and `{n}`/`{n,}`/`{,m}`/`{n,m}` quantifiers on
  star and non-star items. The `lquery`/`ltxtquery` named types are not
  registered — write patterns as plain string literals (`path ~ 'a.*'`), not
  `::lquery` casts; `@@` full-text matching and the ltree[] array operators
  are not implemented.
* **cube** — exact contrib text forms (corner order preserved, coincident
  corners print as points), normalized accessors and predicates, inverted
  `cube_inter` results for disjoint inputs (like PostgreSQL). The
  `cube(cube, ...)` dimension-appending constructors, `~>` coordinate
  extraction, taxicab/chebyshev distance operators and the zero-dimensional
  `'()'` cube are not implemented.
* **earthdistance** — declares `requires: cube`, so plain `CREATE EXTENSION
  earthdistance` fails `0A000` naming the requirement and `... CASCADE`
  installs both. The `earth` domain is not enforced (any cube is accepted);
  the point-based `<@>` operator is out (no `point` type).

sqlparser 0.62 requires the `WITH` noise word before `CREATE EXTENSION`
options; GuardianDB re-normalizes on the parse-error path so PostgreSQL's
`CREATE EXTENSION earthdistance CASCADE` spelling works unchanged.

**Explicitly not available** (fail `CREATE EXTENSION` with a typed `0A000`):

| Missing     | Reason                                                         |
| ----------- | -------------------------------------------------------------- |
| `isn`       | check-digit type family (ISBN/ISSN/EAN13/UPC) pending           |
| `lo`        | no large-object (`pg_largeobject`) infrastructure               |
| `tablefunc` | `crosstab` requires set-returning-function-over-query machinery |

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

### The PostgreSQL sidecar runtime

Extensions that cannot be reimplemented in the engine (C code, planner hooks,
background workers — PostGIS, TimescaleDB, pg_stat_statements) are delegated
to a **managed PostgreSQL sidecar**: a real PostgreSQL process the operator
runs next to GuardianDB. GuardianDB ships its own minimal wire-protocol
*client* (`src/sql/ext/sidecar.rs`) — plaintext protocol 3.0, trust or
cleartext-password auth, simple query protocol, text results — and each
session lazily pins one sidecar connection (closed when the session ends).

**Configuration** — two channels, session GUC first, environment second:

```sql
SET guardian.sidecar_dsn = 'postgres://user:pass@host:5432/db?sslmode=disable';
```

```bash
export GUARDIAN_PG_SIDECAR_DSN='postgres://user:pass@host:5432/db?sslmode=disable'
```

The DSN is a standard `postgres://` URI (`%XX` escapes decoded; the database
defaults to the user, like libpq). Because the client is plaintext-only,
`sslmode` must be absent or `disable` — anything else is rejected with
`0A000`. Other URI parameters are accepted and ignored. `SET
guardian.sidecar_dsn = ''` disables routing for the session.

**Routing rules**

1. `CREATE EXTENSION` of a sidecar-strategy extension: with no DSN configured
   it fails `0A000` naming both configuration channels; with a DSN it is
   forwarded **verbatim** to the sidecar, and on success the install is
   recorded in the (replicated) local catalog with the version the sidecar
   reports (`SELECT extversion FROM pg_extension ...`), marked as
   sidecar-bound.
2. A statement that fails locally with **undefined function (`42883`),
   undefined type, or undefined relation (`42P01`)** while a DSN is
   configured is forwarded verbatim and the sidecar's result (or its
   SQLSTATE-tagged error) is returned. This is what makes
   `SELECT ST_AsText(...)` or `SELECT * FROM pg_stat_statements` work: the
   objects only exist on the sidecar. Statements with bound (`$n`) parameters
   are not forwarded — the extended protocol keeps the local error.
3. `DROP EXTENSION` of a sidecar-bound extension forwards the drop to the
   sidecar, then removes the local record. Without a configured DSN it fails
   `0A000` with the configuration hint.

**Transaction limitation.** The sidecar cannot join a local GuardianDB
transaction, so sidecar routing is **autocommit-only**: inside an explicit
`BEGIN ... COMMIT` block, fallback-forwarding is disabled — the local error is
kept (same SQLSTATE) with a hint appended — and sidecar `CREATE`/`DROP
EXTENSION` are refused with `0A000`.

Sidecar errors arrive as ordinary SQLSTATE-tagged errors: the sidecar's code
and message are preserved verbatim, so clients cannot tell them apart from
local errors. The wire-level conformance tests (`tests/sql_extensions.rs`)
drive a second GuardianDB pgwire server as the mock sidecar, and an
`#[ignore]`d `sidecar_real_postgres` test runs the full flow — `initdb`, real
`CREATE EXTENSION pg_stat_statements`, stats query over the wire — against a
local PostgreSQL 16 (`cargo test --features sql --test sql_extensions --
--ignored`).

---

## 7. Unsupported SQL (documented gaps)

Each gap has a conformance test in `tests/sql_conformance.rs`
(clean-failure tests pass; intended-future features are `#[ignore]`d).

| Feature                              | Status | Behaviour                              |
| ------------------------------------ | ------ | -------------------------------------- |
| Window functions (`OVER`)            | ✓      | subset: ranking funcs, `lag`/`lead`, `first_value`/`last_value`/`nth_value`, all regular aggregates as window aggregates (with `FILTER`); `PARTITION BY`/`ORDER BY`/named `WINDOW` (incl. refinement); `ROWS` frames with offsets, `RANGE` frames with `UNBOUNDED`/`CURRENT ROW` bounds (default frame includes peers, like PostgreSQL). Out-of-subset → typed errors: `RANGE` with offset, `GROUPS` mode, `DISTINCT`/`WITHIN GROUP`/`IGNORE NULLS` in a window call → `0A000`; misplaced `OVER` (`WHERE`/`GROUP BY`/`HAVING`), nested window calls, invalid frames → `42P20`; `OVER` on a non-window function → `42809` |
| `WITH RECURSIVE`                     | ✓      | iterate-to-fixpoint with PostgreSQL working-table semantics (`UNION` dedups against the full accumulation; the recursive term sees only the previous iteration's rows); column types fixed by the base term (recursive rows coerced or error). Guards: iteration cap (default 100 000, session-settable via `guardian.recursive_max_iterations`) and 10M-row cap → `54001` instead of hanging; invalid self-reference shapes (more than once, in a subquery, in the non-recursive term, in an outer join, aggregated) → `42P19`; `ORDER BY`/`LIMIT`/`GROUP BY`/`DISTINCT` in the recursive query and mutual recursion → `0A000` |
| Set-returning funcs in `FROM`        | ✗      | error `0A000` (scalar table funcs ok)  |
| `WITH` inside a subquery             | ✗      | error `0A000` (top-level `WITH` ok)    |
| `COPY` (bulk load)                   | ✗      | error `0A000` (no CopyIn/Out framing)  |
| Materialized views                   | ✗      | error `0A000`                          |
| `CREATE FUNCTION` (`LANGUAGE SQL` / `plpgsql` subset) | ✓ | see §5 "User-defined functions" for the exact PL/pgSQL subset and its typed `0A000` exclusions |
| `CREATE PROCEDURE` / `CREATE TRIGGER` / `DROP TRIGGER` | ✗ | error `0A000` for **every** spelling — detected by keyword prefix before the parser, since sqlparser 0.62 parses only some forms (which would otherwise leak `42601`); `CREATE FUNCTION ... RETURNS trigger` is rejected the same way |
| Full-text search                     | ✗      | error `0A000` — the function family (`to_tsvector`, `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery`, `websearch_to_tsquery`, `ts_rank`, `ts_rank_cd`, `ts_headline`, `setweight`, `ts_delete`, `tsvector_to_array`) is *named*-unsupported (never `42883`, so it is never sidecar-routed), and `@@` is an unsupported operator; the `tsvector`/`tsquery` types are `42704` |
| Generated/computed columns           | ✗      | ignored test                           |
| `SAVEPOINT` partial rollback         | partial| `SAVEPOINT`/`RELEASE` no-op; `ROLLBACK TO` collapses to full rollback |
| `DEFERRABLE` constraints             | ✗      | error `0A000` — foreign keys are enforced immediately, per statement (see "Foreign keys" in §5); `MATCH FULL`/`MATCH PARTIAL` are `0A000` too |
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
- Constraint checks (NOT NULL, unique, CHECK, foreign keys — including
  referential actions) run before the statement's writes are staged, so a
  violating statement aborts without partial effects.
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
| `23503`  | foreign-key violation (see §5 "Foreign keys")   |
| `23514`  | check violation                 |
| `22P02`  | invalid text representation     |
| `22003`  | numeric value out of range      |
| `22012`  | division by zero                |
| `42804`  | datatype mismatch               |
| `3F000`  | undefined schema                |
| `40P01`  | deadlock detected               |
| `55P03`  | lock not available (NOWAIT / lock_timeout) |
| `25P02`  | in failed SQL transaction       |
| `42501`  | insufficient privilege (row-level security) |
| `42710`  | duplicate object (policy, extension) |
| `42704`  | undefined object (policy, extension) |
| `2BP01`  | dependent objects still exist (DROP of an FK-referenced table) |
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

### SQL compatibility note 15

Tracks PostgreSQL-compatible behavior for window functions, recursive CTEs, SQLSTATE-mapped validation, aggregate FILTER handling, and min/max type inference without changing executable code.

### SQL compatibility note 15

Tracks PostgreSQL-compatible behavior for window functions, recursive CTEs, SQLSTATE-mapped validation, aggregate FILTER handling, and min/max type inference without changing executable code.
