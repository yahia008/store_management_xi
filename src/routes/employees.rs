//! Employees, roles and the audit view (FR-04, FR-05, FR-06).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::{role_summary, Principal, Role};
use crate::security::{check_password_strength, hash_password, normalize_email};
use crate::state::AppState;

pub async fn roles() -> Json<Value> {
    let list: Vec<Value> = Role::ALL
        .iter()
        .map(|r| json!({ "role": r.as_str(), "scope": if r.is_business_wide() { "business" } else { "store" }, "can": role_summary(*r) }))
        .collect();
    Json(json!({ "roles": list }))
}

#[derive(Serialize, FromRow)]
pub struct Employee {
    id: Uuid,
    email: String,
    full_name: String,
    phone: Option<String>,
    is_active: bool,
}

#[derive(Serialize, FromRow)]
pub struct Membership {
    id: Uuid,
    role: String,
    store_id: Option<Uuid>,
}

const EMP_COLS: &str = "id, email, full_name, phone, is_active";

async fn employee_json(db: &mut PgConnection, emp: Employee) -> ApiResult<Value> {
    let ms = sqlx::query_as::<_, Membership>("SELECT id, role, store_id FROM memberships WHERE user_id = $1 ORDER BY created_at").bind(emp.id).fetch_all(&mut *db).await?;
    let mut v = serde_json::to_value(&emp).map_err(|e| ApiError::Internal(e.to_string()))?;
    v["roles"] = json!(ms);
    Ok(v)
}

/// The target must belong to the caller's business, otherwise it does not exist as far as they know.
async fn load_employee(state: &AppState, p: &Principal, id: Uuid) -> ApiResult<Employee> {
    sqlx::query_as::<_, Employee>(&format!("SELECT {EMP_COLS} FROM users WHERE id = $1 AND business_id = $2"))
        .bind(id)
        .bind(p.business_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound("employee"))
}

async fn managed_store_ids(state: &AppState, user_id: Uuid) -> ApiResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar("SELECT store_id FROM memberships WHERE user_id = $1 AND role = 'store_manager' AND store_id IS NOT NULL").bind(user_id).fetch_all(&state.pool).await?)
}

pub async fn list(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Value>>> {
    let emps = if p.is_business_wide() {
        sqlx::query_as::<_, Employee>(&format!("SELECT {EMP_COLS} FROM users WHERE business_id = $1 ORDER BY full_name")).bind(p.business_id).fetch_all(&state.pool).await?
    } else {
        let stores = managed_store_ids(&state, p.user_id).await?;
        if stores.is_empty() {
            audit::denied(&state.pool, p.business_id, p.user_id, None, "list_employees").await;
            return Err(ApiError::Forbidden);
        }
        sqlx::query_as::<_, Employee>(&format!(
            "SELECT {EMP_COLS} FROM users WHERE business_id = $1 AND id IN (SELECT user_id FROM memberships WHERE store_id = ANY($2)) ORDER BY full_name"
        ))
        .bind(p.business_id)
        .bind(stores)
        .fetch_all(&state.pool)
        .await?
    };
    let mut conn = state.pool.acquire().await?;
    let mut out = Vec::new();
    for e in emps {
        out.push(employee_json(&mut conn, e).await?);
    }
    Ok(Json(out))
}

pub async fn get(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let emp = load_employee(&state, &p, id).await?;
    if id != p.user_id && !p.is_business_wide() {
        let stores = managed_store_ids(&state, p.user_id).await?;
        let shares: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND store_id = ANY($2))").bind(id).bind(stores).fetch_one(&state.pool).await?;
        if !shares {
            audit::denied(&state.pool, p.business_id, p.user_id, None, "view_employee").await;
            return Err(ApiError::Forbidden);
        }
    }
    let mut conn = state.pool.acquire().await?;
    Ok(Json(employee_json(&mut conn, emp).await?))
}

/// Checks that a role/store pair is coherent and that the caller may hand it out.
async fn check_grant(state: &AppState, p: &Principal, role: &str, store_id: Option<Uuid>) -> ApiResult<Role> {
    let role = Role::parse(role).ok_or_else(|| ApiError::bad("unknown role"))?;
    match (role.is_business_wide(), store_id) {
        (true, Some(_)) => return Err(ApiError::bad("owner and administrator roles apply to the whole business; leave store_id out")),
        (false, None) => return Err(ApiError::bad("this role belongs to one store; store_id is required")),
        _ => {}
    }
    if role.is_business_wide() {
        p.require_owner(state, "grant_business_wide_role").await?;
    } else {
        p.require_business_admin(state, "grant_role").await?;
    }
    if let Some(store) = store_id {
        let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM stores WHERE id = $1 AND business_id = $2)").bind(store).bind(p.business_id).fetch_one(&state.pool).await?;
        if !ok {
            return Err(ApiError::NotFound("store"));
        }
    }
    Ok(role)
}

