//! The database engine: sessions, transactions, statement dispatch, and the
//! load → execute → commit lifecycle.

use crate::relational::catalog::QualifiedName;
use crate::relational::{Catalog, RelationalStorage, SqlValue};
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::Exec;
use crate::sql::lock::{LockManager, LockMode, LockObject, LockScope, SessionId, WaitPolicy};
use crate::sql::result::ExecResult;
use crate::sql::store::{LoadedTable, Mutation};
use serde_json::Value as Json;
use sqlparser::ast::{Query, Statement};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// A shared, storage-backed relational database.
pub struct Database<S: RelationalStorage> {
    storage: Arc<S>,
    pub name: String,
    locks: Arc<LockManager>,
    /// Registered row-change listeners (see [`Database::subscribe_changes`]).
    /// Closed receivers are pruned lazily on the next emission.
    change_listeners: std::sync::RwLock<Vec<tokio::sync::mpsc::UnboundedSender<ChangeEvent>>>,
}

impl<S: RelationalStorage> Database<S> {
    pub fn new(storage: Arc<S>, name: impl Into<String>) -> Self {
        Self {
            storage,
            name: name.into(),
            locks: Arc::new(LockManager::new()),
            change_listeners: std::sync::RwLock::new(Vec::new()),
        }
    }

    pub fn storage(&self) -> &Arc<S> {
        &self.storage
    }

    /// The shared lock manager (single-node coordinator).
    pub fn locks(&self) -> &Arc<LockManager> {
        &self.locks
    }

    /// Subscribe to committed row changes. Every row mutation that reaches
    /// storage through a [`Session`] — autocommit statements and explicit
    /// `COMMIT`s alike — is delivered as a [`ChangeEvent`] *after* it has been
    /// applied. Dropping the receiver unsubscribes (the sender is pruned on the
    /// next emission). When no listener is registered the engine skips event
    /// collection entirely, so the hook costs nothing unless used.
    ///
    /// `TRUNCATE` produces no per-row events, and writes that bypass the
    /// engine (direct [`RelationalStorage`] calls, remote replication) are not
    /// observed — this is a local-commit hook, not a replication changefeed.
    pub fn subscribe_changes(&self) -> tokio::sync::mpsc::UnboundedReceiver<ChangeEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.change_listeners.write().unwrap().push(tx);
        rx
    }

    /// Is at least one change listener registered?
    fn has_change_listeners(&self) -> bool {
        !self.change_listeners.read().unwrap().is_empty()
    }

    /// Deliver `events` to every registered listener, pruning closed ones.
    fn emit_changes(&self, events: Vec<ChangeEvent>) {
        if events.is_empty() {
            return;
        }
        let mut listeners = self.change_listeners.write().unwrap();
        if listeners.is_empty() {
            return;
        }
        listeners.retain(|tx| events.iter().all(|e| tx.send(e.clone()).is_ok()));
    }
}

/// A committed row change, delivered to [`Database::subscribe_changes`]
/// receivers after the write reached storage. `old`/`new` carry the stored row
/// documents (the engine's JSON row encoding, including the `__schema` /
/// `__table` metadata fields); consumers decode column values with the catalog
/// column types.
#[derive(Clone, Debug)]
pub struct ChangeEvent {
    pub schema: String,
    pub table: String,
    pub op: ChangeOp,
    /// The row document before the change (`UPDATE` / `DELETE`).
    pub old: Option<Json>,
    /// The row document after the change (`INSERT` / `UPDATE`).
    pub new: Option<Json>,
    /// When the local commit applied this change.
    pub commit_time: chrono::DateTime<chrono::Utc>,
}

/// The kind of row change a [`ChangeEvent`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

impl ChangeOp {
    /// The PostgreSQL logical-replication spelling (`INSERT`/`UPDATE`/`DELETE`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeOp::Insert => "INSERT",
            ChangeOp::Update => "UPDATE",
            ChangeOp::Delete => "DELETE",
        }
    }
}

/// An in-flight explicit transaction (BEGIN ... COMMIT/ROLLBACK).
struct Transaction {
    catalog: Catalog,
    catalog_dirty: bool,
    /// collection -> row_id -> Some(doc) (upsert) / None (delete)
    overlay: HashMap<String, HashMap<String, Option<Json>>>,
    truncated: HashSet<String>,
    /// Set when a statement errors inside the block (PostgreSQL aborts the txn).
    aborted: bool,
}

/// A connection-scoped session.
pub struct Session<S: RelationalStorage> {
    db: Arc<Database<S>>,
    username: String,
    txn: Option<Transaction>,
    session_id: SessionId,
    lock_timeout: Option<Duration>,
    /// Session variables (`SET name = value`), including extension GUCs.
    vars: HashMap<String, String>,
    /// Lazily pinned connection to the PostgreSQL sidecar runtime (closed
    /// when the session drops, ending the backend session with it).
    sidecar: Option<crate::sql::ext::sidecar::SidecarConn>,
}

impl<S: RelationalStorage> Drop for Session<S> {
    fn drop(&mut self) {
        // Release any locks still held (e.g. session-level advisory locks) when
        // the connection goes away.
        self.db.locks.release_session(self.session_id);
    }
}

/// A parsed, reusable prepared statement.
#[derive(Clone)]
pub struct Prepared {
    pub sql: String,
    pub statement: Statement,
    pub param_count: usize,
}

