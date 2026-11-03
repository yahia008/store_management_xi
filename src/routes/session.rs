use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::{Principal, Role};
use crate::security::{dummy_verify, hash_token, new_token, normalize_email, verify_password};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

/// Create a session for a user and return the one-time token.
pub async fn open_session(state: &AppState, user_id: Uuid, business_wide: bool, mfa_verified: bool) -> ApiResult<(String, Uuid)> {
    let token = new_token();
    let idle = if business_wide { state.config.admin_session_idle_minutes } else { state.config.session_idle_minutes };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO sessions (user_id, token_hash, expires_at, idle_timeout_minutes, mfa_verified)
         VALUES ($1, $2, now() + make_interval(hours => $3::int), $4, $5) RETURNING id",
    )
    .bind(user_id)
    .bind(hash_token(&token))
    .bind(state.config.session_ttl_hours as i32)
    .bind(idle)
    .bind(mfa_verified)
    .fetch_one(&state.pool)
    .await?;
    Ok((token, id))
}

/// Counts a wrong password or wrong MFA code against the account, locking it after too many in a row
/// (NFR-16), and records it. Returns whether the account is now locked.
pub async fn record_failed_attempt(state: &AppState, user_id: Uuid, business_id: Uuid, what: &str) -> ApiResult<bool> {
    let now_locked: bool = sqlx::query_scalar(
        "UPDATE users SET failed_login_count = failed_login_count + 1,
             locked_until = CASE WHEN failed_login_count + 1 >= $2 THEN now() + make_interval(mins => $3::int) ELSE locked_until END
         WHERE id = $1 RETURNING (locked_until IS NOT NULL AND locked_until > now())",
    )
    .bind(user_id)
    .bind(state.config.max_failed_logins)
    .bind(state.config.lockout_minutes as i32)
    .fetch_one(&state.pool)
    .await?;
    let action = if now_locked { format!("{what}_failed_account_locked") } else { format!("{what}_failed") };
    audit::record(&state.pool, Entry { business_id: Some(business_id), actor: Some(user_id), action: &action, entity_type: "user", entity_id: Some(user_id.to_string()), outcome: Some("denied"), ..Default::default() }).await?;
    Ok(now_locked)
}

pub async fn login(State(state): State<AppState>, Json(body): Json<LoginBody>) -> ApiResult<Json<Value>> {
    let Ok(email) = normalize_email(&body.email) else {
        dummy_verify(&body.password);
        return Err(ApiError::InvalidCredentials);
    };
    let row = sqlx::query(
        "SELECT id, business_id, password_hash, is_active, (locked_until IS NOT NULL AND locked_until > now()) AS locked, (mfa_enabled_at IS NOT NULL) AS mfa_on
         FROM users WHERE lower(email) = $1",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row else {
        dummy_verify(&body.password);
        return Err(ApiError::InvalidCredentials);
    };
    let user_id: Uuid = row.get("id");
    let business_id: Uuid = row.get("business_id");
    let locked: bool = row.get("locked");
    let active: bool = row.get("is_active");
    let correct = verify_password(&body.password, row.get::<&str, _>("password_hash"));

    // A locked or deactivated account is refused even with the right password, and says nothing more.
    if locked || !active {
        let action = if locked { "login_while_locked" } else { "login_while_inactive" };
        audit::record(&state.pool, Entry { business_id: Some(business_id), actor: Some(user_id), action, entity_type: "user", entity_id: Some(user_id.to_string()), outcome: Some("denied"), ..Default::default() }).await?;
        return Err(ApiError::InvalidCredentials);
    }

    if !correct {
        record_failed_attempt(&state, user_id, business_id, "login").await?;
        return Err(ApiError::InvalidCredentials);
    }

    // With MFA switched on, a correct password only earns a short-lived challenge; the session comes after the code.
    if row.get::<bool, _>("mfa_on") {
        let challenge = new_token();
        sqlx::query("INSERT INTO mfa_challenges (user_id, token_hash, expires_at) VALUES ($1, $2, now() + interval '5 minutes')").bind(user_id).bind(hash_token(&challenge)).execute(&state.pool).await?;
        audit::record(&state.pool, Entry { business_id: Some(business_id), actor: Some(user_id), action: "login_password_ok_mfa_pending", entity_type: "user", entity_id: Some(user_id.to_string()), ..Default::default() }).await?;
        return Ok(Json(json!({ "mfa_required": true, "mfa_token": challenge, "expires_in_seconds": 300 })));
    }
    sqlx::query("UPDATE users SET failed_login_count = 0, locked_until = NULL WHERE id = $1").bind(user_id).execute(&state.pool).await?;
    let roles: Vec<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE user_id = $1").bind(user_id).fetch_all(&state.pool).await?;
    let wide = roles.iter().any(|r| Role::parse(r).is_some_and(Role::is_business_wide));
    let (token, _) = open_session(&state, user_id, wide, false).await?;
    audit::record(&state.pool, Entry { business_id: Some(business_id), actor: Some(user_id), action: "login", entity_type: "user", entity_id: Some(user_id.to_string()), ..Default::default() }).await?;
    Ok(Json(json!({ "token": token, "user_id": user_id })))
}

pub async fn logout(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'logout' WHERE id = $1").bind(p.session_id).execute(&state.pool).await?;
    audit::record(&state.pool, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "logout", entity_type: "user", entity_id: Some(p.user_id.to_string()), ..Default::default() }).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn me(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    let u = sqlx::query("SELECT email, full_name, phone, (mfa_enabled_at IS NOT NULL) AS mfa_on FROM users WHERE id = $1").bind(p.user_id).fetch_one(&state.pool).await?;
    let grants: Vec<Value> = p.grants.iter().map(|g| json!({ "role": g.role.as_str(), "store_id": g.store_id })).collect();
    Ok(Json(json!({
        "id": p.user_id, "business_id": p.business_id,
        "email": u.get::<String, _>("email"), "full_name": u.get::<String, _>("full_name"), "phone": u.get::<Option<String>, _>("phone"),
        "roles": grants,
        "mfa_enabled": u.get::<bool, _>("mfa_on"), "mfa_verified": p.mfa_verified,
        "mfa_required": state.config.require_admin_mfa && p.is_business_wide() && !p.mfa_verified,
    })))
}
