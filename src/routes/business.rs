//! Business profile, settings and tax rates (FR-01, FR-03).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::Principal;
use crate::security::{check_password_strength, hash_password, normalize_email};
use crate::state::AppState;

#[derive(Serialize, FromRow)]
pub struct Business {
    id: uuid::Uuid,
    legal_name: String,
    trading_name: Option<String>,
    registration_number: Option<String>,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    logo_url: Option<String>,
}

#[derive(Serialize, FromRow)]
pub struct Settings {
    base_currency: String,
    timezone: String,
    receipt_header: Option<String>,
    receipt_footer: Option<String>,
}

#[derive(Serialize, FromRow)]
pub struct TaxRate {
    id: uuid::Uuid,
    name: String,
    rate_bps: i32,
    effective_from: DateTime<Utc>,
}

const BUSINESS_COLS: &str = "id, legal_name, trading_name, registration_number, address, phone, email, logo_url";

fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn check_currency(c: &str) -> ApiResult<String> {
    let c = c.trim().to_uppercase();
    if c.len() == 3 && c.chars().all(|ch| ch.is_ascii_uppercase()) {
        Ok(c)
    } else {
        Err(ApiError::bad("currency must be a 3-letter ISO 4217 code such as NGN"))
    }
}

fn check_timezone(tz: &str) -> ApiResult<String> {
    let tz = tz.trim();
    tz.parse::<chrono_tz::Tz>().map(|_| tz.to_string()).map_err(|_| ApiError::bad("timezone must be an IANA name such as Africa/Lagos"))
}

#[derive(Deserialize)]
pub struct SetupBody {
    legal_name: String,
    trading_name: Option<String>,
    registration_number: Option<String>,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    base_currency: String,
    timezone: String,
    owner_email: String,
    owner_name: String,
    owner_password: String,
}

/// Creates the business and its first owner. It works exactly once: after that the business exists and
/// everything else needs a signed-in owner or administrator.
pub async fn setup(State(state): State<AppState>, Json(b): Json<SetupBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    if b.legal_name.trim().is_empty() || b.owner_name.trim().is_empty() {
        return Err(ApiError::bad("business name and owner name are required"));
    }
    let currency = check_currency(&b.base_currency)?;
    let timezone = check_timezone(&b.timezone)?;
    let owner_email = normalize_email(&b.owner_email)?;
    check_password_strength(&b.owner_password)?;
    let hash = hash_password(&b.owner_password)?;

    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7001)").execute(&mut *tx).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM businesses)").fetch_one(&mut *tx).await?;
    if exists {
        return Err(ApiError::conflict("the business has already been set up"));
    }
    let business_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO businesses (legal_name, trading_name, registration_number, address, phone, email) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
    )
    .bind(b.legal_name.trim())
    .bind(clean(b.trading_name))
    .bind(clean(b.registration_number))
    .bind(clean(b.address))
    .bind(clean(b.phone))
    .bind(clean(b.email))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO business_settings (business_id, base_currency, timezone) VALUES ($1,$2,$3)").bind(business_id).bind(&currency).bind(&timezone).execute(&mut *tx).await?;
    let owner_id: uuid::Uuid = sqlx::query_scalar("INSERT INTO users (business_id, email, full_name, password_hash) VALUES ($1,$2,$3,$4) RETURNING id")
        .bind(business_id)
        .bind(&owner_email)
        .bind(b.owner_name.trim())
        .bind(hash)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO memberships (user_id, business_id, store_id, role) VALUES ($1,$2,NULL,'business_owner')").bind(owner_id).bind(business_id).execute(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(business_id), actor: Some(owner_id), action: "business_setup", entity_type: "business", entity_id: Some(business_id.to_string()), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "business_id": business_id, "owner_id": owner_id }))))
}

pub async fn get(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Business>> {
    let b = sqlx::query_as::<_, Business>(&format!("SELECT {BUSINESS_COLS} FROM businesses WHERE id = $1")).bind(p.business_id).fetch_one(&state.pool).await?;
    Ok(Json(b))
}

#[derive(Deserialize)]
pub struct UpdateBusiness {
    legal_name: Option<String>,
    trading_name: Option<String>,
    registration_number: Option<String>,
    address: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    logo_url: Option<String>,
}

pub async fn update(State(state): State<AppState>, p: Principal, Json(b): Json<UpdateBusiness>) -> ApiResult<Json<Business>> {
    p.require_business_admin(&state, "update_business").await?;
    if b.legal_name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad("business name cannot be empty"));
    }
    let mut tx = state.pool.begin().await?;
    let q = format!("SELECT {BUSINESS_COLS} FROM businesses WHERE id = $1 FOR UPDATE");
    let before = sqlx::query_as::<_, Business>(&q).bind(p.business_id).fetch_one(&mut *tx).await?;
    let after = sqlx::query_as::<_, Business>(&format!(
        "UPDATE businesses SET legal_name = COALESCE($2, legal_name), trading_name = COALESCE($3, trading_name),
             registration_number = COALESCE($4, registration_number), address = COALESCE($5, address), phone = COALESCE($6, phone),
             email = COALESCE($7, email), logo_url = COALESCE($8, logo_url), updated_at = now(), updated_by = $9
         WHERE id = $1 RETURNING {BUSINESS_COLS}"
    ))
    .bind(p.business_id)
    .bind(clean(b.legal_name))
    .bind(clean(b.trading_name))
    .bind(clean(b.registration_number))
    .bind(clean(b.address))
    .bind(clean(b.phone))
    .bind(clean(b.email))
    .bind(clean(b.logo_url))
    .bind(p.user_id)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "update_business", entity_type: "business", entity_id: Some(p.business_id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}

