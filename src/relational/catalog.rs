//! The relational catalog: schemas, tables, columns, constraints, indexes,
//! sequences and views.
//!
//! The catalog is the authoritative, serializable description of the relational
//! schema. It is persisted as a single JSON document in GuardianDB's reserved
//! `__gdb_sql_catalog` collection and snapshotted for transaction isolation.

use crate::relational::error::{RelError, Result};
use crate::relational::types::SqlType;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};

/// First OID handed out to user objects (mirrors PostgreSQL's `FirstNormalObjectId`).
pub const FIRST_USER_OID: u32 = 16384;

/// A `(schema, name)` key used throughout the catalog.
///
/// Because it is used as a `BTreeMap` key and serialized to JSON (where map keys
/// must be strings), it (de)serializes to a `"schema\u{1f}name"` string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualifiedName {
    pub schema: String,
    pub name: String,
}

impl Serialize for QualifiedName {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}\u{1f}{}", self.schema, self.name))
    }
}

impl<'de> Deserialize<'de> for QualifiedName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.split_once('\u{1f}') {
            Some((schema, name)) => Ok(QualifiedName::new(schema, name)),
            None => Err(D::Error::custom("malformed qualified name key")),
        }
    }
}

impl QualifiedName {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    pub fn to_string_qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub oid: u32,
    pub owner: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
    /// Raw SQL text of the DEFAULT expression, if any.
    pub default: Option<String>,
    /// Name of the backing sequence when the column is `serial`/`bigserial`.
    pub identity_sequence: Option<String>,
    pub ordinal: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimaryKey {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferentialAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl ReferentialAction {
    pub fn as_sql(&self) -> &'static str {
        match self {
            ReferentialAction::NoAction => "NO ACTION",
            ReferentialAction::Restrict => "RESTRICT",
            ReferentialAction::Cascade => "CASCADE",
            ReferentialAction::SetNull => "SET NULL",
            ReferentialAction::SetDefault => "SET DEFAULT",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: ReferentialAction,
    pub on_update: ReferentialAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: String,
    /// Raw SQL text of the CHECK expression.
    pub expr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Option<PrimaryKey>,
    pub uniques: Vec<UniqueConstraint>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<CheckConstraint>,
    /// Opaque storage collection name for this table's rows.
    pub storage_collection: String,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn column_mut(&mut self, name: &str) -> Option<&mut Column> {
        self.columns.iter_mut().find(|c| c.name == name)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn qualified(&self) -> QualifiedName {
        QualifiedName::new(self.schema.clone(), self.name.clone())
    }

    /// The columns that make up the primary key, or empty if none.
    pub fn pk_columns(&self) -> Vec<String> {
        self.primary_key
            .as_ref()
            .map(|pk| pk.columns.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub oid: u32,
    pub name: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sequence {
    pub schema: String,
    pub name: String,
    pub current: i64,
    pub increment: i64,
    pub start: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    /// The SQL text of the SELECT defining the view.
    pub query: String,
    pub columns: Vec<String>,
}

/// The authoritative, serializable relational catalog.
///
/// Tables and views are stored in flat `Vec`s. Three `HashMap` indexes provide
/// O(1) name-based lookup so that every `get_table` / `get_view` / `find_*`
/// call avoids a linear scan.
///
/// Index fields are skipped during serialization and rebuilt transparently on
/// deserialization via the custom `Deserialize` impl.
#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    pub database: String,
    schemas: BTreeMap<String, Schema>,
    /// Backing store for tables — append-only with swap-remove for deletion.
    tables: Vec<Table>,
    /// O(1) lookup: `"schema.name"` → index into `tables`.
    #[serde(skip)]
    table_idx: HashMap<String, usize>,
    indexes: BTreeMap<QualifiedName, Index>,
    sequences: BTreeMap<QualifiedName, Sequence>,
    /// Backing store for views — append-only with swap-remove for deletion.
    views: Vec<View>,
    /// O(1) lookup: `"schema.name"` → index into `views`.
    #[serde(skip)]
    view_idx: HashMap<String, usize>,
    /// O(1) lookup: `(schema, name, arity)` → index into a future functions vec.
    #[serde(skip)]
    func_idx: HashMap<(String, String, u8), usize>,
    next_oid: u32,
    pub search_path: Vec<String>,
}

// ---------------------------------------------------------------------------
// Custom Deserialize: reconstruct the catalog from its serialized form and
// immediately rebuild all HashMap indexes so the catalog is ready to use.
// ---------------------------------------------------------------------------

/// A plain, derived-Deserialize mirror of `Catalog` used as a deserialization
/// intermediary so that the index HashMaps can be populated after loading.
#[derive(Deserialize)]
struct CatalogHelper {
    database: String,
    schemas: BTreeMap<String, Schema>,
    tables: Vec<Table>,
    indexes: BTreeMap<QualifiedName, Index>,
    sequences: BTreeMap<QualifiedName, Sequence>,
    views: Vec<View>,
    next_oid: u32,
    search_path: Vec<String>,
}

impl<'de> Deserialize<'de> for Catalog {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let h = CatalogHelper::deserialize(deserializer)?;
        let mut catalog = Catalog {
            database: h.database,
            schemas: h.schemas,
            tables: h.tables,
            table_idx: HashMap::new(),
            indexes: h.indexes,
            sequences: h.sequences,
            views: h.views,
            view_idx: HashMap::new(),
            func_idx: HashMap::new(),
            next_oid: h.next_oid,
            search_path: h.search_path,
        };
        catalog.rebuild_indexes();
        Ok(catalog)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Inline key format used by all three HashMap indexes.
#[inline]
fn table_key(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

impl Catalog {
    /// A fresh catalog containing only the `public` and system schemas.
    pub fn new(database: impl Into<String>) -> Self {
        let mut catalog = Self {
            database: database.into(),
            schemas: BTreeMap::new(),
            tables: Vec::new(),
            table_idx: HashMap::new(),
            indexes: BTreeMap::new(),
            sequences: BTreeMap::new(),
            views: Vec::new(),
            view_idx: HashMap::new(),
            func_idx: HashMap::new(),
            next_oid: FIRST_USER_OID,
            search_path: vec!["public".to_string()],
        };
        // System schemas always present.
        for sys in ["pg_catalog", "information_schema"] {
            let oid = catalog.allocate_oid();
            catalog.schemas.insert(
                sys.to_string(),
                Schema {
                    name: sys.to_string(),
                    oid,
                    owner: "guardian".into(),
                },
            );
        }
        let oid = catalog.allocate_oid();
        catalog.schemas.insert(
            "public".to_string(),
            Schema {
                name: "public".into(),
                oid,
                owner: "guardian".into(),
            },
        );
        catalog
    }

    /// Rebuild all three HashMap indexes from the backing `Vec`s.
    ///
    /// Call this after any bulk mutation (e.g. deserialization). Individual
    /// mutation methods (`insert_table`, `drop_table_qualified`, …) keep the
    /// indexes in sync incrementally so a full rebuild is rarely needed outside
    /// of construction.
    pub fn rebuild_indexes(&mut self) {
        self.table_idx.clear();
        for (i, t) in self.tables.iter().enumerate() {
            self.table_idx.insert(table_key(&t.schema, &t.name), i);
        }
        self.view_idx.clear();
        for (i, v) in self.views.iter().enumerate() {
            self.view_idx.insert(table_key(&v.schema, &v.name), i);
        }
        self.func_idx.clear();
        // No functions are stored in the catalog at this time.
    }

    pub fn allocate_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    // ---- schemas -------------------------------------------------------

    pub fn has_schema(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }

    pub fn schemas(&self) -> impl Iterator<Item = &Schema> {
        self.schemas.values()
    }

    pub fn create_schema(&mut self, name: &str, if_not_exists: bool) -> Result<()> {
        if self.schemas.contains_key(name) {
            if if_not_exists {
                return Ok(());
            }
            return Err(RelError::DuplicateSchema(name.to_string()));
        }
        let oid = self.allocate_oid();
        self.schemas.insert(
            name.to_string(),
            Schema {
                name: name.to_string(),
                oid,
                owner: "guardian".into(),
            },
        );
        Ok(())
    }

    pub fn drop_schema(&mut self, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
        if !self.schemas.contains_key(name) {
            if if_exists {
                return Ok(());
            }
            return Err(RelError::UndefinedSchema(name.to_string()));
        }
        let table_names: Vec<QualifiedName> = self
            .tables
            .iter()
            .filter(|t| t.schema == name)
            .map(|t| t.qualified())
            .collect();
        if !table_names.is_empty() && !cascade {
            return Err(RelError::FeatureNotSupported(format!(
                "cannot drop schema {name} because it contains objects (use CASCADE)"
            )));
        }
        for t in table_names {
            self.drop_table_qualified(&t)?;
        }
        self.schemas.remove(name);
        Ok(())
    }

    // ---- resolution ----------------------------------------------------

    /// Resolve a possibly-unqualified table name using the search path.
    pub fn resolve_table_name(&self, schema: Option<&str>, name: &str) -> Option<QualifiedName> {
        if let Some(schema) = schema {
            let key = table_key(schema, name);
            if self.table_idx.contains_key(&key) || self.view_idx.contains_key(&key) {
                return Some(QualifiedName::new(schema, name));
            }
            return None;
        }
        for schema in &self.search_path {
            let key = table_key(schema, name);
            if self.table_idx.contains_key(&key) || self.view_idx.contains_key(&key) {
                return Some(QualifiedName::new(schema.clone(), name));
            }
        }
        None
    }

    /// The schema an unqualified, to-be-created object should live in.
    pub fn creation_schema(&self, schema: Option<&str>) -> Result<String> {
        match schema {
            Some(s) => {
                if !self.schemas.contains_key(s) {
                    return Err(RelError::UndefinedSchema(s.to_string()));
                }
                Ok(s.to_string())
            }
            None => Ok(self
                .search_path
                .first()
                .cloned()
                .unwrap_or_else(|| "public".to_string())),
        }
    }

    // ---- tables --------------------------------------------------------

    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.iter()
    }

    /// O(1) table lookup via the `table_idx` HashMap.
    pub fn get_table(&self, q: &QualifiedName) -> Option<&Table> {
        let key = table_key(&q.schema, &q.name);
        self.table_idx.get(&key).map(|&i| &self.tables[i])
    }

    /// O(1) mutable table lookup via the `table_idx` HashMap.
    pub fn get_table_mut(&mut self, q: &QualifiedName) -> Option<&mut Table> {
        let key = table_key(&q.schema, &q.name);
        let i = *self.table_idx.get(&key)?;
        Some(&mut self.tables[i])
    }

    /// O(1) table lookup; returns an error if the table is absent.
    pub fn require_table(&self, q: &QualifiedName) -> Result<&Table> {
        self.get_table(q)
            .ok_or_else(|| RelError::UndefinedTable(q.to_string_qualified()))
    }

    /// O(1) existence check via `table_idx`.
    pub fn has_table(&self, q: &QualifiedName) -> bool {
        let key = table_key(&q.schema, &q.name);
        self.table_idx.contains_key(&key)
    }

    /// Register a new table. The storage collection name is derived from the oid.
    pub fn insert_table(&mut self, mut table: Table) -> Result<()> {
        let q = table.qualified();
        let key = table_key(&q.schema, &q.name);
        if self.table_idx.contains_key(&key) || self.view_idx.contains_key(&key) {
            return Err(RelError::DuplicateTable(q.to_string_qualified()));
        }
        if !self.schemas.contains_key(&table.schema) {
            return Err(RelError::UndefinedSchema(table.schema.clone()));
        }
        if table.storage_collection.is_empty() {
            table.storage_collection = format!("__gdb_sql_rows_{}", table.oid);
        }
        let idx = self.tables.len();
        self.tables.push(table);
        self.table_idx.insert(key, idx);
        Ok(())
    }

    pub fn drop_table_qualified(&mut self, q: &QualifiedName) -> Result<Table> {
        let key = table_key(&q.schema, &q.name);
        let idx = self
            .table_idx
            .remove(&key)
            .ok_or_else(|| RelError::UndefinedTable(q.to_string_qualified()))?;

        // O(1) removal: swap the target with the last element, pop, then fix up
        // the index entry for the element that moved into position `idx`.
        let last_idx = self.tables.len() - 1;
        if idx != last_idx {
            self.tables.swap(idx, last_idx);
            let swapped_key = table_key(&self.tables[idx].schema, &self.tables[idx].name);
            self.table_idx.insert(swapped_key, idx);
        }
        let table = self.tables.pop().unwrap();

        // Drop dependent indexes and sequences.
        let idx_keys: Vec<QualifiedName> = self
            .indexes
            .iter()
            .filter(|(_, i)| i.schema == q.schema && i.table == q.name)
            .map(|(k, _)| k.clone())
            .collect();
        for k in idx_keys {
            self.indexes.remove(&k);
        }
        for col in &table.columns {
            if let Some(seq) = &col.identity_sequence {
                let sk = QualifiedName::new(q.schema.clone(), seq.clone());
                self.sequences.remove(&sk);
            }
        }
        Ok(table)
    }

    // ---- indexes -------------------------------------------------------

    pub fn indexes(&self) -> impl Iterator<Item = &Index> {
        self.indexes.values()
    }

    pub fn indexes_for_table(&self, schema: &str, table: &str) -> Vec<&Index> {
        self.indexes
            .values()
            .filter(|i| i.schema == schema && i.table == table)
            .collect()
    }

    pub fn get_index(&self, q: &QualifiedName) -> Option<&Index> {
        self.indexes.get(q)
    }

    pub fn insert_index(&mut self, index: Index) -> Result<()> {
        let q = QualifiedName::new(index.schema.clone(), index.name.clone());
        if self.indexes.contains_key(&q) {
            return Err(RelError::DuplicateIndex(q.to_string_qualified()));
        }
        self.indexes.insert(q, index);
        Ok(())
    }

    pub fn drop_index(&mut self, schema: Option<&str>, name: &str, if_exists: bool) -> Result<()> {
        let q = match schema {
            Some(s) => QualifiedName::new(s, name),
            None => {
                // Search path lookup for the index name.
                let found = self
                    .search_path
                    .iter()
                    .map(|s| QualifiedName::new(s.clone(), name))
                    .find(|q| self.indexes.contains_key(q));
                match found {
                    Some(q) => q,
                    None => {
                        if if_exists {
                            return Ok(());
                        }
                        return Err(RelError::UndefinedIndex(name.to_string()));
                    }
                }
            }
        };
        if self.indexes.remove(&q).is_none() && !if_exists {
            return Err(RelError::UndefinedIndex(q.to_string_qualified()));
        }
        Ok(())
    }

    // ---- sequences -----------------------------------------------------

    pub fn sequences(&self) -> impl Iterator<Item = &Sequence> {
        self.sequences.values()
    }

    pub fn create_sequence(&mut self, schema: &str, name: &str) -> Result<()> {
        let q = QualifiedName::new(schema, name);
        self.sequences.entry(q).or_insert(Sequence {
            schema: schema.to_string(),
            name: name.to_string(),
            current: 0,
            increment: 1,
            start: 1,
        });
        Ok(())
    }

    /// Advance a sequence and return the next value.
    pub fn next_sequence_value(&mut self, schema: &str, name: &str) -> Result<i64> {
        let q = QualifiedName::new(schema, name);
        let seq = self
            .sequences
            .get_mut(&q)
            .ok_or_else(|| RelError::UndefinedObject(format!("sequence {schema}.{name}")))?;
        let next = if seq.current == 0 {
            seq.start
        } else {
            seq.current + seq.increment
        };
        seq.current = next;
        Ok(next)
    }

    /// Ensure a sequence's current value is at least `value` (used after explicit inserts).
    pub fn observe_sequence_value(&mut self, schema: &str, name: &str, value: i64) {
        let q = QualifiedName::new(schema, name);
        if let Some(seq) = self.sequences.get_mut(&q)
            && value > seq.current
        {
            seq.current = value;
        }
    }

    // ---- views ---------------------------------------------------------

    pub fn views(&self) -> impl Iterator<Item = &View> {
        self.views.iter()
    }

    /// O(1) view lookup via the `view_idx` HashMap.
    pub fn get_view(&self, q: &QualifiedName) -> Option<&View> {
        let key = table_key(&q.schema, &q.name);
        self.view_idx.get(&key).map(|&i| &self.views[i])
    }

    pub fn insert_view(&mut self, view: View) -> Result<()> {
        let q = QualifiedName::new(view.schema.clone(), view.name.clone());
        let key = table_key(&q.schema, &q.name);
        if self.table_idx.contains_key(&key) || self.view_idx.contains_key(&key) {
            return Err(RelError::DuplicateTable(q.to_string_qualified()));
        }
        let idx = self.views.len();
        self.views.push(view);
        self.view_idx.insert(key, idx);
        Ok(())
    }

    pub fn drop_view(&mut self, q: &QualifiedName, if_exists: bool) -> Result<()> {
        let key = table_key(&q.schema, &q.name);
        match self.view_idx.remove(&key) {
            None if !if_exists => return Err(RelError::UndefinedTable(q.to_string_qualified())),
            None => return Ok(()),
            Some(idx) => {
                let last_idx = self.views.len() - 1;
                if idx != last_idx {
                    self.views.swap(idx, last_idx);
                    let swapped_key =
                        table_key(&self.views[idx].schema, &self.views[idx].name);
                    self.view_idx.insert(swapped_key, idx);
                }
                self.views.pop();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_table(cat: &mut Catalog) -> Table {
        let oid = cat.allocate_oid();
        Table {
            oid,
            schema: "public".into(),
            name: "users".into(),
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: SqlType::Integer,
                    nullable: false,
                    default: None,
                    identity_sequence: None,
                    ordinal: 0,
                },
                Column {
                    name: "email".into(),
                    ty: SqlType::Text,
                    nullable: false,
                    default: None,
                    identity_sequence: None,
                    ordinal: 1,
                },
            ],
            primary_key: Some(PrimaryKey {
                name: "users_pkey".into(),
                columns: vec!["id".into()],
            }),
            uniques: vec![],
            foreign_keys: vec![],
            checks: vec![],
            storage_collection: String::new(),
        }
    }

    #[test]
    fn create_and_resolve_table() {
        let mut cat = Catalog::new("app");
        let t = sample_table(&mut cat);
        cat.insert_table(t).unwrap();
        let q = cat.resolve_table_name(None, "users").unwrap();
        assert_eq!(q.schema, "public");
        assert!(
            cat.get_table(&q)
                .unwrap()
                .storage_collection
                .starts_with("__gdb_sql_rows_")
        );
    }

    #[test]
    fn duplicate_table_errors() {
        let mut cat = Catalog::new("app");
        let t = sample_table(&mut cat);
        cat.insert_table(t.clone()).unwrap();
        let t2 = sample_table(&mut cat);
        assert!(matches!(
            cat.insert_table(t2),
            Err(RelError::DuplicateTable(_))
        ));
    }

    #[test]
    fn sequence_advances() {
        let mut cat = Catalog::new("app");
        cat.create_sequence("public", "users_id_seq").unwrap();
        assert_eq!(
            cat.next_sequence_value("public", "users_id_seq").unwrap(),
            1
        );
        assert_eq!(
            cat.next_sequence_value("public", "users_id_seq").unwrap(),
            2
        );
        cat.observe_sequence_value("public", "users_id_seq", 10);
        assert_eq!(
            cat.next_sequence_value("public", "users_id_seq").unwrap(),
            11
        );
    }

    #[test]
    fn drop_schema_requires_cascade() {
        let mut cat = Catalog::new("app");
        cat.create_schema("app", false).unwrap();
        let oid = cat.allocate_oid();
        let mut t = sample_table(&mut cat);
        t.schema = "app".into();
        t.oid = oid;
        cat.insert_table(t).unwrap();
        assert!(cat.drop_schema("app", false, false).is_err());
        assert!(cat.drop_schema("app", false, true).is_ok());
    }

    #[test]
    fn catalog_round_trips_json() {
        let mut cat = Catalog::new("app");
        let t = sample_table(&mut cat);
        cat.insert_table(t).unwrap();
        let json = serde_json::to_value(&cat).unwrap();
        let back: Catalog = serde_json::from_value(json).unwrap();
        assert!(back.resolve_table_name(None, "users").is_some());
    }
}