#[derive(Deserialize)]
pub struct CreateEmployee {
    email: String,
    full_name: String,
    phone: Option<String>,
    password: String,
    role: String,
    store_id: Option<Uuid>,
}

pub async fn create(State(state): State<AppState>, p: Principal, Json(b): Json<CreateEmployee>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.require_business_admin(&state, "create_employee").await?;
    let email = normalize_email(&b.email)?;
    if b.full_name.trim().is_empty() {
        return Err(ApiError::bad("full name is required"));
    }
    check_password_strength(&b.password)?;
    let role = check_grant(&state, &p, &b.role, b.store_id).await?;
    let hash = hash_password(&b.password)?;

    let mut tx = state.pool.begin().await?;
    let emp = sqlx::query_as::<_, Employee>(&format!("INSERT INTO users (business_id, email, full_name, phone, password_hash, created_by) VALUES ($1,$2,$3,$4,$5,$6) RETURNING {EMP_COLS}"))
        .bind(p.business_id)
        .bind(&email)
        .bind(b.full_name.trim())
        .bind(b.phone.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        .bind(hash)
        .bind(p.user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match ApiError::from(e) {
            ApiError::Conflict(_) => ApiError::conflict("an employee with that email already exists"),
            other => other,
        })?;
    sqlx::query("INSERT INTO memberships (user_id, business_id, store_id, role) VALUES ($1,$2,$3,$4)").bind(emp.id).bind(p.business_id).bind(b.store_id).bind(role.as_str()).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: b.store_id, actor: Some(p.user_id), action: "create_employee", entity_type: "user", entity_id: Some(emp.id.to_string()), after: Some(json!({ "email": emp.email, "role": role.as_str(), "store_id": b.store_id })), ..Default::default() }).await?;
    let out = employee_json(&mut tx, emp).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Deserialize)]
pub struct UpdateEmployee {
    full_name: Option<String>,
    phone: Option<String>,
    is_active: Option<bool>,
}

async fn target_is_privileged(state: &AppState, id: Uuid) -> ApiResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND store_id IS NULL)").bind(id).fetch_one(&state.pool).await?)
}

/// Stops the business ending up with nobody who can run it.
async fn ensure_other_owner(db: &mut PgConnection, business_id: Uuid, leaving: Uuid) -> ApiResult<()> {
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships m JOIN users u ON u.id = m.user_id
         WHERE m.business_id = $1 AND m.role = 'business_owner' AND u.is_active AND m.user_id <> $2",
    )
    .bind(business_id)
    .bind(leaving)
    .fetch_one(&mut *db)
    .await?;
    if others == 0 {
        return Err(ApiError::conflict("the business must keep at least one active owner"));
    }
    Ok(())
}

