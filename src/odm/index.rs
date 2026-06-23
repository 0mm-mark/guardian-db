use crate::odm::error::{OdmError, Result};
use crate::odm::query::value_at_path;
use crate::odm::schema::ModelSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub field: String,
    pub primary: bool,
    pub unique: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IndexCatalog {
    unique: BTreeMap<String, BTreeMap<String, String>>,
    secondary: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
}

impl IndexCatalog {
    pub(crate) fn rebuild(
        schema: &ModelSchema,
        documents: &BTreeMap<String, Value>,
    ) -> Result<Self> {
        let mut catalog = Self::default();
        for field in schema.unique_fields() {
            catalog.unique.insert(field, BTreeMap::new());
        }
        for field in schema.indexed_fields() {
            catalog.secondary.insert(field, BTreeMap::new());
        }

        for (id, document) in documents {
            for (field, values) in &mut catalog.unique {
                let Some(value) = value_at_path(document, field) else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                for token in index_tokens(value)? {
                    if let Some(existing) = values.insert(token.clone(), id.clone())
                        && existing != *id
                    {
                        return Err(OdmError::DuplicateKey {
                            field: field.clone(),
                            value: token,
                        });
                    }
                }
            }

            for (field, values) in &mut catalog.secondary {
                let Some(value) = value_at_path(document, field) else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                for token in index_tokens(value)? {
                    values.entry(token).or_default().insert(id.clone());
                }
            }
        }
        Ok(catalog)
    }

    pub(crate) fn candidates(&self, query: &Value) -> Result<Option<BTreeSet<String>>> {
        let Some(query) = query.as_object() else {
            return Ok(None);
        };

        let mut candidates: Option<BTreeSet<String>> = None;
        for (field, condition) in query {
            if field.starts_with('$') {
                continue;
            }
            let Some(index) = self.secondary.get(field) else {
                continue;
            };
            let equality = if let Some(object) = condition.as_object() {
                if object.len() == 1 {
                    object.get("$eq")
                } else {
                    None
                }
            } else {
                Some(condition)
            };
            let Some(equality) = equality else {
                continue;
            };
            // Null values are intentionally omitted from indexes so multiple
            // nullable unique fields can coexist. A null equality therefore
            // requires a full scan rather than an empty indexed candidate set.
            if equality.is_null() {
                continue;
            }
            let token = index_token(equality)?;
            let matches = index.get(&token).cloned().unwrap_or_default();
            candidates = Some(match candidates {
                Some(existing) => existing.intersection(&matches).cloned().collect(),
                None => matches,
            });
        }
        Ok(candidates)
    }

    pub(crate) fn metadata(schema: &ModelSchema) -> Vec<IndexMetadata> {
        schema
            .indexed_fields()
            .into_iter()
            .map(|field| IndexMetadata {
                primary: field == schema.primary_key(),
                unique: schema.unique_fields().contains(&field),
                field,
            })
            .collect()
    }
}

fn index_token(value: &Value) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

fn index_tokens(value: &Value) -> Result<BTreeSet<String>> {
    let mut tokens = BTreeSet::from([index_token(value)?]);
    if let Value::Array(items) = value {
        for item in items {
            tokens.insert(index_token(item)?);
        }
    }
    Ok(tokens)
}