impl<S: RelationalStorage> Session<S> {
    pub fn new(db: Arc<Database<S>>, username: impl Into<String>) -> Self {
        let session_id = db.locks.new_session();
        Self {
            db,
            username: username.into(),
            txn: None,
            session_id,
            lock_timeout: None,
            vars: HashMap::new(),
            sidecar: None,
        }
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Set a session variable directly (equivalent to `SET name = value`, no
    /// SQL round-trip). Used by the Supabase gateway to inject the request's
    /// JWT claims (`request.jwt.claims`) for row-security policy evaluation.
    pub fn set_var(&mut self, name: &str, value: &str) {
        self.vars
            .insert(name.to_ascii_lowercase(), value.to_string());
    }

    /// Parse and execute a (possibly multi-statement) SQL string.
    ///
    /// The input is split into top-level statements first (quote- and
    /// comment-aware, see [`crate::sql::parser::split_statements`]) so that
    /// `ALTER EXTENSION` — which sqlparser 0.62 has no AST for — can be routed
    /// to its hand parser; every other segment goes through [`parse_sql`]
    /// unchanged and all statements execute in their original order. Parsing
    /// happens up front, so a syntax error anywhere executes nothing.
    pub async fn execute(&mut self, sql: &str) -> Result<Vec<ExecResult>> {
        enum Piece {
            Statements(Vec<Statement>),
            AlterExtension(crate::sql::ext::alter::AlterExtension),
        }
        let mut pieces = Vec::new();
        for segment in crate::sql::parser::split_statements(sql) {
            if crate::sql::ext::alter::is_alter_extension(&segment) {
                pieces.push(Piece::AlterExtension(
                    crate::sql::ext::alter::parse_alter_extension(&segment)?,
                ));
            } else if let Some(feature) = unsupported_by_prefix(&segment) {
                // Truthfulness contract: statements the engine deliberately
                // does not implement are recognized by keyword prefix *before*
                // parsing, so every syntactic variant fails with the same
                // stable `0A000` — sqlparser accepts some spellings of these
                // and rejects others, which would otherwise leak a
                // form-dependent `42601` instead.
                return Err(SqlError::FeatureNotSupported(format!(
                    "{feature} is not supported"
                )));
            } else {
                pieces.push(Piece::Statements(crate::sql::parser::parse_sql(&segment)?));
            }
        }
        let mut results = Vec::new();
        for piece in pieces {
            match piece {
                Piece::Statements(stmts) => {
                    for stmt in stmts {
                        results.push(self.execute_one(&stmt, &[]).await?);
                    }
                }
                Piece::AlterExtension(cmd) => {
                    results.push(self.execute_alter_extension(&cmd).await?);
                }
            }
        }
        Ok(results)
    }

    /// Prepare a statement for the extended query protocol.
    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        // sqlparser has no ALTER EXTENSION AST, so it cannot be carried through
        // the extended protocol's parse/bind/execute pipeline.
        if crate::sql::ext::alter::is_alter_extension(sql) {
            return Err(SqlError::Syntax(
                "ALTER EXTENSION is only supported over the simple query protocol — \
                 send it as an unprepared statement"
                    .into(),
            ));
        }
        // Deliberately-unsupported statements keep their stable `0A000` here
        // too, instead of a form-dependent parser error (see
        // [`unsupported_by_prefix`]).
        if let Some(feature) = unsupported_by_prefix(sql) {
            return Err(SqlError::FeatureNotSupported(format!(
                "{feature} is not supported"
            )));
        }
        let mut statements = crate::sql::parser::parse_sql(sql)?;
        let statement = match statements.len() {
            0 => Statement::Query(Box::new(empty_query())),
            1 => statements.remove(0),
            _ => {
                return Err(SqlError::Syntax(
                    "cannot insert multiple commands into a prepared statement".into(),
                ));
            }
        };
        let param_count = count_placeholders(sql);
        Ok(Prepared {
            sql: sql.to_string(),
            statement,
            param_count,
        })
    }

    /// Execute one statement with bound parameters.
    pub async fn execute_one(
        &mut self,
        stmt: &Statement,
        params: &[SqlValue],
    ) -> Result<ExecResult> {
        // Transaction control bypasses locking/abort handling.
        match stmt {
            Statement::StartTransaction { .. } => return self.begin().await,
            Statement::Commit { .. } => return self.commit().await,
            Statement::Rollback { .. } => return self.rollback().await,
            _ => {}
        }

        // A failed transaction ignores commands until it is ended.
        if self.txn.as_ref().map(|t| t.aborted).unwrap_or(false) {
            return Err(SqlError::InFailedTransaction);
        }

        // `SET lock_timeout = ...` is observed here.
        if matches!(stmt, Statement::Set(_)) {
            self.apply_set(&stmt.to_string());
        }

        let outcome = self.execute_routed(stmt, params).await;
        if outcome.is_err() {
            // Any error inside an explicit transaction aborts it (PostgreSQL);
            // an autocommit statement releases the locks it took.
            match &mut self.txn {
                Some(txn) => txn.aborted = true,
                None => self.db.locks.release_transaction(self.session_id),
            }
        }
        outcome
    }

    /// Sidecar-aware execution wrapper. Routing rules (see
    /// `docs/postgres-compat.md`):
    ///
    /// 1. `CREATE EXTENSION` of a sidecar-strategy extension is forwarded to
    ///    the configured sidecar and recorded in the local catalog with the
    ///    version the sidecar reports.
    /// 2. `DROP EXTENSION` naming a sidecar-bound extension forwards the drop
    ///    before removing the local record.
    /// 3. A statement that fails locally with undefined function/type/table
    ///    is forwarded verbatim when a sidecar DSN is configured — autocommit
    ///    only: inside an explicit transaction the local error is kept with a
    ///    hint, because the sidecar cannot join a local transaction.
    async fn execute_routed(
        &mut self,
        stmt: &Statement,
        params: &[SqlValue],
    ) -> Result<ExecResult> {
        use crate::sql::ext::RuntimeStrategy;
        if let Statement::CreateExtension(ce) = stmt {
            let name = crate::sql::names::ident_name(&ce.name).to_lowercase();
            if let Some(def) = crate::sql::ext::find(&name)
                && def.strategy == RuntimeStrategy::SidecarPostgres
            {
                return self.sidecar_create_extension(stmt, ce, def).await;
            }
        }
        if let Statement::DropExtension(de) = stmt {
            let catalog = match &self.txn {
                Some(txn) => txn.catalog.clone(),
                None => self.load_catalog().await?,
            };
            let any_sidecar = de.names.iter().any(|ident| {
                catalog.extension_is_sidecar(&crate::sql::names::ident_name(ident).to_lowercase())
            });
            if any_sidecar {
                return self.sidecar_drop_extension(de, catalog).await;
            }
        }
        match self.execute_inner(stmt, params).await {
            Err(e) if sidecar_routable(&e) && self.sidecar_dsn().is_some() => {
                if self.in_transaction() {
                    Err(with_sidecar_txn_hint(e))
                } else if params.is_empty() {
                    // The failed statement's autocommit locks are still held.
                    self.db.locks.release_transaction(self.session_id);
                    let mut results = self.sidecar_exec(&stmt.to_string()).await?;
                    results
                        .pop()
                        .ok_or_else(|| SqlError::Storage("sidecar returned no result".into()))
                } else {
                    // Bound parameters cannot be forwarded as verbatim text.
                    Err(e)
                }
            }
            other => other,
        }
    }

    /// `CREATE EXTENSION` of a sidecar-strategy extension: forward verbatim,
    /// then record the install locally with the version the sidecar reports.
    async fn sidecar_create_extension(
        &mut self,
        stmt: &Statement,
        ce: &sqlparser::ast::CreateExtension,
        def: &'static crate::sql::ext::ExtensionDef,
    ) -> Result<ExecResult> {
        let mut catalog = match &self.txn {
            Some(txn) => txn.catalog.clone(),
            None => self.load_catalog().await?,
        };
        if catalog.extension_installed(def.name) {
            if ce.if_not_exists {
                return Ok(ExecResult::empty_command("CREATE EXTENSION"));
            }
            return Err(SqlError::DuplicateObject(format!(
                "extension \"{}\"",
                def.name
            )));
        }
        if self.in_transaction() {
            return Err(SqlError::FeatureNotSupported(format!(
                "CREATE EXTENSION {} cannot run inside a transaction block — the \
                 PostgreSQL sidecar cannot join a local transaction",
                def.name
            )));
        }
        if self.sidecar_dsn().is_none() {
            return Err(crate::sql::ext::sidecar_unconfigured(def.name));
        }
        self.sidecar_exec(&stmt.to_string()).await?;
        let version = self
            .sidecar_scalar(&format!(
                "SELECT extversion FROM pg_extension WHERE extname = '{}'",
                def.name
            ))
            .await?
            .unwrap_or_else(|| def.default_version.to_string());
        catalog.install_sidecar_extension(def.name, &version);
        self.persist_catalog(catalog).await?;
        Ok(ExecResult::empty_command("CREATE EXTENSION"))
    }

    /// `DROP EXTENSION` where at least one name is sidecar-bound: forward each
    /// sidecar drop, apply native semantics to the rest, then persist.
    async fn sidecar_drop_extension(
        &mut self,
        de: &sqlparser::ast::DropExtension,
        mut catalog: Catalog,
    ) -> Result<ExecResult> {
        use sqlparser::ast::ReferentialAction as RA;
        if self.in_transaction() {
            return Err(SqlError::FeatureNotSupported(
                "DROP EXTENSION of a sidecar-bound extension cannot run inside a \
                 transaction block — the PostgreSQL sidecar cannot join a local \
                 transaction"
                    .into(),
            ));
        }
        for ident in &de.names {
            let name = crate::sql::names::ident_name(ident).to_lowercase();
            if catalog.extension_is_sidecar(&name) {
                if self.sidecar_dsn().is_none() {
                    return Err(crate::sql::ext::sidecar_unconfigured(&name));
                }
                let mut forward = String::from("DROP EXTENSION ");
                if de.if_exists {
                    forward.push_str("IF EXISTS ");
                }
                forward.push('"');
                forward.push_str(&name);
                forward.push('"');
                match de.cascade_or_restrict {
                    Some(RA::Cascade) => forward.push_str(" CASCADE"),
                    Some(RA::Restrict) => forward.push_str(" RESTRICT"),
                    _ => {}
                }
                self.sidecar_exec(&forward).await?;
                catalog.uninstall_extension(&name);
            } else {
                crate::sql::ext::drop_native_extension(
                    &mut catalog,
                    &name,
                    de.if_exists,
                    de.cascade_or_restrict,
                )?;
            }
        }
        self.persist_catalog(catalog).await?;
        Ok(ExecResult::empty_command("DROP EXTENSION"))
    }

    /// The configured sidecar DSN: the `guardian.sidecar_dsn` session variable
    /// wins; the `GUARDIAN_PG_SIDECAR_DSN` environment variable is the
    /// fallback. Empty values mean "not configured".
    fn sidecar_dsn(&self) -> Option<String> {
        if let Some(v) = self.vars.get("guardian.sidecar_dsn") {
            let v = v.trim();
            if v.is_empty() {
                return None; // SET guardian.sidecar_dsn = '' disables routing
            }
            return Some(v.to_string());
        }
        std::env::var("GUARDIAN_PG_SIDECAR_DSN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Run `sql` on the pinned sidecar connection, connecting lazily and
    /// reconnecting when the DSN changed or the previous connection broke.
    async fn sidecar_exec(&mut self, sql: &str) -> Result<Vec<ExecResult>> {
        let dsn = self
            .sidecar_dsn()
            .ok_or_else(|| crate::sql::ext::sidecar_unconfigured("(sidecar)"))?;
        let reusable = self
            .sidecar
            .as_ref()
            .map(|c| c.dsn() == dsn && !c.is_broken())
            .unwrap_or(false);
        if !reusable {
            self.sidecar = Some(crate::sql::ext::sidecar::SidecarConn::connect(&dsn).await?);
        }
        let conn = self
            .sidecar
            .as_mut()
            .expect("sidecar connection just pinned");
        let result = conn.simple_query(sql).await;
        if conn.is_broken() {
            self.sidecar = None;
        }
        result
    }

    /// First column of the first row of a sidecar query, as text.
    async fn sidecar_scalar(&mut self, sql: &str) -> Result<Option<String>> {
        for result in self.sidecar_exec(sql).await? {
            if let ExecResult::Rows { rows, .. } = result {
                return Ok(rows
                    .first()
                    .and_then(|row| row.first())
                    .and_then(|v| v.to_text()));
            }
        }
        Ok(None)
    }

    async fn execute_inner(&mut self, stmt: &Statement, params: &[SqlValue]) -> Result<ExecResult> {
        let catalog = match &self.txn {
            Some(txn) => txn.catalog.clone(),
            None => self.load_catalog().await?,
        };

        // Explicit `LOCK TABLE ... IN <mode> MODE [NOWAIT]`.
        if let Statement::Lock(lock) = stmt {
            return self.exec_lock_table(lock, &catalog).await;
        }

        // Acquire the implicit table-level locks for this statement.
        for (oid, mode) in table_lock_plan(stmt, &catalog) {
            self.db
                .locks
                .acquire(
                    self.session_id,
                    LockObject::Table(oid),
                    mode,
                    LockScope::Transaction,
                    WaitPolicy::Wait,
                    self.lock_timeout,
                )
                .await?;
        }

        // Preload referenced tables.
        let mut names = Vec::new();
        collect_stmt(stmt, &mut names);
        // Foreign-key enforcement reads parents (existence checks) and, for
        // UPDATE/DELETE/upsert, reads and writes referencing tables
        // transitively (referential actions); preload that ripple too.
        names.extend(fk_preload(stmt, &catalog));
        // Row-security policies may reference other tables (e.g. in EXISTS
        // subqueries); preload those too so policy evaluation can scan them.
        let mut policy_names = Vec::new();
        for (schema, name) in &names {
            if let Some(q) = catalog.resolve_table_name(schema.as_deref(), name)
                && let Some(table) = catalog.get_table(&q)
                && table.rls_enabled
            {
                for policy in &table.policies {
                    for text in policy.using_expr.iter().chain(policy.check_expr.iter()) {
                        if let Ok(expr) = crate::sql::parser::parse_expr(text) {
                            collect_expr(&expr, &mut policy_names);
                        }
                    }
                }
            }
        }
        names.extend(policy_names);
        let mut tables: HashMap<QualifiedName, LoadedTable> = HashMap::new();
        for (schema, name) in &names {
            if let Some(q) = catalog.resolve_table_name(schema.as_deref(), name)
                && !tables.contains_key(&q)
                && let Some(loaded) = self.load_table(&catalog, &q).await?
            {
                tables.insert(q, loaded);
            }
        }

        // Build the synchronous execution context and run.
        let now = chrono::Utc::now();
        let mut exec = Exec::new(
            catalog,
            tables,
            params.to_vec(),
            now,
            self.db.name.clone(),
            self.username.clone(),
            self.db.locks.clone(),
            self.session_id,
        );
        exec.vars = std::cell::RefCell::new(self.vars.clone());
        // Row security: compute per-table visibility once, before anything is
        // evaluated (CTEs, scans and DML snapshots all consult it).
        exec.init_rls(stmt)?;
        // Pre-materialize top-level CTEs, in order. Recursive members of a
        // `WITH RECURSIVE` iterate to a fixpoint against a working table (see
        // `Exec::materialize_with`); everything else materializes exactly
        // once, non-recursively.
        if let Statement::Query(q) = stmt
            && let Some(with) = &q.with
        {
            exec.materialize_with(with)?;
        }
        let result = self.dispatch(&mut exec, stmt)?;
        // Persist variable writes made during execution (e.g. `set_limit`).
        self.vars = exec.vars.borrow().clone();

        // Acquire row / blocking-advisory locks queued during execution.
        let pending: Vec<_> = exec.pending_locks.borrow_mut().drain(..).collect();
        for (object, mode, scope) in pending {
            self.db
                .locks
                .acquire(
                    self.session_id,
                    object,
                    mode,
                    scope,
                    WaitPolicy::Wait,
                    self.lock_timeout,
                )
                .await?;
        }

        // Commit or stage the produced mutations / catalog changes.
        let mutations = std::mem::take(&mut exec.mutations);
        let catalog_dirty = exec.catalog_dirty;
        let new_catalog = exec.catalog;
        match &mut self.txn {
            Some(txn) => {
                txn.catalog = new_catalog;
                txn.catalog_dirty |= catalog_dirty;
                stage_mutations(txn, mutations);
            }
            None => {
                self.apply_mutations(mutations).await?;
                if catalog_dirty {
                    self.save_catalog(&new_catalog).await?;
                }
                // Autocommit: release the locks this statement acquired.
                self.db.locks.release_transaction(self.session_id);
            }
        }
        Ok(result)
    }

    async fn exec_lock_table(
        &mut self,
        lock: &sqlparser::ast::Lock,
        catalog: &Catalog,
    ) -> Result<ExecResult> {
        let mode = map_lock_table_mode(lock.lock_mode.clone());
        let wait = if lock.nowait {
            WaitPolicy::NoWait
        } else {
            WaitPolicy::Wait
        };
        for target in &lock.tables {
            let (schema, name) = crate::sql::names::split_schema_table(&target.name);
            let q = catalog
                .resolve_table_name(schema.as_deref(), &name)
                .ok_or_else(|| SqlError::UndefinedTable(name.clone()))?;
            let oid = catalog.require_table(&q)?.oid;
            self.db
                .locks
                .acquire(
                    self.session_id,
                    LockObject::Table(oid),
                    mode,
                    LockScope::Transaction,
                    wait,
                    self.lock_timeout,
                )
                .await?;
        }
        Ok(ExecResult::empty_command("LOCK TABLE"))
    }

    /// Observe `SET name = value`. `lock_timeout` feeds the lock manager; every
    /// other variable (extension GUCs like `pg_trgm.similarity_threshold`,
    /// application settings) is stored as a session variable readable via
    /// `SHOW` / `current_setting()`.
    fn apply_set(&mut self, text: &str) {
        let body = text.trim().trim_end_matches(';');
        let Some(eq) = body.find('=') else { return };
        // "SET [LOCAL|SESSION] <name>" — take the last identifier before `=`.
        let name = body[..eq]
            .trim()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .to_ascii_lowercase();
        if name.is_empty() {
            return;
        }
        let raw = body[eq + 1..]
            .trim()
            .trim_matches(|c| c == '\'' || c == '"')
            .to_string();
        if name == "lock_timeout" {
            let ms = parse_timeout_ms(&raw);
            self.lock_timeout = if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms))
            };
        }
        self.vars.insert(name, raw);
    }

