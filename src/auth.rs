//! Turns `Authorization: Bearer <token>` into a [`Principal`] built from the database (FR-04).

use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts};
use sqlx::Row;
use uuid::Uuid;

use crate::audit;
use crate::error::{ApiError, ApiResult};
use crate::permissions::{Grant, Principal, Role};
use crate::security::hash_token;
use crate::state::AppState;

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;
        let hash = hash_token(token.trim());

        // One statement checks revocation, expiry, idle time and whether the account is still active,
        // and slides the idle window forward.
        let row = sqlx::query(
            "UPDATE sessions s SET last_used_at = now()
             FROM users u
             WHERE s.token_hash = $1 AND u.id = s.user_id AND u.is_active
               AND s.revoked_at IS NULL AND s.expires_at > now()
               AND s.last_used_at + make_interval(mins => s.idle_timeout_minutes) > now()
             RETURNING s.id AS session_id, u.id AS user_id, u.business_id",
        )
        .bind(&hash)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::Unauthorized)?;
        let session_id: Uuid = row.get("session_id");
        let user_id: Uuid = row.get("user_id");
        let business_id: Uuid = row.get("business_id");

        let rows = sqlx::query("SELECT role, store_id FROM memberships WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&state.pool)
            .await?;
        let grants = rows
            .iter()
            .filter_map(|r| Some(Grant { role: Role::parse(r.get::<String, _>("role").as_str())?, store_id: r.get("store_id") }))
            .collect();
        Ok(Principal { user_id, business_id, session_id, grants })
    }
}

impl Principal {
    /// Owner or administrator only.
    pub async fn require_business_admin(&self, state: &AppState, action: &str) -> ApiResult<()> {
        if self.is_business_wide() {
            return Ok(());
        }
        audit::denied(&state.pool, self.business_id, self.user_id, None, action).await;
        Err(ApiError::Forbidden)
    }

    pub async fn require_owner(&self, state: &AppState, action: &str) -> ApiResult<()> {
        if self.is_owner() {
            return Ok(());
        }
        audit::denied(&state.pool, self.business_id, self.user_id, None, action).await;
        Err(ApiError::Forbidden)
    }

    /// Check access to one store. A store in another business looks like it does not exist; a store in
    /// this business that the user has no matching role at is refused and recorded (FR-06).
    pub async fn authorize_store(&self, state: &AppState, store_id: Uuid, roles: &[Role], action: &str) -> ApiResult<()> {
        let owner: Option<Uuid> = sqlx::query_scalar("SELECT business_id FROM stores WHERE id = $1")
            .bind(store_id)
            .fetch_optional(&state.pool)
            .await?;
        if owner != Some(self.business_id) {
            return Err(ApiError::NotFound("store"));
        }
        if self.has_role_at(store_id, roles) {
            return Ok(());
        }
        audit::denied(&state.pool, self.business_id, self.user_id, Some(store_id), action).await;
        Err(ApiError::Forbidden)
    }
}
