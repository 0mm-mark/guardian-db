//! Triggers: `CREATE TRIGGER` / `DROP TRIGGER` DDL, `ALTER TABLE ...
//! ENABLE/DISABLE TRIGGER`, and the firing engine the DML executors call.
//!
//! Supported surface: `BEFORE`/`AFTER` × `INSERT`/`UPDATE [OF cols]`/`DELETE`
//! × `FOR EACH ROW`/`FOR EACH STATEMENT`, `WHEN` conditions on row triggers,
//! `EXECUTE FUNCTION|PROCEDURE fn()` naming a zero-argument `RETURNS trigger`
//! PL/pgSQL function (see [`crate::sql::udf`]), and `OR REPLACE`. Everything
//! outside the subset fails typed at DDL time — `INSTEAD OF`, `TRUNCATE`
//! events, `CONSTRAINT TRIGGER`/`DEFERRABLE`, `REFERENCING` transition
//! tables, trigger arguments (`TG_ARGV`), `WHEN` on statement triggers —
//! see `docs/postgres-compat.md` for the full list.
//!
//! Firing semantics (PostgreSQL): same-event triggers fire in alphabetical
//! name order; a BEFORE ROW trigger's returned `NEW` feeds the next trigger
//! in the chain and `RETURN NULL` suppresses the row (skipping the rest of
//! the chain); AFTER ROW triggers observe the final rows, including
//! foreign-key cascade effects; statement-level triggers fire exactly once
//! per statement, even when zero rows are affected. **Documented
//! divergence**: rows removed/rewritten by cascaded referential actions
//! (`ON DELETE CASCADE`, `SET NULL`, `SET DEFAULT`) do *not* fire the child
//! table's own triggers — see the note on [`stage-2 path`](#fk-cascades)
//! below and `docs/postgres-compat.md`.
//!
//! # FK cascades (stage-2 path)
//!
//! PostgreSQL fires the child table's row triggers for cascade-affected
//! rows. `ri_delete_child`/`ri_update_child` (see [`crate::sql::fk`])
//! deliberately bypass the row pipeline; firing BEFORE triggers there would
//! let a `RETURN NULL` suppress a cascade row and leave a dangling
//! reference, which PostgreSQL handles by re-running RI checks. Making that
//! correct requires routing the cascade writers through shared per-row
//! helpers that treat BEFORE-suppression as a pending RI violation and fold
//! new work into the existing RI queue — deferred, and pinned as a
//! documented divergence by `tests/sql_triggers.rs::
//! fk_cascade_does_not_fire_child_triggers`.

use crate::relational::FunctionDef;
use crate::relational::catalog::{
    QualifiedName, Table, TriggerDef, TriggerEventDef, TriggerLevel, TriggerTiming,
};
use crate::sql::dml::{row_tuple, table_schema_named};
use crate::sql::error::{Result, SqlError, unsupported};
use crate::sql::exec::{Exec, Frame};
use crate::sql::names::{ident_name, object_name_parts, split_schema_table};
use crate::sql::result::ExecResult;
use crate::sql::store::RowValues;
use crate::sql::udf::TriggerInvocation;
use sqlparser::ast::{
    CreateTrigger, DropTrigger, Expr, Ident, TriggerEvent, TriggerObject, TriggerObjectKind,
    TriggerPeriod,
};
use std::collections::BTreeSet;

/// `pg_trigger.tgtype` bits (PostgreSQL's values). `TRUNCATE` (32) and
/// `INSTEAD` (64) are never set — those trigger forms are rejected at DDL.
pub(crate) const TGTYPE_ROW: i16 = 1;
pub(crate) const TGTYPE_BEFORE: i16 = 2;
pub(crate) const TGTYPE_INSERT: i16 = 4;
pub(crate) const TGTYPE_DELETE: i16 = 8;
pub(crate) const TGTYPE_UPDATE: i16 = 16;

/// The PostgreSQL `pg_trigger.tgtype` bitmask for a stored trigger.
pub(crate) fn tgtype(trg: &TriggerDef) -> i16 {
    let mut bits = 0i16;
    if trg.level == TriggerLevel::Row {
        bits |= TGTYPE_ROW;
    }
    if trg.timing == TriggerTiming::Before {
        bits |= TGTYPE_BEFORE;
    }
    for e in &trg.events {
        bits |= match e {
            TriggerEventDef::Insert => TGTYPE_INSERT,
            TriggerEventDef::Update { .. } => TGTYPE_UPDATE,
            TriggerEventDef::Delete => TGTYPE_DELETE,
        };
    }
    bits
}

