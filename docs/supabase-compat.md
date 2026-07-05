# Supabase-compatible layer for GuardianDB

A Kong-shaped HTTP gateway that lets Supabase client libraries (`supabase-js`,
PostgREST clients, GoTrue clients) talk to a GuardianDB node with no
GuardianDB-specific code. It lives entirely behind the `supabase` Cargo feature
(`supabase = ["sql", "dep:axum", "dep:tower"]`); default builds are unaffected.

This document describes the **foundation slice**: **REST** (PostgREST-compatible)
and **Auth** (GoTrue-compatible) are implemented end-to-end. Every other Kong
service returns a typed `501` — never a bare 404 and never fake success.

---

## 1. Scouted seams (what the layer is built on)

Everything is built on the SQL engine's *public* surface. No file under
`src/sql/**` or `src/relational/**` was modified (this keeps the layer
conflict-free with concurrent work on `src/sql/ext/`).

| Seam | Where | How the gateway uses it |
| --- | --- | --- |
| `Database<S: RelationalStorage>` | `src/sql/engine.rs` | Built once with `Database::new(Arc<S>, name)`; shared via `Arc` in `AppState`. |
| `Session<S>` | `src/sql/engine.rs` | **One session per HTTP request**, `Session::new(db, role)`. The `role` (Postgres role name) drives row-security enforcement; `Session::set_var("request.jwt.claims", ...)` injects the caller's claims for policies. |
| `Session::prepare` + `execute_one(&stmt, &[SqlValue])` | `src/sql/engine.rs` | The injection-safe path: a single parameterised statement with `$1..$n` binds. Used for all REST/Auth data ops. |
| `Session::execute(sql)` | `src/sql/engine.rs` | Multi-statement string (no params). Used only for the `auth` schema bootstrap DDL. |
| `ExecResult::{Rows{fields,rows}, Command{tag}}` | `src/sql/result.rs` | `OutField.name` + `SqlType` drive JSON rendering. |
| `SqlValue` / `SqlType` + `from_text` / `decode_json` | `src/relational/value.rs` | REST coerces request strings/JSON to the **declared column type** read from the catalog. |
| `Catalog` / `Table` / `Column` | `src/relational/catalog.rs` | Loaded via `db.storage().load_catalog()` to look up column types and primary keys (for upsert). |
| `RelError::sqlstate()` | `src/relational/error.rs` | Engine errors are surfaced in PostgREST shape with the real SQLSTATE. |
| `bcrypt` | already in-tree (pgcrypto) | Password hash/verify. |
| `hmac` + `sha2` + `base64` | already in-tree (`sql` feature) | HS256 JWTs implemented from scratch — **no `jsonwebtoken` dependency added**. |
| `MemoryStorage` / `open_sql` | `src/relational/storage.rs`, `src/sql/guardian_storage.rs` | In-memory (dev/tests) and persistent Iroh-replicated backends for the binary. |

Verified engine capabilities the layer relies on: `RETURNING`, `ON CONFLICT DO
UPDATE/NOTHING`, `DEFAULT` in `VALUES`, `ILIKE`, `ORDER BY ... NULLS FIRST/LAST`,
`count(*)`, schema-qualified DDL (`CREATE SCHEMA auth; CREATE TABLE auth.users
(...)`), `uuid`/`timestamptz`/`jsonb`/`boolean` columns.

---

## 2. Architecture

```
HTTP request
  │
  ├─ request_id middleware      (x-request-id: read or generate; echoed on response)
  │
  ├─ apikey middleware          (rest + auth only)
  │     • verify `apikey` (a JWT signed by the project secret) → api_key_role
  │     • verify optional `Authorization: Bearer` → claims (fail closed on bad JWT)
  │     • effective role = bearer.role ?? api_key_role  (PostgREST semantics)
  │     • attach AuthContext{ role, api_key_role, claims, request_id }
  │
  ├─ /rest/v1/*   → rest.rs   → Session(role)          → SQL → PostgREST JSON
  ├─ /auth/v1/*   → auth.rs   → Session(service_role)  → auth.* tables → GoTrue JSON
  └─ /storage | /realtime | /functions | /graphql | /pg-meta → 501 typed
```

Files (all under `src/supabase/`, behind `#[cfg(feature = "supabase")]`):

- `project.rs` — `SupabaseCompatProject`, `ServiceConfig`, `ProjectKeys`, `Secret`.
- `jwt.rs` — HS256 sign/verify + `Claims`.
- `error.rs` — `SupaError` taxonomy + PostgREST/GoTrue error rendering.
- `gateway.rs` — axum `Router`, middleware, `AuthContext`, shared exec helpers.
- `rest.rs` — PostgREST translation and handlers.
- `auth.rs` — GoTrue schema bootstrap and handlers.
- `src/bin/guardian-supabase.rs` — the binary.

