//! Foreign-key referential enforcement.
//!
//! Semantics are PostgreSQL's `MATCH SIMPLE` (the default): a child row
//! satisfies a foreign key when **any** of its FK columns is NULL; otherwise a
//! parent row matching *all* referenced columns must exist. Child-side checks
//! run on INSERT and on UPDATEs that change an FK column's value; parent-side
//! referential actions (`NO ACTION` / `RESTRICT` / `CASCADE` / `SET NULL` /
//! `SET DEFAULT`) run when a referenced row is deleted or a referenced key
//! column actually changes value.
//!
//! `NO ACTION` is checked per statement — after the statement's own writes are
//! applied to the loaded tables — not deferred to commit; `DEFERRABLE`
//! constraints are rejected at DDL time (`0A000`).
//!
//! PostgreSQL runs referential actions with the table owner's privileges, so
//! the internal child-row reads and writes here deliberately **bypass
//! row-level security**: they never consult the statement's RLS row filters
//! and never run the new-row policy checks. Everything else goes through the
//! normal write path ([`Exec::write_update`], [`Mutation`]s, pending row
//! locks), so an aborted statement or a `ROLLBACK` undoes cascades exactly
//! like direct writes.
//!
//! [`Mutation`]: crate::sql::store::Mutation

use crate::relational::catalog::{ForeignKey, QualifiedName, ReferentialAction, Table};
use crate::relational::{Catalog, SqlValue, composite_key, ordered_key};
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::Exec;
use crate::sql::store::RowValues;
use std::collections::{BTreeSet, VecDeque};

/// Upper bound on processed referential-action work items per statement, a
/// guard against pathological `ON UPDATE CASCADE` cycles that never reach a
/// fixpoint. Delete cascades terminate structurally (a deleted row leaves the
/// loaded table and can never match again), and update cascades stop when
/// values stop changing; the cap only exists so a degenerate constraint graph
/// fails typed instead of spinning.
const MAX_RI_STEPS: usize = 1_000_000;

/// A parent-side referential-action work item: a row of `table` that this
/// statement removed or rewrote.
pub(crate) enum RiWork {
    Deleted {
        table: QualifiedName,
        row: RowValues,
    },
    Updated {
        table: QualifiedName,
        old: RowValues,
        new: RowValues,
    },
}

/// A deferred-to-end-of-statement `NO ACTION`/`RESTRICT` verification.
struct PendingCheck {
    parent: QualifiedName,
    child: QualifiedName,
    fk: ForeignKey,
    key: String,
    key_vals: Vec<SqlValue>,
}

/// The tables a DML statement on `root` may touch through foreign keys:
/// `.0` — tables referential actions may *write* (all descendants through
/// referencing constraints), `.1` — tables *read* for existence checks (the
/// parents of `root` and of every writable table). `include_children` is
/// false for plain INSERT, which can only ever read parents.
pub(crate) fn fk_ripple(
    catalog: &Catalog,
    root: &QualifiedName,
    include_children: bool,
) -> (Vec<QualifiedName>, Vec<QualifiedName>) {
    let mut written: BTreeSet<QualifiedName> = BTreeSet::new();
    if include_children {
        let mut queue: VecDeque<QualifiedName> = VecDeque::from([root.clone()]);
        while let Some(q) = queue.pop_front() {
            for (child, _fk) in catalog.referencing_foreign_keys(&q) {
                if child != *root && written.insert(child.clone()) {
                    queue.push_back(child);
                }
            }
        }
    }
    let mut read: BTreeSet<QualifiedName> = BTreeSet::new();
    for q in std::iter::once(root).chain(written.iter()) {
        if let Some(table) = catalog.get_table(q) {
            for fk in &table.foreign_keys {
                if let Some(parent) =
                    catalog.resolve_table_name(Some(&fk.ref_schema), &fk.ref_table)
                {
                    read.insert(parent);
                }
            }
        }
    }
    (written.into_iter().collect(), read.into_iter().collect())
}