/// The operation a firing corresponds to (`TG_OP`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerOp {
    Insert,
    Update,
    Delete,
}

impl TriggerOp {
    /// The `TG_OP` spelling.
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            TriggerOp::Insert => "INSERT",
            TriggerOp::Update => "UPDATE",
            TriggerOp::Delete => "DELETE",
        }
    }
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

impl Exec {
    pub fn exec_create_trigger(&mut self, ct: &CreateTrigger) -> Result<ExecResult> {
        // Out-of-subset forms fail typed (0A000, naming the construct) before
        // anything is resolved — the repo's truthfulness contract.
        if ct.temporary {
            return Err(unsupported("CREATE TEMPORARY TRIGGER"));
        }
        if ct.or_alter {
            return Err(unsupported("CREATE OR ALTER TRIGGER"));
        }
        if ct.is_constraint {
            return Err(unsupported("CONSTRAINT TRIGGER"));
        }
        let timing = match ct.period {
            Some(TriggerPeriod::Before) => TriggerTiming::Before,
            Some(TriggerPeriod::After) => TriggerTiming::After,
            Some(TriggerPeriod::InsteadOf) => return Err(unsupported("INSTEAD OF triggers")),
            // `FOR` is an MSSQL spelling; a missing timing cannot come from
            // the PostgreSQL grammar either.
            Some(TriggerPeriod::For) | None => {
                return Err(SqlError::Syntax(
                    "CREATE TRIGGER requires BEFORE or AFTER".into(),
                ));
            }
        };
        if ct.referenced_table_name.is_some() {
            return Err(unsupported("constraint-trigger FROM clause"));
        }
        if !ct.referencing.is_empty() {
            return Err(unsupported("REFERENCING transition tables"));
        }
        if ct.characteristics.is_some() {
            return Err(unsupported("DEFERRABLE trigger characteristics"));
        }
        if ct.statements.is_some() || ct.exec_body.is_none() {
            return Err(unsupported("trigger body without EXECUTE FUNCTION"));
        }
        let exec_body = ct.exec_body.as_ref().expect("checked above");
        if exec_body
            .func_desc
            .args
            .as_ref()
            .is_some_and(|a| !a.is_empty())
        {
            return Err(unsupported("trigger arguments (TG_ARGV)"));
        }

        // Target table. A view target names the one trigger form views could
        // ever support (INSTEAD OF), which is itself unsupported.
        let (schema, n) = split_schema_table(&ct.table_name);
        let q = self
            .catalog
            .resolve_table_name(schema.as_deref(), &n)
            .ok_or_else(|| SqlError::UndefinedTable(n.clone()))?;
        if self.catalog.get_view(&q).is_some() {
            return Err(SqlError::WrongObjectType(format!(
                "\"{}\" is a view — triggers on views require INSTEAD OF, which is not supported",
                q.name
            )));
        }
        let table = self.catalog.require_table(&q)?.clone();

        // Row/statement level; PostgreSQL defaults to STATEMENT when the
        // FOR EACH clause is omitted.
        let level = match &ct.trigger_object {
            None => TriggerLevel::Statement,
            Some(TriggerObjectKind::For(o)) | Some(TriggerObjectKind::ForEach(o)) => match o {
                TriggerObject::Row => TriggerLevel::Row,
                TriggerObject::Statement => TriggerLevel::Statement,
            },
        };

        // Events (`INSERT OR UPDATE [OF cols] OR DELETE`).
        let event_kind = |e: &TriggerEventDef| match e {
            TriggerEventDef::Insert => 0u8,
            TriggerEventDef::Update { .. } => 1,
            TriggerEventDef::Delete => 2,
        };
        let mut events: Vec<TriggerEventDef> = Vec::new();
        for ev in &ct.events {
            let mapped = match ev {
                TriggerEvent::Insert => TriggerEventDef::Insert,
                TriggerEvent::Delete => TriggerEventDef::Delete,
                TriggerEvent::Truncate => return Err(unsupported("TRUNCATE triggers")),
                TriggerEvent::Update(cols) => {
                    let mut columns = Vec::new();
                    for c in cols {
                        let cname = ident_name(c);
                        if table.column(&cname).is_none() {
                            return Err(SqlError::UndefinedColumn(cname));
                        }
                        columns.push(cname);
                    }
                    TriggerEventDef::Update { columns }
                }
            };
            if events.iter().any(|e| event_kind(e) == event_kind(&mapped)) {
                // PostgreSQL's message and SQLSTATE (42601).
                return Err(SqlError::Syntax(
                    "duplicate trigger events specified".into(),
                ));
            }
            events.push(mapped);
        }
        if events.is_empty() {
            return Err(SqlError::Syntax(
                "CREATE TRIGGER requires at least one event".into(),
            ));
        }

        // WHEN condition (row triggers only; stored as raw SQL text and
        // re-parsed at fire time — the Policy expression pattern).
        let when_expr = match &ct.condition {
            None => None,
            Some(cond) => {
                if level == TriggerLevel::Statement {
                    // PostgreSQL allows constant-only WHEN there; a named
                    // rejection is truthful and keeps the surface small.
                    return Err(unsupported("WHEN conditions on statement-level triggers"));
                }
                let mut refs = WhenRefs::default();
                validate_when_expr(cond, &table, &mut refs)?;
                let has_insert = events.iter().any(|e| matches!(e, TriggerEventDef::Insert));
                let has_delete = events.iter().any(|e| matches!(e, TriggerEventDef::Delete));
                if refs.old && has_insert {
                    return Err(SqlError::InvalidObjectDefinition(
                        "INSERT trigger's WHEN condition cannot reference OLD values".into(),
                    ));
                }
                if refs.new && has_delete {
                    return Err(SqlError::InvalidObjectDefinition(
                        "DELETE trigger's WHEN condition cannot reference NEW values".into(),
                    ));
                }
                Some(cond.to_string())
            }
        };

        // The trigger function: resolved once, here (search path applied at
        // DDL time, like foreign-key ref_schema). Must be a zero-argument
        // `RETURNS trigger` function.
        let (fschema, fname) = split_schema_table(&exec_body.func_desc.name);
        let def = self
            .catalog
            .find_function(fschema.as_deref(), &fname, 0)
            .ok_or_else(|| SqlError::UndefinedFunction(format!("{fname}()")))?;
        if !def.returns_trigger {
            return Err(SqlError::InvalidObjectDefinition(format!(
                "function {fname} must return type trigger"
            )));
        }
        let function_schema = def.schema.clone();
        let function_name = def.name.clone();

        // Trigger name: per-table namespace, never schema-qualified
        // (PostgreSQL: "trigger name cannot be qualified").
        let name_parts = object_name_parts(&ct.name);
        if name_parts.len() > 1 {
            return Err(SqlError::Syntax("trigger name cannot be qualified".into()));
        }
        let name = name_parts.into_iter().next().unwrap_or_default();
        if name.is_empty() {
            return Err(SqlError::Syntax("trigger name cannot be empty".into()));
        }
        let existing_oid = table.trigger(&name).map(|t| t.oid);
        if existing_oid.is_some() && !ct.or_replace {
            return Err(SqlError::DuplicateObject(format!(
                "trigger \"{name}\" for relation \"{}\"",
                q.name
            )));
        }

        // `OR REPLACE` overwrites in place preserving the oid, mirroring
        // `Catalog::replace_function`.
        let oid = match existing_oid {
            Some(oid) => oid,
            None => self.catalog.allocate_oid(),
        };
        let trg = TriggerDef {
            oid,
            name: name.clone(),
            timing,
            events,
            level,
            when_expr,
            function_schema,
            function_name,
            enabled: true,
        };
        let table = self.catalog.get_table_mut(&q).expect("resolved above");
        match table.triggers.iter_mut().find(|t| t.name == name) {
            Some(slot) => *slot = trg,
            None => table.triggers.push(trg),
        }
        self.catalog_dirty = true;
        Ok(ExecResult::empty_command("CREATE TRIGGER"))
    }