pub async fn update(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<UpdateEmployee>) -> ApiResult<Json<Value>> {
    p.require_business_admin(&state, "update_employee").await?;
    let before = load_employee(&state, &p, id).await?;
    if b.full_name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad("full name cannot be empty"));
    }
    if b.is_active.is_some() && target_is_privileged(&state, id).await? {
        p.require_owner(&state, "change_status_of_business_wide_user").await?;
    }
    if b.is_active == Some(false) && id == p.user_id {
        return Err(ApiError::conflict("you cannot deactivate your own account"));
    }
    let mut tx = state.pool.begin().await?;
    if b.is_active == Some(false) {
        let is_owner: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND role = 'business_owner')").bind(id).fetch_one(&mut *tx).await?;
        if is_owner {
            ensure_other_owner(&mut tx, p.business_id, id).await?;
        }
    }
    let after = sqlx::query_as::<_, Employee>(&format!(
        "UPDATE users SET full_name = COALESCE($2, full_name), phone = COALESCE($3, phone), is_active = COALESCE($4, is_active), updated_at = now()
         WHERE id = $1 RETURNING {EMP_COLS}"
    ))
    .bind(id)
    .bind(b.full_name.map(|s| s.trim().to_string()))
    .bind(b.phone.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
    .bind(b.is_active)
    .fetch_one(&mut *tx)
    .await?;
    if b.is_active == Some(false) {
        sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'deactivated' WHERE user_id = $1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await?;
    }
    let action = match b.is_active {
        Some(false) => "deactivate_employee",
        Some(true) => "reactivate_employee",
        None => "update_employee",
    };
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action, entity_type: "user", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    let out = employee_json(&mut tx, after).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct GrantBody {
    role: String,
    store_id: Option<Uuid>,
}

pub async fn grant(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<GrantBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.require_business_admin(&state, "grant_role").await?;
    load_employee(&state, &p, id).await?;
    let role = check_grant(&state, &p, &b.role, b.store_id).await?;
    let mut tx = state.pool.begin().await?;
    let m = sqlx::query_as::<_, Membership>("INSERT INTO memberships (user_id, business_id, store_id, role) VALUES ($1,$2,$3,$4) RETURNING id, role, store_id")
        .bind(id)
        .bind(p.business_id)
        .bind(b.store_id)
        .bind(role.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match ApiError::from(e) {
            ApiError::Conflict(_) => ApiError::conflict("the employee already has that role"),
            other => other,
        })?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: b.store_id, actor: Some(p.user_id), action: "grant_role", entity_type: "user", entity_id: Some(id.to_string()), after: audit::json(&m), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!(m))))
}

pub async fn revoke(State(state): State<AppState>, p: Principal, Path((id, membership_id)): Path<(Uuid, Uuid)>) -> ApiResult<Json<Value>> {
    p.require_business_admin(&state, "revoke_role").await?;
    load_employee(&state, &p, id).await?;
    let mut tx = state.pool.begin().await?;
    let m = sqlx::query_as::<_, Membership>("SELECT id, role, store_id FROM memberships WHERE id = $1 AND user_id = $2 FOR UPDATE")
        .bind(membership_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound("role"))?;
    if Role::parse(&m.role).is_some_and(Role::is_business_wide) {
        p.require_owner(&state, "revoke_business_wide_role").await?;
    }
    if m.role == "business_owner" {
        ensure_other_owner(&mut tx, p.business_id, id).await?;
    }
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1 AND id <> $2").bind(id).bind(membership_id).fetch_one(&mut *tx).await?;
    if remaining == 0 {
        return Err(ApiError::conflict("an employee needs at least one role; deactivate the account instead"));
    }
    sqlx::query("DELETE FROM memberships WHERE id = $1").bind(membership_id).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: m.store_id, actor: Some(p.user_id), action: "revoke_role", entity_type: "user", entity_id: Some(id.to_string()), before: audit::json(&m), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

/// Signs an employee out everywhere (a lost phone, a suspected leak).
pub async fn revoke_sessions(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    if id != p.user_id {
        p.require_business_admin(&state, "revoke_sessions").await?;
        load_employee(&state, &p, id).await?;
    }
    let n = sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'revoked_by_admin' WHERE user_id = $1 AND revoked_at IS NULL").bind(id).execute(&state.pool).await?.rows_affected();
    audit::record(&state.pool, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "revoke_sessions", entity_type: "user", entity_id: Some(id.to_string()), after: Some(json!({ "sessions": n })), ..Default::default() }).await?;
    Ok(Json(json!({ "revoked": n })))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    action: Option<String>,
    outcome: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize, FromRow)]
pub struct AuditRow {
    id: i64,
    store_id: Option<Uuid>,
    actor_user_id: Option<Uuid>,
    action: String,
    entity_type: String,
    entity_id: Option<String>,
    outcome: String,
    created_at: DateTime<Utc>,
}

pub async fn audit(State(state): State<AppState>, p: Principal, Query(q): Query<AuditQuery>) -> ApiResult<Json<Vec<AuditRow>>> {
    p.require_business_admin(&state, "view_audit_log").await?;
    let rows = sqlx::query_as::<_, AuditRow>(
        "SELECT id, store_id, actor_user_id, action, entity_type, entity_id, outcome, created_at FROM audit_log
         WHERE business_id = $1 AND ($2::text IS NULL OR action = $2) AND ($3::text IS NULL OR outcome = $3)
         ORDER BY id DESC LIMIT $4",
    )
    .bind(p.business_id)
    .bind(q.action)
    .bind(q.outcome)
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}