    /// Execute a hand-parsed `ALTER EXTENSION`, mirroring `execute_one`'s
    /// transaction semantics (ignored while aborted; an error aborts an open
    /// block or releases autocommit locks).
    async fn execute_alter_extension(
        &mut self,
        cmd: &crate::sql::ext::alter::AlterExtension,
    ) -> Result<ExecResult> {
        if self.txn.as_ref().map(|t| t.aborted).unwrap_or(false) {
            return Err(SqlError::InFailedTransaction);
        }
        let outcome = self.alter_extension_inner(cmd).await;
        if outcome.is_err() {
            match &mut self.txn {
                Some(txn) => txn.aborted = true,
                None => self.db.locks.release_transaction(self.session_id),
            }
        }
        outcome
    }

    async fn alter_extension_inner(
        &mut self,
        cmd: &crate::sql::ext::alter::AlterExtension,
    ) -> Result<ExecResult> {
        use crate::sql::ext::alter::AlterExtensionAction as Action;
        let mut catalog = match &self.txn {
            Some(txn) => txn.catalog.clone(),
            None => self.load_catalog().await?,
        };
        // Every form requires the extension to be installed (PostgreSQL's
        // `extension "x" does not exist`, SQLSTATE 42704).
        if !catalog.extension_installed(&cmd.name) {
            return Err(SqlError::UndefinedObject(format!(
                "extension \"{}\"",
                cmd.name
            )));
        }
        match &cmd.action {
            Action::Update { to } => {
                let def = crate::sql::ext::find(&cmd.name).ok_or_else(|| {
                    SqlError::UndefinedObject(format!("extension \"{}\"", cmd.name))
                })?;
                if let Some(v) = to
                    && v != def.default_version
                {
                    return Err(SqlError::UndefinedObject(format!(
                        "extension \"{}\" has no update path to version \"{v}\" \
                         (available version: \"{}\")",
                        def.name, def.default_version
                    )));
                }
                catalog.set_extension_version(def.name, def.default_version);
                self.persist_catalog(catalog).await?;
                Ok(ExecResult::empty_command("ALTER EXTENSION"))
            }
            Action::SetSchema(_) => Err(SqlError::FeatureNotSupported(format!(
                "extension \"{}\" is not relocatable",
                cmd.name
            ))),
            Action::Add(obj) | Action::Drop(obj) => Err(SqlError::FeatureNotSupported(format!(
                "ALTER EXTENSION {} {} {obj}: PostgreSQL reserves extension membership \
                 changes for extension scripts, and GuardianDB extension contents are fixed",
                cmd.name,
                if matches!(cmd.action, Action::Add(_)) {
                    "ADD"
                } else {
                    "DROP"
                },
            ))),
        }
    }