A single project is served per gateway instance (the "single-project shell"),
configured from CLI flags / a supplied JWT secret.

---

## 3. Implemented routes

### REST (`/rest/v1`)

| Method | Path | Behaviour |
| --- | --- | --- |
| `GET`/`HEAD` | `/rest/v1/{table}` | SELECT with `select=`, filters, `order=`, `limit`/`offset`, `Range`. |
| `POST` | `/rest/v1/{table}` | INSERT (object or array), upsert via `Prefer: resolution=…` + `on_conflict=`. |
| `PATCH` | `/rest/v1/{table}` | UPDATE with filters. |
| `DELETE` | `/rest/v1/{table}` | DELETE with filters. |
| `GET`/`POST` | `/rest/v1/rpc/{fn}` | `SELECT fn(args)` — see RPC note below. |

### Auth (`/auth/v1`)

| Method | Path | Behaviour |
| --- | --- | --- |
| `POST` | `/auth/v1/signup` | bcrypt the password, insert user (auto-confirmed), issue access+refresh tokens. |
| `POST` | `/auth/v1/token?grant_type=password` | verify bcrypt, issue tokens. |
| `POST` | `/auth/v1/token?grant_type=refresh_token` | rotate refresh token, issue new access token. |
| `POST` | `/auth/v1/logout` | revoke the user's refresh tokens. |
| `GET` | `/auth/v1/user` | the user for the `Authorization: Bearer` access token. |
| `PUT` | `/auth/v1/user` | update email / password / metadata. |
| `GET`/`POST` | `/auth/v1/admin/users` | list / create (service_role only). |
| `GET`/`PUT`/`DELETE` | `/auth/v1/admin/users/{id}` | get / update / delete (service_role only). |

### Not implemented in this slice → typed `501`

`/realtime/v1/*`, `/storage/v1/*`, `/functions/v1/*`, `/graphql/v1`,
`/pg-meta/*`, `/platform/pg-meta/*` each return
`{"code":"SUPA_COMPAT_<SERVICE>_NOT_IMPLEMENTED","message":"…","hint":"tracked for a later slice"}`
with HTTP `501`. `/health` returns `200 {"status":"ok"}`.

---

## 4. Auth, JWT, and Row-Level Security

**JWT.** HS256, implemented from scratch (`jwt.rs`) on `hmac`+`sha2`+`base64`.
Signatures are compared in constant time; `exp` is enforced. `Claims` carries
`{iss, role, sub, email, aud, iat, exp, session_id, extra}`; unknown claims
round-trip through `extra`.

**API keys.** `ProjectKeys::from_secret(secret, iat)` deterministically derives
the `anon` and `service_role` keys as real HS256 JWTs with the Supabase claim
shape (`{role, iss:"supabase", iat, exp}`, 10-year expiry). The generator is
**pure** — the secret and `iat` are injected, never sourced from `now()`/`rand`
inside the constructor — so tests are deterministic. The random secret generator
(`generate_jwt_secret`, ≥ 48 chars) and project-ref generator are the only impure
helpers, used by the binary.

**Secrets.** `Secret` and `ProjectKeys` redact themselves in `Debug`; no secret is
ever written to an error body or log.

**Passwords.** bcrypt (cost 10) via the in-tree `bcrypt` crate.

**Sessions & refresh tokens.** A sign-in creates an `auth.sessions` row and an
opaque `auth.refresh_tokens` row. Refresh **rotates**: the presented token is
marked `revoked`, a new token is minted on the same session (with `parent`
set), and a fresh access token is issued.

**Role resolution.** The effective Postgres role is the role claim of the
`Authorization: Bearer` token if present, else the `apikey`'s role (PostgREST
semantics). Each request opens `Session::new(db, role)`.

**Row-Level Security — enforced.** The Supabase security model is real:

- Every REST data path runs through `run_sql_as` (`gateway.rs`), which binds
  the per-request session to the resolved role **and injects the verified JWT
  claims** as the `request.jwt.claims` session variable
  (`Session::set_var`, no SQL round-trip). When no bearer token was supplied,
  a minimal `{"role": "<role>"}` document is synthesized so `auth.role()`
  still reflects the effective role.
- The engine evaluates policies per row (see `docs/postgres-compat.md`,
  "Row-Level Security"): tables with `ALTER TABLE ... ENABLE ROW LEVEL
  SECURITY` are filtered for `anon` / `authenticated` / custom roles by their
  `CREATE POLICY` rules — permissive policies OR together, restrictive
  policies AND, no applicable policy means **default deny**.
- `service_role` **bypasses** row security exactly like Supabase's service
  key (as do the engine-owner roles `postgres`/`guardian`).
