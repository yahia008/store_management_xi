//! Customer records (FR-19). Customers belong to the business, so any store can serve them; who may see
//! or change them is limited to people who serve customers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, Row};
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::{Principal, Role};
use crate::state::AppState;

const SERVE: [Role; 2] = [Role::Cashier, Role::StoreManager];

#[derive(Serialize, FromRow)]
pub struct Customer {
    id: Uuid,
    name: String,
    phone: Option<String>,
    email: Option<String>,
    notes: Option<String>,
    is_active: bool,
    created_at: DateTime<Utc>,
}

const COLS: &str = "id, name, phone, email, notes, is_active, created_at";

fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Phone numbers are compared without spaces or dashes, so "0803 123-4567" and "08031234567" match.
fn phone(s: Option<String>) -> ApiResult<Option<String>> {
    let Some(raw) = clean(s) else { return Ok(None) };
    let p: String = raw.chars().filter(|c| !matches!(c, ' ' | '-' | '(' | ')')).collect();
    let digits = p.chars().filter(char::is_ascii_digit).count();
    if digits < 7 || p.len() > 20 || !p.chars().all(|c| c.is_ascii_digit() || c == '+') {
        return Err(ApiError::bad("enter a valid phone number"));
    }
    Ok(Some(p))
}

fn email(s: Option<String>) -> ApiResult<Option<String>> {
    clean(s).map(|e| crate::security::normalize_email(&e)).transpose()
}

async fn require_server(state: &AppState, p: &Principal, action: &str) -> ApiResult<()> {
    if p.has_any_role(&SERVE) {
        return Ok(());
    }
    audit::denied(&state.pool, p.business_id, p.user_id, None, action).await;
    Err(ApiError::Forbidden)
}

#[derive(Deserialize)]
pub struct CreateBody {
    name: String,
    phone: Option<String>,
    email: Option<String>,
    notes: Option<String>,
}

pub async fn create(State(state): State<AppState>, p: Principal, Json(b): Json<CreateBody>) -> ApiResult<(StatusCode, Json<Customer>)> {
    require_server(&state, &p, "create_customer").await?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad("customer name is required"));
    }
    let mut tx = state.pool.begin().await?;
    let c = sqlx::query_as::<_, Customer>(&format!("INSERT INTO customers (business_id, name, phone, email, notes, created_by) VALUES ($1,$2,$3,$4,$5,$6) RETURNING {COLS}"))
        .bind(p.business_id)
        .bind(b.name.trim())
        .bind(phone(b.phone)?)
        .bind(email(b.email)?)
        .bind(clean(b.notes))
        .bind(p.user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match ApiError::from(e) {
            ApiError::Conflict(_) => ApiError::conflict("a customer with that phone number already exists"),
            other => other,
        })?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "create_customer", entity_type: "customer", entity_id: Some(c.id.to_string()), after: audit::json(&c), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(c)))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
}

pub async fn list(State(state): State<AppState>, p: Principal, Query(q): Query<SearchQuery>) -> ApiResult<Json<Vec<Customer>>> {
    require_server(&state, &p, "view_customers").await?;
    let like = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")));
    Ok(Json(
        sqlx::query_as::<_, Customer>(&format!(
            "SELECT {COLS} FROM customers WHERE business_id = $1 AND ($2::text IS NULL OR name ILIKE $2 OR phone LIKE $2 OR email ILIKE $2) ORDER BY lower(name), id LIMIT $3"
        ))
        .bind(p.business_id)
        .bind(like)
        .bind(q.limit.unwrap_or(50).clamp(1, 200))
        .fetch_all(&state.pool)
        .await?,
    ))
}

pub async fn get(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Customer>> {
    require_server(&state, &p, "view_customer").await?;
    sqlx::query_as::<_, Customer>(&format!("SELECT {COLS} FROM customers WHERE id = $1 AND business_id = $2"))
        .bind(id)
        .bind(p.business_id)
        .fetch_optional(&state.pool)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound("customer"))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    name: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    notes: Option<String>,
    is_active: Option<bool>,
}

/// Changing a customer's record is for managers and business-wide roles; cashiers can add and look up.
pub async fn update(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<UpdateBody>) -> ApiResult<Json<Customer>> {
    if !p.has_any_role(&[Role::StoreManager]) {
        audit::denied(&state.pool, p.business_id, p.user_id, None, "update_customer").await;
        return Err(ApiError::Forbidden);
    }
    if b.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad("customer name cannot be empty"));
    }
    let mut tx = state.pool.begin().await?;
    let before = sqlx::query_as::<_, Customer>(&format!("SELECT {COLS} FROM customers WHERE id = $1 AND business_id = $2 FOR UPDATE")).bind(id).bind(p.business_id).fetch_optional(&mut *tx).await?.ok_or(ApiError::NotFound("customer"))?;
    let after = sqlx::query_as::<_, Customer>(&format!(
        "UPDATE customers SET name = COALESCE($2,name), phone = COALESCE($3,phone), email = COALESCE($4,email), notes = COALESCE($5,notes), is_active = COALESCE($6,is_active), updated_at = now()
         WHERE id = $1 RETURNING {COLS}"
    ))
    .bind(id)
    .bind(clean(b.name))
    .bind(phone(b.phone)?)
    .bind(email(b.email)?)
    .bind(clean(b.notes))
    .bind(b.is_active)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match ApiError::from(e) {
        ApiError::Conflict(_) => ApiError::conflict("a customer with that phone number already exists"),
        other => other,
    })?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "update_customer", entity_type: "customer", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}

/// Purchase history, limited to the sales the caller is allowed to see: everything for business-wide
/// roles, the whole store for managers and finance, only their own sales for a cashier.
pub async fn history(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    require_server(&state, &p, "view_customer_history").await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)").bind(id).bind(p.business_id).fetch_one(&state.pool).await?;
    if !exists {
        return Err(ApiError::NotFound("customer"));
    }
    let wide = p.is_business_wide();
    let stores: Vec<Uuid> = p.grants.iter().filter(|g| matches!(g.role, Role::StoreManager | Role::FinanceOfficer)).filter_map(|g| g.store_id).collect();
    let rows = sqlx::query(
        "SELECT id, store_id, receipt_number, total_minor, currency::text AS currency, created_at FROM sales
         WHERE customer_id = $1 AND business_id = $2 AND ($3 OR store_id = ANY($4) OR cashier_id = $5) ORDER BY created_at DESC LIMIT 200",
    )
    .bind(id)
    .bind(p.business_id)
    .bind(wide)
    .bind(stores)
    .bind(p.user_id)
    .fetch_all(&state.pool)
    .await?;
    let sales: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid, _>("id"), "store_id": r.get::<Uuid, _>("store_id"), "receipt_number": r.get::<String, _>("receipt_number"),
        "total_minor": r.get::<i64, _>("total_minor"), "currency": r.get::<String, _>("currency"), "created_at": r.get::<DateTime<Utc>, _>("created_at"),
    })).collect();
    let total: i64 = sales.iter().filter_map(|s| s["total_minor"].as_i64()).sum();
    Ok(Json(json!({ "sales": sales, "total_spent_minor": total })))
}