    /// Persist a modified catalog the way `execute_inner`'s tail does: stage
    /// it on the open transaction, or save it and release autocommit locks.
    async fn persist_catalog(&mut self, catalog: Catalog) -> Result<()> {
        match &mut self.txn {
            Some(txn) => {
                txn.catalog = catalog;
                txn.catalog_dirty = true;
            }
            None => {
                self.save_catalog(&catalog).await?;
                self.db.locks.release_transaction(self.session_id);
            }
        }
        Ok(())
    }

    fn dispatch(&self, exec: &mut Exec, stmt: &Statement) -> Result<ExecResult> {
        match stmt {
            Statement::Query(q) => {
                // Row-level locking (FOR UPDATE / FOR SHARE [NOWAIT | SKIP LOCKED]).
                exec.prepare_for_update(q)?;
                let rs = exec.exec_select_query(q, &[])?;
                let fields = rs
                    .schema
                    .fields
                    .iter()
                    .map(|f| crate::sql::result::OutField::new(f.name.clone(), f.ty.clone()))
                    .collect();
                Ok(ExecResult::Rows {
                    fields,
                    rows: rs.rows,
                })
            }
            Statement::Insert(insert) => exec.exec_insert(insert),
            Statement::Update(update) => exec.exec_update(update),
            Statement::Delete(delete) => exec.exec_delete(delete),
            Statement::CreateTable(ct) => exec.exec_create_table(ct),
            Statement::CreateSchema {
                schema_name,
                if_not_exists,
                ..
            } => {
                let name = schema_name_to_string(schema_name);
                exec.exec_create_schema(&name, *if_not_exists)
            }
            Statement::CreateIndex(ci) => exec.exec_create_index(ci),
            Statement::CreateView(cv) => exec.exec_create_view(cv),
            Statement::AlterTable(alter) => exec.exec_alter_table(&alter.name, &alter.operations),
            Statement::Drop {
                object_type,
                if_exists,
                names,
                cascade,
                ..
            } => exec.exec_drop(object_type, *if_exists, names, *cascade),
            Statement::Truncate(_) => exec.exec_truncate(stmt),
            Statement::Set(_) => Ok(ExecResult::empty_command("SET")),
            Statement::CreatePolicy(cp) => exec.exec_create_policy(cp),
            Statement::DropPolicy(dp) => exec.exec_drop_policy(dp),
            Statement::CreateExtension(ce) => exec.exec_create_extension(ce),
            Statement::DropExtension(de) => exec.exec_drop_extension(de),
            Statement::ShowVariable { variable } => {
                let name = variable
                    .iter()
                    .map(|i| crate::sql::names::ident_name(i).to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join(".");
                let value = exec
                    .vars
                    .borrow()
                    .get(&name)
                    .cloned()
                    .or_else(|| crate::sql::ext::default_guc(&name).map(str::to_string))
                    .or_else(|| builtin_show_default(&name));
                match value {
                    Some(v) => Ok(ExecResult::Rows {
                        fields: vec![crate::sql::result::OutField::new(
                            name.clone(),
                            crate::relational::SqlType::Text,
                        )],
                        rows: vec![vec![crate::relational::SqlValue::Text(v)]],
                    }),
                    None => Err(SqlError::UndefinedObject(format!(
                        "unrecognized configuration parameter \"{name}\""
                    ))),
                }
            }
            // Truthfulness contract: features the engine deliberately does not
            // implement get a *named* stable rejection (0A000) rather than the
            // generic fallback message. These arms serve the extended query
            // protocol; the simple protocol already rejects the same statements
            // by keyword prefix (see [`unsupported_by_prefix`]).
            Statement::CreateFunction(_) => Err(SqlError::FeatureNotSupported(
                "CREATE FUNCTION is not supported".into(),
            )),
            Statement::CreateProcedure { .. } => Err(SqlError::FeatureNotSupported(
                "CREATE PROCEDURE is not supported".into(),
            )),
            Statement::CreateTrigger(_) => Err(SqlError::FeatureNotSupported(
                "CREATE TRIGGER is not supported".into(),
            )),
            Statement::DropTrigger(_) => Err(SqlError::FeatureNotSupported(
                "DROP TRIGGER is not supported".into(),
            )),
            other => self.dispatch_fallback(other),
        }
    }

    /// Handle utility statements (SET/SHOW/RESET/...) by inspecting the text.
    fn dispatch_fallback(&self, stmt: &Statement) -> Result<ExecResult> {
        let text = stmt.to_string();
        let mut words = text.split_whitespace();
        let first = words.next().unwrap_or("").to_ascii_uppercase();
        let second = words.next().unwrap_or("").to_ascii_uppercase();
        // Extension / sequence management is a no-op (sequences are managed
        // implicitly by serial columns; no extensions are required).
        if matches!(
            (first.as_str(), second.as_str()),
            ("CREATE", "EXTENSION")
                | ("DROP", "EXTENSION")
                | ("CREATE", "SEQUENCE")
                | ("ALTER", "SEQUENCE")
                | ("DROP", "SEQUENCE")
        ) {
            return Ok(ExecResult::empty_command(format!("{first} {second}")));
        }
        match first.as_str() {
            "SET" | "RESET" | "DISCARD" | "DEALLOCATE" | "LISTEN" | "UNLISTEN" | "CHECKPOINT"
            | "CLOSE" | "ANALYZE" | "VACUUM" | "COMMENT" | "GRANT" | "REVOKE" | "SAVEPOINT"
            | "RELEASE" | "PREPARE" | "EXECUTE" => Ok(ExecResult::empty_command(first)),
            "SHOW" => {
                let var = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .trim_end_matches(';')
                    .to_string();
                let value = show_value(&var);
                Ok(ExecResult::Rows {
                    fields: vec![crate::sql::result::OutField::new(
                        if var.is_empty() {
                            "show".to_string()
                        } else {
                            var
                        },
                        crate::relational::SqlType::Text,
                    )],
                    rows: vec![vec![SqlValue::Text(value)]],
                })
            }
            _ => Err(SqlError::FeatureNotSupported(format!(
                "statement not supported: {first}"
            ))),
        }
    }

    // ---- transaction control -------------------------------------------

    async fn begin(&mut self) -> Result<ExecResult> {
        if self.txn.is_none() {
            let catalog = self.load_catalog().await?;
            self.txn = Some(Transaction {
                catalog,
                catalog_dirty: false,
                overlay: HashMap::new(),
                truncated: HashSet::new(),
                aborted: false,
            });
        }
        Ok(ExecResult::empty_command("BEGIN"))
    }

    async fn commit(&mut self) -> Result<ExecResult> {
        if let Some(txn) = self.txn.take() {
            // Committing an aborted transaction rolls it back (PostgreSQL).
            if txn.aborted {
                self.db.locks.release_transaction(self.session_id);
                return Ok(ExecResult::empty_command("ROLLBACK"));
            }
            let watch = self.db.has_change_listeners();
            let at = chrono::Utc::now();
            let mut events = Vec::new();
            for c in &txn.truncated {
                self.db.storage.truncate(c).await?;
            }
            for (collection, rows) in &txn.overlay {
                for (rid, val) in rows {
                    let old = if watch {
                        self.db.storage.get(collection, rid).await?
                    } else {
                        None
                    };
                    match val {
                        Some(doc) => {
                            if watch {
                                push_change(&mut events, old.as_ref(), Some(doc), at);
                            }
                            self.db.storage.put(collection, rid, doc).await?
                        }
                        None => {
                            if watch {
                                push_change(&mut events, old.as_ref(), None, at);
                            }
                            self.db.storage.delete(collection, rid).await?
                        }
                    }
                }
            }
            if txn.catalog_dirty {
                self.save_catalog(&txn.catalog).await?;
            }
            self.db.emit_changes(events);
        }
        self.db.locks.release_transaction(self.session_id);
        Ok(ExecResult::empty_command("COMMIT"))
    }

    async fn rollback(&mut self) -> Result<ExecResult> {
        self.txn = None;
        self.db.locks.release_transaction(self.session_id);
        Ok(ExecResult::empty_command("ROLLBACK"))
    }

    // ---- storage helpers -----------------------------------------------

    async fn load_catalog(&self) -> Result<Catalog> {
        match self.db.storage.load_catalog().await? {
            Some(json) => serde_json::from_value(json)
                .map_err(|e| SqlError::Storage(format!("corrupt catalog: {e}"))),
            None => Ok(Catalog::new(&self.db.name)),
        }
    }

    async fn save_catalog(&self, catalog: &Catalog) -> Result<()> {
        let json = serde_json::to_value(catalog)
            .map_err(|e| SqlError::Storage(format!("serialize catalog: {e}")))?;
        self.db.storage.save_catalog(&json).await
    }

    async fn load_table(
        &self,
        catalog: &Catalog,
        q: &QualifiedName,
    ) -> Result<Option<LoadedTable>> {
        let Some(table) = catalog.get_table(q) else {
            return Ok(None);
        };
        let collection = table.storage_collection.clone();
        let mut docs = self.db.storage.scan(&collection).await?;
        if let Some(txn) = &self.txn {
            let truncated = txn.truncated.contains(&collection);
            let overlay = txn.overlay.get(&collection);
            if truncated || overlay.is_some() {
                let mut map: std::collections::BTreeMap<String, Json> = if truncated {
                    std::collections::BTreeMap::new()
                } else {
                    docs.into_iter().collect()
                };
                if let Some(ov) = overlay {
                    for (rid, val) in ov {
                        match val {
                            Some(doc) => {
                                map.insert(rid.clone(), doc.clone());
                            }
                            None => {
                                map.remove(rid);
                            }
                        }
                    }
                }
                docs = map.into_iter().collect();
            }
        }
        let index_defs = catalog
            .indexes_for_table(&q.schema, &q.name)
            .into_iter()
            .cloned()
            .collect();
        Ok(Some(LoadedTable::build(table.clone(), docs, index_defs)?))
    }

    async fn apply_mutations(&self, mutations: Vec<Mutation>) -> Result<()> {
        let watch = self.db.has_change_listeners();
        let at = chrono::Utc::now();
        let mut events = Vec::new();
        for m in mutations {
            match m {
                Mutation::Put {
                    collection,
                    row_id,
                    doc,
                } => {
                    if watch {
                        let old = self.db.storage.get(&collection, &row_id).await?;
                        push_change(&mut events, old.as_ref(), Some(&doc), at);
                    }
                    self.db.storage.put(&collection, &row_id, &doc).await?
                }
                Mutation::Delete { collection, row_id } => {
                    if watch {
                        let old = self.db.storage.get(&collection, &row_id).await?;
                        push_change(&mut events, old.as_ref(), None, at);
                    }
                    self.db.storage.delete(&collection, &row_id).await?
                }
                Mutation::Truncate { collection } => self.db.storage.truncate(&collection).await?,
            }
        }
        self.db.emit_changes(events);
        Ok(())
    }
}

/// Classify one storage write as a [`ChangeEvent`] and append it to `events`.
/// `old` is the stored document before the write (`None` when absent), `new`
/// the document being written (`None` for a physical delete). Tombstoned rows
/// (`__deleted: true`) count as absent, so a tombstoning put is a `DELETE` and
/// re-inserting over a tombstone is an `INSERT`. Documents that are not table
/// rows (no `__table` marker) produce no event.
fn push_change(
    events: &mut Vec<ChangeEvent>,
    old: Option<&Json>,
    new: Option<&Json>,
    at: chrono::DateTime<chrono::Utc>,
) {
    use crate::sql::store::{F_DELETED, F_ID, F_SCHEMA, F_TABLE};
    // A fn item (not a closure) so the input/output lifetimes elide correctly.
    fn live(doc: Option<&Json>) -> Option<&Json> {
        let doc = doc?;
        let obj = doc.as_object()?;
        obj.get(F_ID)?.as_str()?;
        if obj.get(F_DELETED).and_then(Json::as_bool).unwrap_or(false) {
            return None;
        }
        Some(doc)
    }
    let old_live = live(old);
    let new_live = live(new);
    let (op, source) = match (old_live, new_live) {
        (None, Some(n)) => (ChangeOp::Insert, n),
        (Some(_), Some(n)) => (ChangeOp::Update, n),
        (Some(o), None) => (ChangeOp::Delete, o),
        (None, None) => return,
    };
    let Some(obj) = source.as_object() else {
        return;
    };
    let Some(table) = obj.get(F_TABLE).and_then(Json::as_str) else {
        return;
    };
    let schema = obj
        .get(F_SCHEMA)
        .and_then(Json::as_str)
        .unwrap_or("public")
        .to_string();
    events.push(ChangeEvent {
        schema,
        table: table.to_string(),
        op,
        old: old_live.cloned(),
        new: new_live.cloned(),
        commit_time: at,
    });
}

/// Recognize statements the engine deliberately does not support by their
/// leading keywords, returning the feature name for the `0A000` message.
///
/// This runs on raw statement segments *before* parsing (the same mechanism
/// that routes `ALTER EXTENSION` to its hand parser), because sqlparser 0.62
/// parses only some spellings of these statements — e.g. it rejects the
/// PostgreSQL form of `CREATE PROCEDURE` with a `42601` — and the truthfulness
/// contract requires one stable rejection code for the whole family.
fn unsupported_by_prefix(segment: &str) -> Option<&'static str> {
    let words = leading_keywords(segment, 4);
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    match (w(0), w(1)) {
        ("CREATE", "FUNCTION") => Some("CREATE FUNCTION"),
        ("CREATE", "PROCEDURE") => Some("CREATE PROCEDURE"),
        ("CREATE", "TRIGGER") => Some("CREATE TRIGGER"),
        ("CREATE", "CONSTRAINT") if w(2) == "TRIGGER" => Some("CREATE TRIGGER"),
        ("CREATE", "OR") if w(2) == "REPLACE" => match w(3) {
            "FUNCTION" => Some("CREATE FUNCTION"),
            "PROCEDURE" => Some("CREATE PROCEDURE"),
            "TRIGGER" => Some("CREATE TRIGGER"),
            _ => None,
        },
        ("DROP", "TRIGGER") => Some("DROP TRIGGER"),
        _ => None,
    }
}

