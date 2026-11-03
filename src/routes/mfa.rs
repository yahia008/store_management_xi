//! Multi-factor authentication for staff (NFR-17): enrol an authenticator app, confirm it, sign in with a
//! code, use a recovery code if the phone is lost, and let an owner reset a colleague who is locked out.

use axum::extract::{Path, State};
use axum::Json;
use chrono::Utc;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::session::{open_session, record_failed_attempt};
use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::{Principal, Role};
use crate::security::{hash_token, verify_password};
use crate::state::AppState;
use crate::totp;

/// Recovery codes look like `a1b2c3-d4e5f6`; compared without dashes, spaces or case.
fn normalise_recovery(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

fn new_recovery_codes() -> Vec<String> {
    (0..8)
        .map(|_| {
            let mut b = [0u8; 6];
            rand::rngs::OsRng.fill_bytes(&mut b);
            let h = hex::encode(b);
            format!("{}-{}", &h[..6], &h[6..])
        })
        .collect()
}

async fn user_secret(db: &mut PgConnection, user_id: Uuid) -> ApiResult<(Option<String>, bool, Option<i64>, String)> {
    let r = sqlx::query("SELECT mfa_secret, (mfa_enabled_at IS NOT NULL) AS on_, mfa_last_step, email FROM users WHERE id = $1").bind(user_id).fetch_one(&mut *db).await?;
    Ok((r.get("mfa_secret"), r.get("on_"), r.get("mfa_last_step"), r.get("email")))
}

/// Starts enrolment: makes a secret and returns it with an `otpauth://` link (a QR code in the app).
/// MFA is not on until `confirm` succeeds.
pub async fn enroll(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    let mut conn = state.pool.acquire().await?;
    let (_, enabled, _, email) = user_secret(&mut conn, p.user_id).await?;
    if enabled {
        return Err(ApiError::conflict("multi-factor authentication is already on for this account"));
    }
    let secret = totp::new_secret();
    sqlx::query("UPDATE users SET mfa_secret = $2, mfa_enabled_at = NULL, mfa_last_step = NULL WHERE id = $1").bind(p.user_id).bind(&secret).execute(&mut *conn).await?;
    audit::record(&mut *conn, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "mfa_enrol_started", entity_type: "user", entity_id: Some(p.user_id.to_string()), ..Default::default() }).await?;
    Ok(Json(json!({ "secret": secret, "otpauth_uri": totp::otpauth_uri(&state.config.mfa_issuer, &email, &secret), "next": "POST /auth/mfa/confirm with a code from the app" })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    code: String,
}

/// Finishes enrolment with a first code, turns MFA on, marks this session as verified, and returns the
/// recovery codes. They are shown once; only their hashes are kept.
pub async fn confirm(State(state): State<AppState>, p: Principal, Json(b): Json<CodeBody>) -> ApiResult<Json<Value>> {
    let mut tx = state.pool.begin().await?;
    let (secret, enabled, _, _) = user_secret(&mut tx, p.user_id).await?;
    if enabled {
        return Err(ApiError::conflict("multi-factor authentication is already on for this account"));
    }
    let secret = secret.ok_or_else(|| ApiError::bad("start enrolment first (POST /auth/mfa/enroll)"))?;
    let bytes = totp::decode_secret(&secret).ok_or_else(|| ApiError::Internal("bad stored MFA secret".into()))?;
    let Some(step) = totp::verify(&bytes, &b.code, Utc::now().timestamp(), None) else {
        return Err(ApiError::bad("that code is not right; check the clock on your phone and try again"));
    };
    sqlx::query("UPDATE users SET mfa_enabled_at = now(), mfa_last_step = $2 WHERE id = $1").bind(p.user_id).bind(step).execute(&mut *tx).await?;
    sqlx::query("UPDATE sessions SET mfa_verified = TRUE WHERE id = $1").bind(p.session_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM mfa_recovery_codes WHERE user_id = $1").bind(p.user_id).execute(&mut *tx).await?;
    let codes = new_recovery_codes();
    for c in &codes {
        sqlx::query("INSERT INTO mfa_recovery_codes (user_id, code_hash) VALUES ($1,$2)").bind(p.user_id).bind(hash_token(&normalise_recovery(c))).execute(&mut *tx).await?;
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "mfa_enabled", entity_type: "user", entity_id: Some(p.user_id.to_string()), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "enabled": true, "recovery_codes": codes, "note": "Keep these somewhere safe. Each works once and they are not shown again." })))
}

#[derive(Deserialize)]
pub struct VerifyBody {
    mfa_token: String,
    code: Option<String>,
    recovery_code: Option<String>,
}