    pub fn exec_drop_trigger(&mut self, dt: &DropTrigger) -> Result<ExecResult> {
        // PostgreSQL's grammar requires `ON <table>`; sqlparser also accepts
        // the bare MySQL form.
        let Some(table_name) = &dt.table_name else {
            return Err(SqlError::Syntax("DROP TRIGGER requires ON <table>".into()));
        };
        let name_parts = object_name_parts(&dt.trigger_name);
        if name_parts.len() > 1 {
            return Err(SqlError::Syntax("trigger name cannot be qualified".into()));
        }
        let name = name_parts.into_iter().next().unwrap_or_default();
        // A missing *table* errors even under IF EXISTS (PostgreSQL).
        let (schema, n) = split_schema_table(table_name);
        let q = self
            .catalog
            .resolve_table_name(schema.as_deref(), &n)
            .ok_or_else(|| SqlError::UndefinedTable(n.clone()))?;
        // `CASCADE`/`RESTRICT` (`dt.option`) are accepted and ignored:
        // nothing can depend on a trigger in this engine, so both behave
        // identically — exactly as they do in PostgreSQL for a trigger with
        // no dependents.
        let removed = self
            .catalog
            .get_table_mut(&q)
            .map(|table| {
                let before = table.triggers.len();
                table.triggers.retain(|t| t.name != name);
                table.triggers.len() != before
            })
            // A view resolved: views cannot carry triggers here.
            .unwrap_or(false);
        if !removed && !dt.if_exists {
            return Err(SqlError::UndefinedObject(format!(
                "trigger \"{name}\" for table \"{}\"",
                q.name
            )));
        }
        self.catalog_dirty = true;
        Ok(ExecResult::empty_command("DROP TRIGGER"))
    }