/// The child row's FK column values, in constraint order.
fn fk_values(fk: &ForeignKey, row: &RowValues) -> Vec<SqlValue> {
    fk.columns
        .iter()
        .map(|c| row.get(c).cloned().unwrap_or(SqlValue::Null))
        .collect()
}

/// The parent row's referenced column values, in constraint order.
fn ref_values(fk: &ForeignKey, row: &RowValues) -> Vec<SqlValue> {
    fk.ref_columns
        .iter()
        .map(|c| row.get(c).cloned().unwrap_or(SqlValue::Null))
        .collect()
}

fn display_vals(vals: &[SqlValue]) -> String {
    vals.iter()
        .map(|v| v.to_text().unwrap_or_else(|| "null".into()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Exec {
    /// Child-side enforcement for one row about to be written to `table`:
    /// every foreign key must be satisfied. With `old` given (an UPDATE), FKs
    /// whose column values did not change are skipped, like PostgreSQL's RI
    /// triggers (pre-existing references are not re-validated).
    pub(crate) fn fk_check_child(
        &self,
        table: &Table,
        new: &RowValues,
        old: Option<&RowValues>,
    ) -> Result<()> {
        for fk in &table.foreign_keys {
            if let Some(old) = old
                && ordered_key(&fk_values(fk, old)) == ordered_key(&fk_values(fk, new))
            {
                continue;
            }
            self.fk_check_one(table, fk, new)?;
        }
        Ok(())
    }

    fn fk_check_one(&self, table: &Table, fk: &ForeignKey, row: &RowValues) -> Result<()> {
        let vals = fk_values(fk, row);
        // MATCH SIMPLE: any NULL FK column satisfies the constraint.
        let Some(key) = composite_key(&vals) else {
            return Ok(());
        };
        let parent_q = self.fk_parent(fk)?;
        if self.fk_parent_has_key(&parent_q, fk, &key)? {
            return Ok(());
        }
        Err(SqlError::ForeignKeyViolation {
            table: table.name.clone(),
            constraint: fk.name.clone(),
            detail: format!(
                "Key ({})=({}) is not present in table \"{}\".",
                fk.columns.join(", "),
                display_vals(&vals),
                fk.ref_table
            ),
        })
    }

    /// Resolve the parent table of `fk` (schema-resolved at DDL time). A
    /// dangling reference (only possible in hand-edited catalogs) fails typed.
    fn fk_parent(&self, fk: &ForeignKey) -> Result<QualifiedName> {
        self.catalog
            .resolve_table_name(Some(&fk.ref_schema), &fk.ref_table)
            .ok_or_else(|| SqlError::UndefinedTable(format!("{}.{}", fk.ref_schema, fk.ref_table)))
    }

    /// Does any live parent row carry `key` on the FK's referenced columns?
    /// Uses an exactly-matching index (the referenced PK/unique) when loaded.
    fn fk_parent_has_key(
        &self,
        parent_q: &QualifiedName,
        fk: &ForeignKey,
        key: &str,
    ) -> Result<bool> {
        let loaded = self.fk_loaded(parent_q)?;
        for idx in &loaded.indexes {
            if idx.meta.columns == fk.ref_columns {
                return Ok(!idx.data.get(key).is_empty());
            }
        }
        Ok(loaded
            .rows
            .values()
            .any(|r| composite_key(&ref_values(fk, r)).as_deref() == Some(key)))
    }

    /// Live child rows whose FK columns all equal `key` (non-null equality —
    /// MATCH SIMPLE). Uses an exactly-matching index when one exists.
    fn fk_matching_children(
        &self,
        child_q: &QualifiedName,
        fk: &ForeignKey,
        key: &str,
    ) -> Result<Vec<(String, RowValues)>> {
        let loaded = self.fk_loaded(child_q)?;
        for idx in &loaded.indexes {
            if idx.meta.columns == fk.columns {
                return Ok(idx
                    .data
                    .get(key)
                    .into_iter()
                    .filter_map(|rid| loaded.rows.get(&rid).map(|r| (rid.clone(), r.clone())))
                    .collect());
            }
        }
        Ok(loaded
            .rows
            .iter()
            .filter(|(_, r)| composite_key(&fk_values(fk, r)).as_deref() == Some(key))
            .map(|(rid, r)| (rid.clone(), r.clone()))
            .collect())
    }

    fn fk_loaded(&self, q: &QualifiedName) -> Result<&crate::sql::store::LoadedTable> {
        self.tables.get(q).ok_or_else(|| {
            SqlError::Internal(format!(
                "foreign-key table {} was not preloaded",
                q.to_string_qualified()
            ))
        })
    }

    /// Run parent-side referential actions for `work`, breadth-first;
    /// cascades enqueue further work for the rows they remove or rewrite.
    /// `NO ACTION`/`RESTRICT` violations are collected and verified once every
    /// cascading action has applied — the per-statement check (a child row
    /// that the same statement also removes does not violate); deferral to
    /// commit is not implemented (`DEFERRABLE` is rejected at DDL time).
    pub(crate) fn fk_apply_referential_actions(&mut self, work: Vec<RiWork>) -> Result<()> {
        let mut queue: VecDeque<RiWork> = work.into();
        let mut checks: Vec<PendingCheck> = Vec::new();
        let mut steps = 0usize;
        while let Some(item) = queue.pop_front() {
            steps += 1;
            if steps > MAX_RI_STEPS {
                return Err(SqlError::Internal(
                    "foreign-key referential actions did not terminate".into(),
                ));
            }
            match item {
                RiWork::Deleted { table, row } => {
                    self.ri_on_delete(&table, &row, &mut queue, &mut checks)?
                }
                RiWork::Updated { table, old, new } => {
                    self.ri_on_update(&table, &old, &new, &mut queue, &mut checks)?
                }
            }
        }
        for c in checks {
            // Satisfied if the key reappeared (or survives on another parent
            // row) or no referencing child row is left.
            if self.fk_parent_has_key(&c.parent, &c.fk, &c.key)? {
                continue;
            }
            if !self
                .fk_matching_children(&c.child, &c.fk, &c.key)?
                .is_empty()
            {
                return Err(self.ri_referenced_error(&c.parent, &c.child, &c.fk, &c.key_vals));
            }
        }
        Ok(())
    }

    fn ri_on_delete(
        &mut self,
        parent_q: &QualifiedName,
        row: &RowValues,
        queue: &mut VecDeque<RiWork>,
        checks: &mut Vec<PendingCheck>,
    ) -> Result<()> {
        for (child_q, fk) in self.catalog.referencing_foreign_keys(parent_q) {
            let key_vals = ref_values(&fk, row);
            // A key with a NULL component cannot be referenced (MATCH SIMPLE).
            let Some(key) = composite_key(&key_vals) else {
                continue;
            };
            match fk.on_delete {
                ReferentialAction::NoAction | ReferentialAction::Restrict => {
                    checks.push(PendingCheck {
                        parent: parent_q.clone(),
                        child: child_q.clone(),
                        fk: fk.clone(),
                        key,
                        key_vals,
                    });
                }
                ReferentialAction::Cascade => {
                    for (rid, child_row) in self.fk_matching_children(&child_q, &fk, &key)? {
                        self.ri_delete_child(&child_q, &rid)?;
                        queue.push_back(RiWork::Deleted {
                            table: child_q.clone(),
                            row: child_row,
                        });
                    }
                }
                ReferentialAction::SetNull => {
                    for (rid, child_row) in self.fk_matching_children(&child_q, &fk, &key)? {
                        let mut new = child_row.clone();
                        for c in &fk.columns {
                            new.insert(c.clone(), SqlValue::Null);
                        }
                        self.ri_update_child(&child_q, &rid, child_row, new, queue)?;
                    }
                }
                ReferentialAction::SetDefault => {
                    for (rid, child_row) in self.fk_matching_children(&child_q, &fk, &key)? {
                        let new = self.ri_defaults_for(&child_q, &fk, &child_row)?;
                        self.ri_update_child(&child_q, &rid, child_row, new, queue)?;
                    }
                    // PostgreSQL re-runs the NO ACTION check after SET
                    // DEFAULT: when the default happens to equal the removed
                    // key, the update above is a no-op and would not
                    // re-validate, yet the reference is now dangling.
                    checks.push(PendingCheck {
                        parent: parent_q.clone(),
                        child: child_q.clone(),
                        fk: fk.clone(),
                        key,
                        key_vals,
                    });
                }
            }
        }
        Ok(())
    }

    fn ri_on_update(
        &mut self,
        parent_q: &QualifiedName,
        old: &RowValues,
        new: &RowValues,
        queue: &mut VecDeque<RiWork>,
        checks: &mut Vec<PendingCheck>,
    ) -> Result<()> {
        for (child_q, fk) in self.catalog.referencing_foreign_keys(parent_q) {
            let old_vals = ref_values(&fk, old);
            let Some(key) = composite_key(&old_vals) else {
                continue;
            };
            let new_vals = ref_values(&fk, new);
            // Actions fire only when a referenced column actually changed.
            if ordered_key(&old_vals) == ordered_key(&new_vals) {
                continue;
            }
            match fk.on_update {
                ReferentialAction::NoAction | ReferentialAction::Restrict => {
                    checks.push(PendingCheck {
                        parent: parent_q.clone(),
                        child: child_q.clone(),
                        fk: fk.clone(),
                        key,
                        key_vals: old_vals,
                    });
                }
                ReferentialAction::Cascade => {
                    let child_meta = self.fk_loaded(&child_q)?.meta.clone();
                    for (rid, child_row) in self.fk_matching_children(&child_q, &fk, &key)? {
                        let mut newc = child_row.clone();
                        for (i, c) in fk.columns.iter().enumerate() {
                            let v = new_vals.get(i).cloned().unwrap_or(SqlValue::Null);
                            let v = if v.is_null() {
                                SqlValue::Null
                            } else {
                                crate::sql::dml::coerce_to_col(v, &child_meta, c)?
                            };
                            newc.insert(c.clone(), v);
                        }
                        self.ri_update_child(&child_q, &rid, child_row, newc, queue)?;
                    }
                }
                ReferentialAction::SetNull => {
                    for (rid, child_row) in self.fk_matching_children(&child_q, &fk, &key)? {
                        let mut newc = child_row.clone();
                        for c in &fk.columns {
                            newc.insert(c.clone(), SqlValue::Null);
                        }
                        self.ri_update_child(&child_q, &rid, child_row, newc, queue)?;
                    }
                }
                ReferentialAction::SetDefault => {
                    for (rid, child_row) in self.fk_matching_children(&child_q, &fk, &key)? {
                        let newc = self.ri_defaults_for(&child_q, &fk, &child_row)?;
                        self.ri_update_child(&child_q, &rid, child_row, newc, queue)?;
                    }
                    // Same post-SET DEFAULT re-check as the delete path.
                    checks.push(PendingCheck {
                        parent: parent_q.clone(),
                        child: child_q.clone(),
                        fk: fk.clone(),
                        key,
                        key_vals: old_vals,
                    });
                }
            }
        }
        Ok(())
    }

    fn ri_referenced_error(
        &self,
        parent_q: &QualifiedName,
        child_q: &QualifiedName,
        fk: &ForeignKey,
        key_vals: &[SqlValue],
    ) -> SqlError {
        SqlError::ForeignKeyViolationReferenced {
            table: parent_q.name.clone(),
            constraint: fk.name.clone(),
            referencing: child_q.name.clone(),
            detail: format!(
                "Key ({})=({}) is still referenced from table \"{}\".",
                fk.ref_columns.join(", "),
                display_vals(key_vals),
                child_q.name
            ),
        }
    }

    /// Delete one child row through the normal write path (row lock, index
    /// maintenance, storage mutation). Deliberately not filtered by RLS.
    fn ri_delete_child(&mut self, child_q: &QualifiedName, rid: &str) -> Result<()> {
        let (collection, oid) = {
            let loaded = self.fk_loaded(child_q)?;
            (loaded.meta.storage_collection.clone(), loaded.meta.oid)
        };
        self.record_pending(
            crate::sql::lock::LockObject::Row(oid, rid.to_string()),
            crate::sql::lock::LockMode::ForUpdate,
            crate::sql::lock::LockScope::Transaction,
        );
        let loaded = self.tables.get_mut(child_q).unwrap();
        loaded.apply_delete(rid);
        self.mutations
            .lock()
            .unwrap()
            .push(crate::sql::store::Mutation::Delete {
                collection,
                row_id: rid.to_string(),
            });
        Ok(())
    }

    /// Apply an internal child-row update produced by a referential action:
    /// validates NOT NULL / CHECK / UNIQUE and the child's *own* foreign keys
    /// on the changed columns (this is how `SET DEFAULT` re-checks against the
    /// remaining parents — defaults that reference nothing fail 23503, all-NULL
    /// defaults pass), then writes through the normal update path and enqueues
    /// follow-up work. Deliberately not subject to RLS policy checks.
    fn ri_update_child(
        &mut self,
        child_q: &QualifiedName,
        rid: &str,
        old: RowValues,
        new: RowValues,
        queue: &mut VecDeque<RiWork>,
    ) -> Result<()> {
        let table = self.fk_loaded(child_q)?.meta.clone();
        // Fixpoint guard: an action that changes nothing produces no write and
        // no further work (this is what terminates ON UPDATE CASCADE chains).
        let all_cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
        let row_key = |r: &RowValues| {
            ordered_key(
                &all_cols
                    .iter()
                    .map(|c| r.get(c).cloned().unwrap_or(SqlValue::Null))
                    .collect::<Vec<_>>(),
            )
        };
        if row_key(&old) == row_key(&new) {
            return Ok(());
        }
        for c in &table.columns {
            if !c.nullable && new.get(&c.name).map(SqlValue::is_null).unwrap_or(true) {
                return Err(SqlError::NotNullViolation {
                    column: c.name.clone(),
                    table: table.name.clone(),
                });
            }
        }
        self.check_constraints(&table, &new)?;
        self.check_unique_for(child_q, &new, Some(rid))?;
        self.fk_check_child(&table, &new, Some(&old))?;
        self.record_pending(
            crate::sql::lock::LockObject::Row(table.oid, rid.to_string()),
            crate::sql::lock::LockMode::ForUpdate,
            crate::sql::lock::LockScope::Transaction,
        );
        let collection = table.storage_collection.clone();
        self.write_update(child_q, &collection, &table, rid, new.clone())?;
        queue.push_back(RiWork::Updated {
            table: child_q.clone(),
            old,
            new,
        });
        Ok(())
    }

    /// The child row with the FK columns replaced by their column defaults
    /// (NULL when a column has no default), mirroring INSERT's default
    /// evaluation, serial sequences included.
    fn ri_defaults_for(
        &mut self,
        child_q: &QualifiedName,
        fk: &ForeignKey,
        row: &RowValues,
    ) -> Result<RowValues> {
        let table = self.fk_loaded(child_q)?.meta.clone();
        let mut new = row.clone();
        for cname in &fk.columns {
            let col = table
                .column(cname)
                .ok_or_else(|| SqlError::UndefinedColumn(cname.clone()))?;
            let value = if let Some(seq) = &col.identity_sequence {
                let n = self.catalog.next_sequence_value(&table.schema, seq)?;
                self.catalog_dirty = true;
                crate::sql::dml::coerce_to_col(SqlValue::Int8(n), &table, cname)?
            } else if let Some(def) = &col.default {
                let v = self.eval_default(def)?;
                if v.is_null() {
                    SqlValue::Null
                } else {
                    crate::sql::dml::coerce_to_col(v, &table, cname)?
                }
            } else {
                SqlValue::Null
            };
            new.insert(cname.clone(), value);
        }
        Ok(new)
    }
}