pub async fn get_settings(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Settings>> {
    let s = sqlx::query_as::<_, Settings>("SELECT base_currency::text AS base_currency, timezone, receipt_header, receipt_footer FROM business_settings WHERE business_id = $1")
        .bind(p.business_id)
        .fetch_one(&state.pool)
        .await?;
    Ok(Json(s))
}

#[derive(Deserialize)]
pub struct PutSettings {
    base_currency: Option<String>,
    timezone: Option<String>,
    receipt_header: Option<String>,
    receipt_footer: Option<String>,
}

pub async fn put_settings(State(state): State<AppState>, p: Principal, Json(b): Json<PutSettings>) -> ApiResult<Json<Settings>> {
    p.require_business_admin(&state, "update_business_settings").await?;
    let currency = b.base_currency.as_deref().map(check_currency).transpose()?;
    let timezone = b.timezone.as_deref().map(check_timezone).transpose()?;
    let mut tx = state.pool.begin().await?;
    let cols = "base_currency::text AS base_currency, timezone, receipt_header, receipt_footer";
    let before = sqlx::query_as::<_, Settings>(&format!("SELECT {cols} FROM business_settings WHERE business_id = $1 FOR UPDATE")).bind(p.business_id).fetch_one(&mut *tx).await?;
    let after = sqlx::query_as::<_, Settings>(&format!(
        "UPDATE business_settings SET base_currency = COALESCE($2, base_currency), timezone = COALESCE($3, timezone),
             receipt_header = COALESCE($4, receipt_header), receipt_footer = COALESCE($5, receipt_footer), updated_at = now(), updated_by = $6
         WHERE business_id = $1 RETURNING {cols}"
    ))
    .bind(p.business_id)
    .bind(currency)
    .bind(timezone)
    .bind(b.receipt_header)
    .bind(b.receipt_footer)
    .bind(p.user_id)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "update_business_settings", entity_type: "business_settings", entity_id: Some(p.business_id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}

pub async fn list_tax(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    let rates = sqlx::query_as::<_, TaxRate>("SELECT id, name, rate_bps, effective_from FROM tax_rates WHERE business_id = $1 ORDER BY effective_from DESC, created_at DESC")
        .bind(p.business_id)
        .fetch_all(&state.pool)
        .await?;
    let current = sqlx::query_as::<_, TaxRate>("SELECT id, name, rate_bps, effective_from FROM tax_rates WHERE business_id = $1 AND effective_from <= now() ORDER BY effective_from DESC, created_at DESC LIMIT 1")
        .bind(p.business_id)
        .fetch_optional(&state.pool)
        .await?;
    Ok(Json(json!({ "current": current, "rates": rates })))
}

#[derive(Deserialize)]
pub struct AddTax {
    name: String,
    rate_bps: i32,
    effective_from: Option<DateTime<Utc>>,
}

/// Adds a rate; earlier rates are kept so past sales keep the rate they were made with. A rate cannot
/// start in the past, because that would quietly change what old receipts mean.
pub async fn add_tax(State(state): State<AppState>, p: Principal, Json(b): Json<AddTax>) -> ApiResult<(StatusCode, Json<TaxRate>)> {
    p.require_business_admin(&state, "add_tax_rate").await?;
    if b.name.trim().is_empty() || !(0..=10_000).contains(&b.rate_bps) {
        return Err(ApiError::bad("a name and a rate between 0 and 10000 basis points (100%) are required"));
    }
    let from = b.effective_from.unwrap_or_else(Utc::now);
    if from < Utc::now() - chrono::Duration::seconds(60) {
        return Err(ApiError::bad("a tax rate cannot start in the past"));
    }
    let mut tx = state.pool.begin().await?;
    let rate = sqlx::query_as::<_, TaxRate>("INSERT INTO tax_rates (business_id, name, rate_bps, effective_from, created_by) VALUES ($1,$2,$3,$4,$5) RETURNING id, name, rate_bps, effective_from")
        .bind(p.business_id)
        .bind(b.name.trim())
        .bind(b.rate_bps)
        .bind(from)
        .bind(p.user_id)
        .fetch_one(&mut *tx)
        .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "add_tax_rate", entity_type: "tax_rate", entity_id: Some(rate.id.to_string()), after: audit::json(&rate), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(rate)))
}
