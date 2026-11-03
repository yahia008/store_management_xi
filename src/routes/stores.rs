//! Stores and store-level settings (FR-02, FR-03).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::{Principal, Role};
use crate::state::AppState;

#[derive(Serialize, FromRow)]
pub struct Store {
    id: Uuid,
    name: String,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    status: String,
}

#[derive(Serialize, FromRow)]
pub struct StoreSettings {
    receipt_prefix: String,
    transaction_prefix: String,
    next_receipt_number: i64,
    next_transaction_number: i64,
}

const COLS: &str = "id, name, address, phone, email, status";

fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[derive(Deserialize)]
pub struct StoreSearch {
    q: Option<String>,
}

/// Lists the stores the caller may see; `?q=` matches part of the name or the exact store identifier.
pub async fn list(State(state): State<AppState>, p: Principal, axum::extract::Query(s): axum::extract::Query<StoreSearch>) -> ApiResult<Json<Vec<Store>>> {
    let like = s.q.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(|v| format!("%{}%", v.replace('%', "\\%").replace('_', "\\_")));
    let exact = s.q.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let filter = "AND ($3::text IS NULL OR name ILIKE $3 OR id::text = $4)";
    let stores = match p.visible_store_ids() {
        None => sqlx::query_as::<_, Store>(&format!("SELECT {COLS} FROM stores WHERE business_id = $1 AND ($2::uuid[] IS NULL OR id = ANY($2)) {filter} ORDER BY name")).bind(p.business_id).bind(None::<Vec<Uuid>>).bind(&like).bind(&exact).fetch_all(&state.pool).await?,
        Some(ids) => sqlx::query_as::<_, Store>(&format!("SELECT {COLS} FROM stores WHERE business_id = $1 AND ($2::uuid[] IS NULL OR id = ANY($2)) {filter} ORDER BY name")).bind(p.business_id).bind(Some(ids)).bind(&like).bind(&exact).fetch_all(&state.pool).await?,
    };
    Ok(Json(stores))
}

#[derive(Deserialize)]
pub struct CreateStore {
    name: String,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
}

pub async fn create(State(state): State<AppState>, p: Principal, Json(b): Json<CreateStore>) -> ApiResult<(StatusCode, Json<Store>)> {
    p.require_business_admin(&state, "create_store").await?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad("store name is required"));
    }
    let mut tx = state.pool.begin().await?;
    let store = sqlx::query_as::<_, Store>(&format!("INSERT INTO stores (business_id, name, address, phone, email, updated_by) VALUES ($1,$2,$3,$4,$5,$6) RETURNING {COLS}"))
        .bind(p.business_id)
        .bind(b.name.trim())
        .bind(clean(b.address))
        .bind(clean(b.phone))
        .bind(clean(b.email))
        .bind(p.user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match ApiError::from(e) {
            ApiError::Conflict(_) => ApiError::conflict("a store with that name already exists"),
            other => other,
        })?;
    sqlx::query("INSERT INTO store_settings (store_id, updated_by) VALUES ($1,$2)").bind(store.id).bind(p.user_id).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store.id), actor: Some(p.user_id), action: "create_store", entity_type: "store", entity_id: Some(store.id.to_string()), after: audit::json(&store), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(store)))
}

const ANY_STORE_ROLE: [Role; 4] = [Role::StoreManager, Role::Cashier, Role::InventoryOfficer, Role::FinanceOfficer];

pub async fn get(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Store>> {
    p.authorize_store(&state, id, &ANY_STORE_ROLE, "view_store").await?;
    Ok(Json(sqlx::query_as::<_, Store>(&format!("SELECT {COLS} FROM stores WHERE id = $1")).bind(id).fetch_one(&state.pool).await?))
}

#[derive(Deserialize)]
pub struct UpdateStore {
    name: Option<String>,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    status: Option<String>,
}

/// Contact details can be changed by the store's manager; opening or closing a store, and renaming it,
/// are business-level decisions.
pub async fn update(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<UpdateStore>) -> ApiResult<Json<Store>> {
    p.authorize_store(&state, id, &[Role::StoreManager], "update_store").await?;
    if (b.name.is_some() || b.status.is_some()) && !p.is_business_wide() {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(id), "update_store_name_or_status").await;
        return Err(ApiError::Forbidden);
    }
    if b.status.as_deref().is_some_and(|s| s != "active" && s != "inactive") {
        return Err(ApiError::bad("status must be active or inactive"));
    }
    if b.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad("store name cannot be empty"));
    }
    let mut tx = state.pool.begin().await?;
    let before = sqlx::query_as::<_, Store>(&format!("SELECT {COLS} FROM stores WHERE id = $1 FOR UPDATE")).bind(id).fetch_one(&mut *tx).await?;
    let after = sqlx::query_as::<_, Store>(&format!(
        "UPDATE stores SET name = COALESCE($2, name), address = COALESCE($3, address), phone = COALESCE($4, phone), email = COALESCE($5, email),
             status = COALESCE($6, status), updated_at = now(), updated_by = $7 WHERE id = $1 RETURNING {COLS}"
    ))
    .bind(id)
    .bind(clean(b.name))
    .bind(clean(b.address))
    .bind(clean(b.phone))
    .bind(clean(b.email))
    .bind(b.status)
    .bind(p.user_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match ApiError::from(e) {
        ApiError::Conflict(_) => ApiError::conflict("a store with that name already exists"),
        other => other,
    })?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(id), actor: Some(p.user_id), action: "update_store", entity_type: "store", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}

pub async fn get_settings(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<StoreSettings>> {
    p.authorize_store(&state, id, &ANY_STORE_ROLE, "view_store_settings").await?;
    Ok(Json(sqlx::query_as::<_, StoreSettings>("SELECT receipt_prefix, transaction_prefix, next_receipt_number, next_transaction_number FROM store_settings WHERE store_id = $1").bind(id).fetch_one(&state.pool).await?))
}

#[derive(Deserialize)]
pub struct PutStoreSettings {
    receipt_prefix: Option<String>,
    transaction_prefix: Option<String>,
}

/// Changes the numbering prefixes. The counters themselves only move forward, inside the transaction that
/// issues a number, so a number is never reused.
pub async fn put_settings(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<PutStoreSettings>) -> ApiResult<Json<StoreSettings>> {
    p.authorize_store(&state, id, &[Role::StoreManager], "update_store_settings").await?;
    for prefix in [&b.receipt_prefix, &b.transaction_prefix].into_iter().flatten() {
        if prefix.is_empty() || prefix.len() > 10 || !prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-') {
            return Err(ApiError::bad("prefixes use 1 to 10 capital letters, digits or dashes"));
        }
    }
    let cols = "receipt_prefix, transaction_prefix, next_receipt_number, next_transaction_number";
    let mut tx = state.pool.begin().await?;
    let before = sqlx::query_as::<_, StoreSettings>(&format!("SELECT {cols} FROM store_settings WHERE store_id = $1 FOR UPDATE")).bind(id).fetch_one(&mut *tx).await?;
    let after = sqlx::query_as::<_, StoreSettings>(&format!(
        "UPDATE store_settings SET receipt_prefix = COALESCE($2, receipt_prefix), transaction_prefix = COALESCE($3, transaction_prefix), updated_at = now(), updated_by = $4
         WHERE store_id = $1 RETURNING {cols}"
    ))
    .bind(id)
    .bind(b.receipt_prefix)
    .bind(b.transaction_prefix)
    .bind(p.user_id)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(id), actor: Some(p.user_id), action: "update_store_settings", entity_type: "store_settings", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}
