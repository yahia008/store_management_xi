//! Password reset and change (FR-04, NFR-16).

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::Principal;
use crate::security::{check_password_strength, hash_password, hash_token, new_token, normalize_email, verify_password};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ForgotBody {
    email: String,
}

/// Always answers the same way, so it cannot be used to find out which emails have accounts.
pub async fn forgot(State(state): State<AppState>, Json(body): Json<ForgotBody>) -> ApiResult<Json<Value>> {
    if let Ok(email) = normalize_email(&body.email) {
        let user = sqlx::query("SELECT id, business_id FROM users WHERE lower(email) = $1 AND is_active").bind(&email).fetch_optional(&state.pool).await?;
        if let Some(user) = user {
            let user_id: Uuid = user.get("id");
            let token = new_token();
            sqlx::query("INSERT INTO password_reset_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, now() + make_interval(mins => $3::int))")
                .bind(user_id)
                .bind(hash_token(&token))
                .bind(state.config.reset_token_ttl_minutes as i32)
                .execute(&state.pool)
                .await?;
            audit::record(&state.pool, Entry { business_id: Some(user.get("business_id")), actor: Some(user_id), action: "password_reset_requested", entity_type: "user", entity_id: Some(user_id.to_string()), ..Default::default() }).await?;
            state.notifier.send_reset(&email, &token).await;
        }
    }
    Ok(Json(json!({ "message": "if that email has an account, a reset link has been sent" })))
}

#[derive(Deserialize)]
pub struct ResetBody {
    token: String,
    new_password: String,
}

pub async fn reset(State(state): State<AppState>, Json(body): Json<ResetBody>) -> ApiResult<Json<Value>> {
    check_password_strength(&body.new_password)?;
    let new_hash = hash_password(&body.new_password)?;
    let mut tx = state.pool.begin().await?;
    // Spending the token is one conditional UPDATE, so two requests with the same token cannot both win.
    let row = sqlx::query(
        "UPDATE password_reset_tokens SET used_at = now()
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() RETURNING user_id",
    )
    .bind(hash_token(body.token.trim()))
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::bad("this reset link is invalid or has expired"))?;
    let user_id: Uuid = row.get("user_id");
    let business_id: Uuid = sqlx::query_scalar(
        "UPDATE users SET password_hash = $2, password_changed_at = now(), failed_login_count = 0, locked_until = NULL
         WHERE id = $1 RETURNING business_id",
    )
    .bind(user_id)
    .bind(new_hash)
    .fetch_one(&mut *tx)
    .await?;
    // Any other open reset links and every signed-in session die with the old password.
    sqlx::query("UPDATE password_reset_tokens SET used_at = now() WHERE user_id = $1 AND used_at IS NULL").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'password_reset' WHERE user_id = $1 AND revoked_at IS NULL").bind(user_id).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(business_id), actor: Some(user_id), action: "password_reset", entity_type: "user", entity_id: Some(user_id.to_string()), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ChangeBody {
    current_password: String,
    new_password: String,
}

pub async fn change(State(state): State<AppState>, p: Principal, Json(body): Json<ChangeBody>) -> ApiResult<Json<Value>> {
    check_password_strength(&body.new_password)?;
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1").bind(p.user_id).fetch_one(&state.pool).await?;
    if !verify_password(&body.current_password, &hash) {
        return Err(ApiError::bad("current password is incorrect"));
    }
    let new_hash = hash_password(&body.new_password)?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2, password_changed_at = now() WHERE id = $1").bind(p.user_id).bind(new_hash).execute(&mut *tx).await?;
    // Sign out everywhere except here.
    sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'password_change' WHERE user_id = $1 AND id <> $2 AND revoked_at IS NULL")
        .bind(p.user_id)
        .bind(p.session_id)
        .execute(&mut *tx)
        .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "password_change", entity_type: "user", entity_id: Some(p.user_id.to_string()), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}