/// The first `max` bare keywords of a statement, upper-cased — skipping
/// whitespace, `--` line comments and (nested) `/* */` block comments.
/// Scanning stops at the first token that is not a bare word (a quoted
/// identifier, punctuation, ...), so only genuine leading keywords match.
fn leading_keywords(sql: &str, max: usize) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() && out.len() < max {
        match bytes[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1u32;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                out.push(sql[start..i].to_ascii_uppercase());
            }
            _ => break,
        }
    }
    out
}

/// Errors eligible for sidecar fallback-forwarding: the statement referenced
/// a function, type or relation the local engine does not have (typically
/// objects provided by a sidecar-routed extension).
fn sidecar_routable(e: &SqlError) -> bool {
    matches!(
        e,
        SqlError::UndefinedFunction(_) | SqlError::UndefinedType(_) | SqlError::UndefinedTable(_)
    )
}

/// Keep the local error (same SQLSTATE, same message) but explain why it was
/// not forwarded to the sidecar.
fn with_sidecar_txn_hint(e: SqlError) -> SqlError {
    SqlError::Sidecar {
        sqlstate: e.sqlstate().to_string(),
        message: format!(
            "{e} — hint: statements are not forwarded to the PostgreSQL sidecar inside a \
             transaction block (sidecar routing is autocommit-only)"
        ),
    }
}