    /// `ALTER TABLE ... ENABLE/DISABLE TRIGGER [name | ALL | USER]`.
    /// GuardianDB has no internal (system) triggers, so `ALL` and `USER` are
    /// the same set.
    pub(crate) fn exec_set_trigger_enabled(
        &mut self,
        q: &QualifiedName,
        name: &Ident,
        enabled: bool,
    ) -> Result<()> {
        let target = ident_name(name);
        let table = self
            .catalog
            .get_table_mut(q)
            .ok_or_else(|| SqlError::UndefinedTable(q.to_string_qualified()))?;
        if target == "all" || target == "user" {
            for trg in &mut table.triggers {
                trg.enabled = enabled;
            }
            return Ok(());
        }
        match table.triggers.iter_mut().find(|t| t.name == target) {
            Some(trg) => {
                trg.enabled = enabled;
                Ok(())
            }
            None => Err(SqlError::UndefinedObject(format!(
                "trigger \"{target}\" for table \"{}\"",
                q.name
            ))),
        }
    }
}

/// Which of `NEW`/`OLD` a WHEN condition references.
#[derive(Default)]
struct WhenRefs {
    new: bool,
    old: bool,
}

/// DDL-time validation of a trigger `WHEN` condition: every column reference
/// must be `NEW.col`/`OLD.col` naming a real column (42703 otherwise, like
/// PostgreSQL, whose WHEN scope contains only the NEW/OLD range entries),
/// and subqueries are rejected (PostgreSQL: "WHEN condition cannot contain
/// subqueries" — here a named 0A000).
fn validate_when_expr(expr: &Expr, table: &Table, refs: &mut WhenRefs) -> Result<()> {
    match expr {
        Expr::Identifier(ident) => {
            let name = ident_name(ident);
            // Bare boolean keywords parse as identifiers in some positions.
            if matches!(name.as_str(), "true" | "false" | "null") {
                return Ok(());
            }
            Err(SqlError::UndefinedColumn(name))
        }
        Expr::CompoundIdentifier(parts) => {
            let names: Vec<String> = parts.iter().map(ident_name).collect();
            match names.as_slice() {
                [q, col] if q == "new" || q == "old" => {
                    if table.column(col).is_none() {
                        return Err(SqlError::UndefinedColumn(format!("{q}.{col}")));
                    }
                    if q == "new" {
                        refs.new = true;
                    } else {
                        refs.old = true;
                    }
                    Ok(())
                }
                _ => Err(SqlError::UndefinedTable(
                    names.first().cloned().unwrap_or_default(),
                )),
            }
        }
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err(unsupported("subqueries in trigger WHEN conditions"))
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_when_expr(left, table, refs)?;
            validate_when_expr(right, table, refs)
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            validate_when_expr(left, table, refs)?;
            validate_when_expr(right, table, refs)
        }
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            validate_when_expr(a, table, refs)?;
            validate_when_expr(b, table, refs)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner)
        | Expr::Cast { expr: inner, .. } => validate_when_expr(inner, table, refs),
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            validate_when_expr(inner, table, refs)?;
            validate_when_expr(low, table, refs)?;
            validate_when_expr(high, table, refs)
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            validate_when_expr(inner, table, refs)?;
            for e in list {
                validate_when_expr(e, table, refs)?;
            }
            Ok(())
        }
        Expr::Like {
            expr: inner,
            pattern,
            ..
        }
        | Expr::ILike {
            expr: inner,
            pattern,
            ..
        } => {
            validate_when_expr(inner, table, refs)?;
            validate_when_expr(pattern, table, refs)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                validate_when_expr(o, table, refs)?;
            }
            for w in conditions {
                validate_when_expr(&w.condition, table, refs)?;
                validate_when_expr(&w.result, table, refs)?;
            }
            if let Some(e) = else_result {
                validate_when_expr(e, table, refs)?;
            }
            Ok(())
        }
        Expr::Function(func) => {
            if let sqlparser::ast::FunctionArguments::List(list) = &func.args {
                for arg in &list.args {
                    let e = match arg {
                        sqlparser::ast::FunctionArg::Named { arg, .. }
                        | sqlparser::ast::FunctionArg::ExprNamed { arg, .. }
                        | sqlparser::ast::FunctionArg::Unnamed(arg) => arg,
                    };
                    if let sqlparser::ast::FunctionArgExpr::Expr(e) = e {
                        validate_when_expr(e, table, refs)?;
                    }
                }
            }
            Ok(())
        }
        // Literals and other leaf forms carry no column references; any
        // exotic composite form that slipped through still evaluates against
        // only the NEW/OLD frames at fire time, so an unknown reference fails
        // 42703 there rather than silently passing.
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Firing engine
// ---------------------------------------------------------------------------

