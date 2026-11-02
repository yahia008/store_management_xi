//! Append-only audit trail (the database refuses UPDATE and DELETE on `audit_log`).

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

#[derive(Default)]
pub struct Entry<'a> {
    pub business_id: Option<Uuid>,
    pub store_id: Option<Uuid>,
    pub actor: Option<Uuid>,
    pub action: &'a str,
    pub entity_type: &'a str,
    pub entity_id: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub outcome: Option<&'a str>,
}

pub async fn record<'e>(db: impl PgExecutor<'e>, e: Entry<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log (business_id, store_id, actor_user_id, action, entity_type, entity_id, before_values, after_values, outcome)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(e.business_id)
    .bind(e.store_id)
    .bind(e.actor)
    .bind(e.action)
    .bind(e.entity_type)
    .bind(e.entity_id)
    .bind(e.before)
    .bind(e.after)
    .bind(e.outcome.unwrap_or("success"))
    .execute(db)
    .await?;
    Ok(())
}

/// Record a refused action. Uses the pool (not a transaction) so it survives the request failing.
pub async fn denied(pool: &PgPool, business_id: Uuid, actor: Uuid, store_id: Option<Uuid>, action: &str) {
    let _ = record(
        pool,
        Entry { business_id: Some(business_id), store_id, actor: Some(actor), action, entity_type: "access", outcome: Some("denied"), ..Default::default() },
    )
    .await;
}

pub fn json<T: Serialize>(v: &T) -> Option<Value> {
    serde_json::to_value(v).ok()
}