/// The implicit table-level locks a statement takes, deduplicated to the
/// strongest mode per table (mirrors PostgreSQL's automatic locking).
fn table_lock_plan(stmt: &Statement, catalog: &Catalog) -> Vec<(u32, LockMode)> {
    use sqlparser::ast::{FromTable, ObjectType, TableFactor, TableObject};
    let resolve = |schema: Option<&str>, name: &str| -> Option<u32> {
        catalog
            .resolve_table_name(schema, name)
            .and_then(|q| catalog.get_table(&q).map(|t| t.oid))
    };
    let resolve_name =
        |out: &mut Vec<(u32, LockMode)>, name: &sqlparser::ast::ObjectName, mode: LockMode| {
            let (s, n) = crate::sql::names::split_schema_table(name);
            if let Some(oid) = resolve(s.as_deref(), &n) {
                out.push((oid, mode));
            }
        };
    let read_names = |out: &mut Vec<(u32, LockMode)>, names: &NameOut, mode: LockMode| {
        for (s, n) in names {
            if let Some(oid) = resolve(s.as_deref(), n) {
                out.push((oid, mode));
            }
        }
    };
    let mut plan = Vec::new();
    match stmt {
        Statement::Query(q) => {
            let mode = if q.locks.is_empty() {
                LockMode::AccessShare
            } else {
                LockMode::RowShare
            };
            let mut names = Vec::new();
            collect_query(q, &mut names);
            read_names(&mut plan, &names, mode);
        }
        Statement::Insert(i) => {
            if let TableObject::TableName(name) = &i.table {
                resolve_name(&mut plan, name, LockMode::RowExclusive);
            }
            if let Some(src) = &i.source {
                let mut names = Vec::new();
                collect_query(src, &mut names);
                read_names(&mut plan, &names, LockMode::AccessShare);
            }
        }
        Statement::Update(u) => {
            if let TableFactor::Table { name, .. } = &u.table.relation {
                resolve_name(&mut plan, name, LockMode::RowExclusive);
            }
            if let Some(sel) = &u.selection {
                let mut names = Vec::new();
                collect_expr(sel, &mut names);
                read_names(&mut plan, &names, LockMode::AccessShare);
            }
        }
        Statement::Delete(d) => {
            let items = match &d.from {
                FromTable::WithFromKeyword(items) | FromTable::WithoutKeyword(items) => items,
            };
            if let Some(twj) = items.first()
                && let TableFactor::Table { name, .. } = &twj.relation
            {
                resolve_name(&mut plan, name, LockMode::RowExclusive);
            }
            if let Some(sel) = &d.selection {
                let mut names = Vec::new();
                collect_expr(sel, &mut names);
                read_names(&mut plan, &names, LockMode::AccessShare);
            }
        }
        Statement::CreateIndex(ci) => resolve_name(&mut plan, &ci.table_name, LockMode::Share),
        Statement::AlterTable(a) => resolve_name(&mut plan, &a.name, LockMode::AccessExclusive),
        Statement::Drop {
            object_type: ObjectType::Table,
            names,
            ..
        } => {
            for name in names {
                resolve_name(&mut plan, name, LockMode::AccessExclusive);
            }
        }
        Statement::Truncate(t) => {
            for target in &t.table_names {
                resolve_name(&mut plan, &target.name, LockMode::AccessExclusive);
            }
        }
        _ => {}
    }
    // Foreign-key ripple around a DML target: referencing tables may be
    // written by referential actions (ROW EXCLUSIVE — the mode an explicit
    // UPDATE/DELETE on them takes) and parents are read for existence checks
    // (ROW SHARE, like PostgreSQL's FOR KEY SHARE probes).
    if let Some((name, include_children)) = fk_dml_target(stmt) {
        let (s, n) = crate::sql::names::split_schema_table(name);
        if let Some(q) = catalog.resolve_table_name(s.as_deref(), &n) {
            let (written, read) = crate::sql::fk::fk_ripple(catalog, &q, include_children);
            for (set, mode) in [
                (written, LockMode::RowExclusive),
                (read, LockMode::RowShare),
            ] {
                for fq in set {
                    if let Some(t) = catalog.get_table(&fq) {
                        plan.push((t.oid, mode));
                    }
                }
            }
        }
    }
    // Deduplicate to the strongest mode per table (lock in oid order to reduce
    // deadlocks between statements touching the same set of tables).
    let mut by_oid: std::collections::BTreeMap<u32, LockMode> = std::collections::BTreeMap::new();
    for (oid, mode) in plan {
        let entry = by_oid.entry(oid).or_insert(mode);
        if table_mode_rank(mode) > table_mode_rank(*entry) {
            *entry = mode;
        }
    }
    by_oid.into_iter().collect()
}