/// Does `e` match a firing of `op`? `UPDATE OF` lists match the UPDATE
/// statement's assignment-target set (PostgreSQL semantics — the SET list,
/// not value diffs).
fn event_matches(e: &TriggerEventDef, op: TriggerOp, set_cols: Option<&BTreeSet<String>>) -> bool {
    match (e, op) {
        (TriggerEventDef::Insert, TriggerOp::Insert) => true,
        (TriggerEventDef::Delete, TriggerOp::Delete) => true,
        (TriggerEventDef::Update { columns }, TriggerOp::Update) => {
            columns.is_empty() || set_cols.is_some_and(|s| columns.iter().any(|c| s.contains(c)))
        }
        _ => false,
    }
}

/// Enabled triggers on `table` matching (timing, level, op), in name order —
/// PostgreSQL fires same-event triggers alphabetically.
fn triggers_for(
    table: &Table,
    timing: TriggerTiming,
    level: TriggerLevel,
    op: TriggerOp,
    set_cols: Option<&BTreeSet<String>>,
) -> Vec<TriggerDef> {
    let mut out: Vec<TriggerDef> = table
        .triggers
        .iter()
        .filter(|t| t.enabled && t.timing == timing && t.level == level)
        .filter(|t| t.events.iter().any(|e| event_matches(e, op, set_cols)))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Coerce every column of a trigger-returned row to its declared type — the
/// post-BEFORE-trigger equivalent of INSERT's per-column coercion (a trigger
/// may have assigned e.g. an int expression to a numeric column).
fn coerce_trigger_row(table: &Table, row: RowValues) -> Result<RowValues> {
    let mut out = RowValues::new();
    for (col, v) in row {
        let coerced = crate::sql::dml::coerce_to_col(v, table, &col)?;
        out.insert(col, coerced);
    }
    Ok(out)
}

impl Exec {
    /// BEFORE ... FOR EACH ROW chain for one candidate row. Returns the row
    /// to proceed with — `NEW` (as modified by the chain) for INSERT/UPDATE,
    /// `OLD` for DELETE — or `None` when a trigger returned `NULL`
    /// (suppression: the row is skipped and the remaining chain does not
    /// run, like PostgreSQL).
    pub(crate) fn fire_before_row(
        &mut self,
        table: &Table,
        op: TriggerOp,
        old: Option<&RowValues>,
        new: Option<RowValues>,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<Option<RowValues>> {
        let triggers = triggers_for(
            table,
            TriggerTiming::Before,
            TriggerLevel::Row,
            op,
            set_cols,
        );
        let mut new_row = new;
        for trg in &triggers {
            // WHEN false/NULL skips *this trigger*; the row itself proceeds.
            // Each trigger's WHEN sees the chain's current NEW (PostgreSQL).
            if !self.trigger_when_passes(trg, table, old, new_row.as_ref())? {
                continue;
            }
            let def = self.resolve_trigger_function(trg)?;
            let result = crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table,
                    op: op.as_sql(),
                    timing: TriggerTiming::Before,
                    level: TriggerLevel::Row,
                    old: old.cloned(),
                    new: if op == TriggerOp::Delete {
                        None
                    } else {
                        new_row.clone()
                    },
                },
            )?;
            match op {
                // BEFORE DELETE: NULL suppresses the deletion; a returned
                // row is otherwise ignored (there is no NEW to modify).
                TriggerOp::Delete => {
                    if result.is_none() {
                        return Ok(None);
                    }
                }
                TriggerOp::Insert | TriggerOp::Update => match result {
                    None => return Ok(None),
                    Some(row) => new_row = Some(coerce_trigger_row(table, row)?),
                },
            }
        }
        Ok(match op {
            TriggerOp::Delete => old.cloned(),
            TriggerOp::Insert | TriggerOp::Update => new_row,
        })
    }

    /// AFTER ... FOR EACH ROW: return values are ignored; `WHEN` still
    /// applies. Fired by the DML executors after the statement's writes
    /// (and its referential actions) have been applied.
    pub(crate) fn fire_after_row(
        &mut self,
        table: &Table,
        op: TriggerOp,
        old: Option<&RowValues>,
        new: Option<&RowValues>,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<()> {
        let triggers = triggers_for(table, TriggerTiming::After, TriggerLevel::Row, op, set_cols);
        for trg in &triggers {
            if !self.trigger_when_passes(trg, table, old, new)? {
                continue;
            }
            let def = self.resolve_trigger_function(trg)?;
            crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table,
                    op: op.as_sql(),
                    timing: TriggerTiming::After,
                    level: TriggerLevel::Row,
                    old: old.cloned(),
                    new: new.cloned(),
                },
            )?;
        }
        Ok(())
    }

    /// FOR EACH STATEMENT triggers for one (timing, op), fired exactly once
    /// per statement — including statements that affect zero rows.
    pub(crate) fn fire_statement_triggers(
        &mut self,
        table: &Table,
        timing: TriggerTiming,
        op: TriggerOp,
        set_cols: Option<&BTreeSet<String>>,
    ) -> Result<()> {
        let triggers = triggers_for(table, timing, TriggerLevel::Statement, op, set_cols);
        for trg in &triggers {
            let def = self.resolve_trigger_function(trg)?;
            crate::sql::udf::call_trigger_function(
                self,
                &def,
                TriggerInvocation {
                    trigger_name: &trg.name,
                    table,
                    op: op.as_sql(),
                    timing,
                    level: TriggerLevel::Statement,
                    old: None,
                    new: None,
                },
            )?;
        }
        Ok(())
    }

    /// Resolve a trigger's stored (schema, name) to its function definition.
    /// Dangling references are impossible through SQL (the `DROP FUNCTION`
    /// guard, 2BP01) — this fails typed for hand-edited catalogs.
    fn resolve_trigger_function(&self, trg: &TriggerDef) -> Result<FunctionDef> {
        self.catalog
            .find_function(Some(&trg.function_schema), &trg.function_name, 0)
            .cloned()
            .ok_or_else(|| SqlError::UndefinedFunction(format!("{}()", trg.function_name)))
    }

    /// Evaluate a row trigger's `WHEN` condition against the `NEW`/`OLD`
    /// frames — the same two-frame mechanism `ON CONFLICT`'s `excluded`
    /// pseudo-table uses. Fires iff the condition is TRUE (`NULL` skips,
    /// PostgreSQL semantics).
    fn trigger_when_passes(
        &self,
        trg: &TriggerDef,
        table: &Table,
        old: Option<&RowValues>,
        new: Option<&RowValues>,
    ) -> Result<bool> {
        let Some(text) = &trg.when_expr else {
            return Ok(true);
        };
        let expr = crate::sql::parser::parse_expr(text)?;
        let old_schema = table_schema_named(table, "old");
        let new_schema = table_schema_named(table, "new");
        let old_tuple = old.map(|r| row_tuple(table, r));
        let new_tuple = new.map(|r| row_tuple(table, r));
        let mut frames: Vec<Frame> = Vec::new();
        if let Some(t) = &old_tuple {
            frames.push(Frame {
                schema: &old_schema,
                row: t,
            });
        }
        if let Some(t) = &new_tuple {
            frames.push(Frame {
                schema: &new_schema,
                row: t,
            });
        }
        Ok(self.eval(&expr, &frames)?.truthy() == Some(true))
    }
}