/// The second step of sign-in: the challenge from `login` plus a 6-digit code (or a recovery code).
/// A challenge allows a handful of tries and works once; wrong answers also count toward account lockout.
pub async fn verify(State(state): State<AppState>, Json(b): Json<VerifyBody>) -> ApiResult<Json<Value>> {
    if b.code.is_none() == b.recovery_code.is_none() {
        return Err(ApiError::bad("send either code or recovery_code"));
    }
    // Spend an attempt up front, so parallel guesses cannot beat the limit.
    let ch = sqlx::query(
        "UPDATE mfa_challenges SET attempts = attempts + 1
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() AND attempts < 5 RETURNING id, user_id",
    )
    .bind(hash_token(b.mfa_token.trim()))
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::InvalidCredentials)?;
    let (challenge_id, user_id): (Uuid, Uuid) = (ch.get("id"), ch.get("user_id"));
    let user = sqlx::query("SELECT business_id, is_active, (locked_until IS NOT NULL AND locked_until > now()) AS locked FROM users WHERE id = $1").bind(user_id).fetch_one(&state.pool).await?;
    let business_id: Uuid = user.get("business_id");
    if user.get::<bool, _>("locked") || !user.get::<bool, _>("is_active") {
        return Err(ApiError::InvalidCredentials);
    }

    let ok = if let Some(code) = &b.code {
        let mut conn = state.pool.acquire().await?;
        let (secret, enabled, last, _) = user_secret(&mut conn, user_id).await?;
        match (secret.as_deref().and_then(totp::decode_secret), enabled) {
            (Some(bytes), true) => match totp::verify(&bytes, code, Utc::now().timestamp(), last) {
                // The step is recorded conditionally, so two requests with the same code cannot both succeed.
                Some(step) => sqlx::query("UPDATE users SET mfa_last_step = $2 WHERE id = $1 AND (mfa_last_step IS NULL OR mfa_last_step < $2)").bind(user_id).bind(step).execute(&mut *conn).await?.rows_affected() == 1,
                None => false,
            },
            _ => false,
        }
    } else {
        let hash = hash_token(&normalise_recovery(b.recovery_code.as_deref().unwrap_or("")));
        sqlx::query("UPDATE mfa_recovery_codes SET used_at = now() WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL").bind(user_id).bind(hash).execute(&state.pool).await?.rows_affected() == 1
    };
    if !ok {
        record_failed_attempt(&state, user_id, business_id, "mfa").await?;
        return Err(ApiError::InvalidCredentials);
    }

    sqlx::query("UPDATE mfa_challenges SET used_at = now() WHERE id = $1").bind(challenge_id).execute(&state.pool).await?;
    sqlx::query("UPDATE users SET failed_login_count = 0, locked_until = NULL WHERE id = $1").bind(user_id).execute(&state.pool).await?;
    let roles: Vec<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE user_id = $1").bind(user_id).fetch_all(&state.pool).await?;
    let wide = roles.iter().any(|r| Role::parse(r).is_some_and(Role::is_business_wide));
    let (token, _) = open_session(&state, user_id, wide, true).await?;
    let action = if b.recovery_code.is_some() { "login_with_recovery_code" } else { "login" };
    audit::record(&state.pool, Entry { business_id: Some(business_id), actor: Some(user_id), action, entity_type: "user", entity_id: Some(user_id.to_string()), ..Default::default() }).await?;
    Ok(Json(json!({ "token": token, "user_id": user_id })))
}

#[derive(Deserialize)]
pub struct DisableBody {
    password: String,
    code: String,
}

/// Turns MFA off for your own account (needs your password and a current code). Owners and administrators
/// cannot while the business requires admin MFA.
pub async fn disable(State(state): State<AppState>, p: Principal, Json(b): Json<DisableBody>) -> ApiResult<Json<Value>> {
    if state.config.require_admin_mfa && p.is_business_wide() {
        return Err(ApiError::conflict("owners and administrators must keep multi-factor authentication on"));
    }
    let mut tx = state.pool.begin().await?;
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1").bind(p.user_id).fetch_one(&mut *tx).await?;
    let (secret, enabled, last, _) = user_secret(&mut tx, p.user_id).await?;
    let valid = enabled
        && verify_password(&b.password, &hash)
        && secret.as_deref().and_then(totp::decode_secret).is_some_and(|bytes| totp::verify(&bytes, &b.code, Utc::now().timestamp(), last).is_some());
    if !valid {
        return Err(ApiError::bad("password or code is not right, or MFA is not on"));
    }
    sqlx::query("UPDATE users SET mfa_secret = NULL, mfa_enabled_at = NULL, mfa_last_step = NULL WHERE id = $1").bind(p.user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM mfa_recovery_codes WHERE user_id = $1").bind(p.user_id).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "mfa_disabled", entity_type: "user", entity_id: Some(p.user_id.to_string()), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "enabled": false })))
}

/// For a colleague who lost their phone and their recovery codes: clears their MFA, signs them out
/// everywhere, and records it. Owners and administrators can reset staff; only an owner can reset another
/// owner or administrator.
pub async fn reset_for_employee(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    p.require_business_admin(&state, "reset_mfa").await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND business_id = $2)").bind(id).bind(p.business_id).fetch_one(&state.pool).await?;
    if !exists {
        return Err(ApiError::NotFound("employee"));
    }
    let privileged: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND store_id IS NULL)").bind(id).fetch_one(&state.pool).await?;
    if privileged && id != p.user_id {
        p.require_owner(&state, "reset_mfa_of_business_wide_user").await?;
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE users SET mfa_secret = NULL, mfa_enabled_at = NULL, mfa_last_step = NULL WHERE id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM mfa_recovery_codes WHERE user_id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("UPDATE mfa_challenges SET used_at = now() WHERE user_id = $1 AND used_at IS NULL").bind(id).execute(&mut *tx).await?;
    sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'mfa_reset' WHERE user_id = $1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "mfa_reset", entity_type: "user", entity_id: Some(id.to_string()), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}