/// The DML target of `stmt` plus whether foreign-key referencing tables can
/// be *written* (referential actions): UPDATE/DELETE always; INSERT only when
/// an `ON CONFLICT DO UPDATE` can rewrite referenced columns (a plain INSERT
/// only reads parents).
fn fk_dml_target(stmt: &Statement) -> Option<(&sqlparser::ast::ObjectName, bool)> {
    use sqlparser::ast::{FromTable, OnConflictAction, OnInsert, TableFactor, TableObject};
    match stmt {
        Statement::Insert(i) => {
            let upsert = matches!(
                &i.on,
                Some(OnInsert::OnConflict(oc))
                    if matches!(oc.action, OnConflictAction::DoUpdate(_))
            );
            match &i.table {
                TableObject::TableName(name) => Some((name, upsert)),
                _ => None,
            }
        }
        Statement::Update(u) => match &u.table.relation {
            TableFactor::Table { name, .. } => Some((name, true)),
            _ => None,
        },
        Statement::Delete(d) => {
            let items = match &d.from {
                FromTable::WithFromKeyword(items) | FromTable::WithoutKeyword(items) => items,
            };
            match items.first().map(|twj| &twj.relation) {
                Some(TableFactor::Table { name, .. }) => Some((name, true)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extra table names foreign-key enforcement may touch for `stmt`, to be
/// preloaded alongside the statement's own references.
fn fk_preload(stmt: &Statement, catalog: &Catalog) -> NameOut {
    let Some((name, include_children)) = fk_dml_target(stmt) else {
        return Vec::new();
    };
    let (schema, n) = crate::sql::names::split_schema_table(name);
    let Some(q) = catalog.resolve_table_name(schema.as_deref(), &n) else {
        return Vec::new();
    };
    let (written, read) = crate::sql::fk::fk_ripple(catalog, &q, include_children);
    written
        .into_iter()
        .chain(read)
        .map(|fq| (Some(fq.schema), fq.name))
        .collect()
}

fn table_mode_rank(mode: LockMode) -> u8 {
    match mode {
        LockMode::AccessShare => 0,
        LockMode::RowShare => 1,
        LockMode::RowExclusive => 2,
        LockMode::ShareUpdateExclusive => 3,
        LockMode::Share => 4,
        LockMode::ShareRowExclusive => 5,
        LockMode::Exclusive => 6,
        LockMode::AccessExclusive => 7,
        _ => 0,
    }
}

fn map_lock_table_mode(mode: Option<sqlparser::ast::LockTableMode>) -> LockMode {
    use sqlparser::ast::LockTableMode as M;
    match mode {
        Some(M::AccessShare) => LockMode::AccessShare,
        Some(M::RowShare) => LockMode::RowShare,
        Some(M::RowExclusive) => LockMode::RowExclusive,
        Some(M::ShareUpdateExclusive) => LockMode::ShareUpdateExclusive,
        Some(M::Share) => LockMode::Share,
        Some(M::ShareRowExclusive) => LockMode::ShareRowExclusive,
        Some(M::Exclusive) => LockMode::Exclusive,
        // PostgreSQL's default for LOCK TABLE with no mode is ACCESS EXCLUSIVE.
        Some(M::AccessExclusive) | None => LockMode::AccessExclusive,
    }
}

/// Values `SHOW` reports for standard PostgreSQL parameters we do not track
/// as session variables. Mirrors what the pgwire startup already advertises.
fn builtin_show_default(name: &str) -> Option<String> {
    match name {
        "server_version" => Some("16.0 (GuardianDB)".to_string()),
        "server_encoding" | "client_encoding" => Some("UTF8".to_string()),
        "datestyle" => Some("ISO, MDY".to_string()),
        "timezone" | "time_zone" => Some("UTC".to_string()),
        "transaction_isolation" => Some("read committed".to_string()),
        "standard_conforming_strings" => Some("on".to_string()),
        "lock_timeout" => Some("0".to_string()),
        "search_path" => Some("\"$user\", public".to_string()),
        _ => None,
    }
}

fn parse_timeout_ms(raw: &str) -> u64 {
    let raw = raw.trim();
    if let Some(num) = raw.strip_suffix("ms") {
        num.trim().parse().unwrap_or(0)
    } else if let Some(num) = raw.strip_suffix('s') {
        num.trim().parse::<u64>().map(|n| n * 1000).unwrap_or(0)
    } else {
        raw.parse().unwrap_or(0)
    }
}

fn stage_mutations(txn: &mut Transaction, mutations: Vec<Mutation>) {
    for m in mutations {
        match m {
            Mutation::Put {
                collection,
                row_id,
                doc,
            } => {
                txn.overlay
                    .entry(collection)
                    .or_default()
                    .insert(row_id, Some(doc));
            }
            Mutation::Delete { collection, row_id } => {
                txn.overlay
                    .entry(collection)
                    .or_default()
                    .insert(row_id, None);
            }
            Mutation::Truncate { collection } => {
                txn.truncated.insert(collection.clone());
                txn.overlay.remove(&collection);
            }
        }
    }
}

fn show_value(var: &str) -> String {
    match var.to_ascii_lowercase().as_str() {
        "server_version" => "15.0".into(),
        "server_version_num" => "150000".into(),
        "server_encoding" | "client_encoding" => "UTF8".into(),
        "standard_conforming_strings" | "transaction_read_only" => "on".into(),
        "search_path" => "\"$user\", public".into(),
        "timezone" | "time zone" => "UTC".into(),
        "integer_datetimes" => "on".into(),
        _ => String::new(),
    }
}

fn schema_name_to_string(name: &sqlparser::ast::SchemaName) -> String {
    use sqlparser::ast::SchemaName;
    match name {
        SchemaName::Simple(n) => crate::sql::names::split_schema_table(n).1,
        SchemaName::NamedAuthorization(n, _) => crate::sql::names::split_schema_table(n).1,
        SchemaName::UnnamedAuthorization(ident) => crate::sql::names::ident_name(ident),
    }
}

fn empty_query() -> Query {
    // A harmless `SELECT NULL WHERE false`-style placeholder is overkill; reuse a
    // parsed empty SELECT.
    let stmts = crate::sql::parser::parse_sql("SELECT 1 WHERE 1=0").unwrap();
    match stmts.into_iter().next() {
        Some(Statement::Query(q)) => *q,
        _ => unreachable!(),
    }
}

/// Count `$n` placeholders in a SQL string (ignoring those inside string literals).
fn count_placeholders(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max = 0usize;
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if c == b'\'' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_string = true,
            b'$' => {
                let mut j = i + 1;
                let mut num = 0usize;
                let mut found = false;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    num = num * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                    found = true;
                }
                if found {
                    max = max.max(num);
                    i = j;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    max
}

// ---------------------------------------------------------------------------
// Table-reference collection (for preloading)
// ---------------------------------------------------------------------------

type NameOut = Vec<(Option<String>, String)>;

fn collect_stmt(stmt: &Statement, out: &mut NameOut) {
    match stmt {
        Statement::Query(q) => collect_query(q, out),
        Statement::Insert(i) => {
            if let sqlparser::ast::TableObject::TableName(name) = &i.table {
                push_name(name, out);
            }
            if let Some(src) = &i.source {
                collect_query(src, out);
            }
            if let Some(sqlparser::ast::OnInsert::OnConflict(oc)) = &i.on
                && let sqlparser::ast::OnConflictAction::DoUpdate(du) = &oc.action
                && let Some(sel) = &du.selection
            {
                collect_expr(sel, out);
            }
        }
        Statement::Update(u) => {
            collect_tf(&u.table.relation, out);
            for j in &u.table.joins {
                collect_tf(&j.relation, out);
            }
            for a in &u.assignments {
                collect_expr(&a.value, out);
            }
            if let Some(sel) = &u.selection {
                collect_expr(sel, out);
            }
        }
        Statement::Delete(d) => {
            match &d.from {
                sqlparser::ast::FromTable::WithFromKeyword(items)
                | sqlparser::ast::FromTable::WithoutKeyword(items) => {
                    for twj in items {
                        collect_twj(twj, out);
                    }
                }
            }
            if let Some(using) = &d.using {
                for twj in using {
                    collect_twj(twj, out);
                }
            }
            if let Some(sel) = &d.selection {
                collect_expr(sel, out);
            }
        }
        Statement::AlterTable(alter) => push_name(&alter.name, out),
        Statement::CreateIndex(ci) => push_name(&ci.table_name, out),
        Statement::CreateView(cv) => collect_query(&cv.query, out),
        Statement::Truncate(t) => {
            for target in &t.table_names {
                push_name(&target.name, out);
            }
        }
        _ => {}
    }
}

fn collect_query(q: &Query, out: &mut NameOut) {
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            collect_query(&cte.query, out);
        }
    }
    collect_setexpr(&q.body, out);
}

fn collect_setexpr(s: &sqlparser::ast::SetExpr, out: &mut NameOut) {
    use sqlparser::ast::SetExpr;
    match s {
        SetExpr::Select(sel) => collect_select(sel, out),
        SetExpr::Query(q) => collect_query(q, out),
        SetExpr::SetOperation { left, right, .. } => {
            collect_setexpr(left, out);
            collect_setexpr(right, out);
        }
        SetExpr::Values(v) => {
            for row in &v.rows {
                for e in &row.content {
                    collect_expr(e, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_select(sel: &sqlparser::ast::Select, out: &mut NameOut) {
    for twj in &sel.from {
        collect_twj(twj, out);
    }
    if let Some(w) = &sel.selection {
        collect_expr(w, out);
    }
    if let Some(h) = &sel.having {
        collect_expr(h, out);
    }
    for item in &sel.projection {
        if let sqlparser::ast::SelectItem::UnnamedExpr(e)
        | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } = item
        {
            collect_expr(e, out);
        }
    }
}

fn collect_twj(twj: &sqlparser::ast::TableWithJoins, out: &mut NameOut) {
    collect_tf(&twj.relation, out);
    for j in &twj.joins {
        collect_tf(&j.relation, out);
        if let sqlparser::ast::JoinOperator::Inner(sqlparser::ast::JoinConstraint::On(e))
        | sqlparser::ast::JoinOperator::Left(sqlparser::ast::JoinConstraint::On(e))
        | sqlparser::ast::JoinOperator::Right(sqlparser::ast::JoinConstraint::On(e))
        | sqlparser::ast::JoinOperator::FullOuter(sqlparser::ast::JoinConstraint::On(e)) =
            &j.join_operator
        {
            collect_expr(e, out);
        }
    }
}

fn collect_tf(tf: &sqlparser::ast::TableFactor, out: &mut NameOut) {
    use sqlparser::ast::TableFactor;
    match tf {
        TableFactor::Table { name, .. } => push_name(name, out),
        TableFactor::Derived { subquery, .. } => collect_query(subquery, out),
        _ => {}
    }
}

fn collect_expr(e: &sqlparser::ast::Expr, out: &mut NameOut) {
    use sqlparser::ast::Expr;
    match e {
        Expr::Subquery(q)
        | Expr::Exists { subquery: q, .. }
        | Expr::InSubquery { subquery: q, .. } => collect_query(q, out),
        Expr::BinaryOp { left, right, .. } => {
            collect_expr(left, out);
            collect_expr(right, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Cast { expr, .. } => collect_expr(expr, out),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_expr(expr, out);
            collect_expr(low, out);
            collect_expr(high, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_expr(expr, out);
            for e in list {
                collect_expr(e, out);
            }
        }
        Expr::Case {
            conditions,
            else_result,
            operand,
            ..
        } => {
            if let Some(o) = operand {
                collect_expr(o, out);
            }
            for w in conditions {
                collect_expr(&w.condition, out);
                collect_expr(&w.result, out);
            }
            if let Some(e) = else_result {
                collect_expr(e, out);
            }
        }
        _ => {}
    }
}

fn push_name(name: &sqlparser::ast::ObjectName, out: &mut NameOut) {
    out.push(crate::sql::names::split_schema_table(name));
}

// Maintenance note 1: documents compatibility expectations without changing runtime behavior.

// Maintenance note 13: documents compatibility expectations without changing runtime behavior.

// Maintenance note 25: documents compatibility expectations without changing runtime behavior.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.

// Maintenance note: keeps SQL compatibility behavior explicit for future updates.