- Policies use the standard Supabase helpers: `auth.uid()` (the `sub` claim
  as `uuid`), `auth.role()`, `auth.jwt()`, and
  `current_setting('request.jwt.claims')`.
- A write denied by `WITH CHECK` surfaces as a PostgREST-shaped error with
  SQLSTATE `42501` and HTTP **403** (`new row violates row-level security
  policy for table "t"`); rows filtered by `USING` simply do not appear
  (reads return fewer rows; `UPDATE`/`DELETE` affect fewer rows) — never an
  error, matching PostgreSQL/PostgREST.
- Internal work (GoTrue's `auth.*` tables, schema bootstrap) runs as
  `service_role` via `run_sql` and is unaffected.

**Auth schema bootstrap.** On first auth request, `BOOTSTRAP_SQL` runs through a
`Session` (idempotent `CREATE SCHEMA/TABLE IF NOT EXISTS`), creating
`auth.users`, `auth.refresh_tokens`, `auth.sessions`, `auth.identities`,
`auth.audit_log_entries`, `auth.instances`, `auth.schema_migrations`. The full
integration suite exercises this, proving every statement executes on the engine.
Divergences from stock Supabase, all driven by engine support:
- `auth.refresh_tokens` is keyed by its opaque `token` (stock uses a `bigserial
  id`); the engine supports a `text` primary key cleanly.
- GoTrue's generated/identity columns, partial indexes, and `CHECK`-heavy columns
  are omitted; timestamps and metadata are generated in Rust and inserted
  explicitly (no reliance on DB-side `DEFAULT` functions).

---

## 5. REST behaviour

**Query translation** (`rest.rs`). PostgREST query params become parameterised
SQL. All literal values are bound as `$n`; identifiers (table, column, function,
schema) are validated against `^[A-Za-z_][A-Za-z0-9_]*$` — the only defense for
identifiers, which cannot be bound. Filter/body values are coerced to the
**declared column type** (read from the catalog) so numeric/temporal comparisons
are typed, not lexical; unknown columns fall back to value inference.

| PostgREST | SQL |
| --- | --- |
| `select=id,full_name:name` | `SELECT "id", "name" AS "full_name"` |
| `id=eq.1` | `"id" = $1` (bound `1`) |
| `name=ilike.*ali*` | `"name" ILIKE $1` (bound `%ali%`) |
| `id=in.(1,2,3)` | `"id" IN ($1,$2,$3)` |
| `deleted_at=is.null` | `"deleted_at" IS NULL` |
| `active=not.is.false` | `NOT ("active" IS FALSE)` |
| `order=created_at.desc.nullslast` | `ORDER BY "created_at" DESC NULLS LAST` |
| `limit=10&offset=20` / `Range: 20-29` | `LIMIT 10 OFFSET 20` |

Supported operators: `eq, neq, gt, gte, lt, lte, like, ilike, is, in`, each
optionally negated with the `not.` prefix. **Any other operator** (e.g. `cs`,
`cd`, `fts`) or a logical tree (`and=`, `or=`) is rejected with
`SUPA_COMPAT_REST_UNSUPPORTED_FILTER` (`400`) — never silently ignored.

**Writes.** `POST` accepts a single object or an array; columns are the union of
keys (missing cells become `DEFAULT`). Upsert: `Prefer: resolution=merge-duplicates`
+ `on_conflict=col,…` → `ON CONFLICT (…) DO UPDATE SET col = EXCLUDED.col`
(falls back to the primary key when `on_conflict` is absent);
`resolution=ignore-duplicates` → `DO NOTHING`. `PATCH`/`DELETE` apply the query
filters as the `WHERE`.

**Responses.** Rows render as a JSON array of objects (timestamps as ISO-8601
with a `T` separator). `Accept: application/vnd.pgrst.object+json` returns a
single object (`400` if not exactly one row). `Prefer: return=representation`
echoes affected rows (default `minimal` → `201`/`204` with no body).
`Prefer: count=exact` runs a `count(*)` and returns `206` with a
`Content-Range: start-end/total` header; otherwise `Content-Range: start-end/*`.

**RPC.** `POST /rest/v1/rpc/{fn}` builds `SELECT fn($1, …)` from the JSON body's
values and returns the rows as objects. **Known divergence:** because JSON object
keys are unordered, named arguments are passed **positionally in sorted key
order** rather than by true named binding. Functions must exist in the engine
(built-ins / extension functions); calling an unknown function returns the
engine's `42883` in PostgREST shape.

**Errors.** Engine errors render in PostgREST shape
`{"code": <SQLSTATE>, "message", "details", "hint"}` with the HTTP status derived
from the SQLSTATE class (e.g. `42P01` → `404`, `23505` → `409`, `22P02`/`42601` →
`400`).

---

## 6. Typed-error taxonomy

`SupaError` (`error.rs`) — every gateway-level failure is typed; no secret ever
appears in a body or log.

| Variant | HTTP | `code` |
| --- | --- | --- |
| `MissingApiKey` | 401 | `SUPA_COMPAT_MISSING_API_KEY` |
| `InvalidApiKey` | 401 | `SUPA_COMPAT_INVALID_API_KEY` |
| `InvalidJwt` | 401 | `SUPA_COMPAT_INVALID_JWT` |
| `Forbidden` | 403 | `SUPA_COMPAT_FORBIDDEN` |
| `NotImplemented(SERVICE)` | 501 | `SUPA_COMPAT_<SERVICE>_NOT_IMPLEMENTED` |
| `UnsupportedFilter` | 400 | `SUPA_COMPAT_REST_UNSUPPORTED_FILTER` |
| `BadRequest` | 400 | `SUPA_COMPAT_REST_BAD_REQUEST` |
| `AuthProviderUnsupported` | 400 | `SUPA_COMPAT_AUTH_PROVIDER_UNSUPPORTED` |
| `Sql(err)` | per SQLSTATE | the SQLSTATE (PostgREST shape) |
| `Internal` | 500 | `SUPA_COMPAT_INTERNAL` |

GoTrue endpoints additionally use GoTrue's own shapes:
`{"code","error_code","msg"}` for most endpoints and
`{"error","error_description"}` for the token endpoint (e.g. `invalid_grant` on a
bad password/refresh token). OAuth/SSO grants (`id_token`, `authorization_code`,
`pkce`, `web3`, `implicit`) return `AuthProviderUnsupported` — never fake success.

---

## 7. Deferred to later slices

- **Storage, Realtime, Edge Functions, GraphQL, pg-meta / Studio.** Routed and
  returning typed `501`; not implemented.
- **REST**: embedded resources / joins (`select=author(name)`), the full operator
  set (`cs`, `cd`, `ov`, `fts`, `wfts`, …), logical `and`/`or` trees, computed
  columns, true named-argument RPC binding, vertical filtering on embeds.
- **Auth**: email/SMS delivery and confirmation flows, OAuth/SSO providers, MFA,
  anonymous sign-in, password recovery, per-session logout scopes.
- **Multi-project / multi-tenant** routing (the shell serves one project).

---

## 8. Running it

### Start the gateway

```bash
# In-memory (development); prints SUPABASE_URL, ANON_KEY, SERVICE_ROLE_KEY, JWT_SECRET
cargo run --features supabase --bin guardian-supabase

# Options
cargo run --features supabase --bin guardian-supabase -- \
  --addr 127.0.0.1:54321 \
  --database app \
  --jwt-secret "your-fixed-secret-if-you-want-stable-keys" \
  --path ./guardian_supabase_data   # persistent, Iroh-replicated node
```

Startup prints, e.g.:

```
  SUPABASE_URL      : http://127.0.0.1:54321
  ANON_KEY          : eyJhbGciOiJIUzI1NiIs...
  SERVICE_ROLE_KEY  : eyJhbGciOiJIUzI1NiIs...
  JWT_SECRET        : <generated>   (save this to reuse the keys)
```

### Point supabase-js at it

```ts
import { createClient } from "@supabase/supabase-js";

const supabase = createClient("http://127.0.0.1:54321", ANON_KEY);

// REST
const { data } = await supabase.from("todos").select("*").eq("done", false);
await supabase.from("todos").insert({ id: 1, title: "buy milk", done: false });

// Auth
await supabase.auth.signUp({ email: "a@b.com", password: "hunter2pass" });
const { data: session } = await supabase.auth.signInWithPassword({
  email: "a@b.com",
  password: "hunter2pass",
});
```

Tables must be created first (over `psql`/pgwire, or a direct `Session`); REST
does not create tables. The `auth` schema is bootstrapped automatically on the
first auth request.

### curl smoke test

```bash
ANON=... # from startup
curl -s "http://127.0.0.1:54321/rest/v1/todos?select=*" -H "apikey: $ANON"
curl -s -X POST "http://127.0.0.1:54321/auth/v1/signup" \
  -H "apikey: $ANON" -H 'content-type: application/json' \
  -d '{"email":"a@b.com","password":"hunter2pass"}'
```

### Tests

```bash
cargo test --features supabase                       # everything
cargo test --features supabase --test supabase_gateway   # in-process gateway tests
cargo test --features supabase --lib supabase::          # unit tests
```

The integration tests drive the axum `Router` in-process with
`tower::ServiceExt::oneshot` over a `MemoryStorage`-backed `Database` — no real
ports are bound.
