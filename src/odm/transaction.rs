use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Consistency intent attached to an ODM operation.
///
/// `LocalAtomic` is implemented today: validation, index maintenance, and the
/// write are serialized within one collection instance. `Replicated` reserves
/// the API shape for a future distributed transaction coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencyLevel {
    #[default]
    LocalAtomic,
    Replicated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionContext {
    pub id: Uuid,
    pub started_at: DateTime<Utc>,
    pub consistency: ConsistencyLevel,
}

impl TransactionContext {
    pub fn local() -> Self {
        Self {
            id: Uuid::new_v4(),
            started_at: Utc::now(),
            consistency: ConsistencyLevel::LocalAtomic,
        }
    }

    pub fn with_consistency(consistency: ConsistencyLevel) -> Self {
        Self {
            consistency,
            ..Self::local()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    pub transaction: Option<TransactionContext>,
}
