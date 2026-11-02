//! FR-01 business profile, FR-02 stores, FR-03 settings.
mod common;
use axum::http::StatusCode;
use common::*;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr01_setup_runs_once_and_profile_is_editable(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    // A second setup is refused.
    let again = api
        .call(axum::http::Method::POST, "/api/v1/setup", None, Some(json!({
            "legal_name": "Other", "base_currency": "NGN", "timezone": "Africa/Lagos",
            "owner_email": "x@x.test", "owner_name": "X", "owner_password": PASSWORD })))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    let r = api.patch("/api/v1/business", &owner, json!({ "trading_name": "Amiable", "address": "Lagos", "phone": "0800" })).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["trading_name"], "Amiable");
    assert_eq!(r.body["legal_name"], "Amiable Retail Ltd"); // untouched fields stay

    // Changes leave before/after in the audit trail.
    let (before,): (serde_json::Value,) = sqlx::query_as("SELECT before_values FROM audit_log WHERE action = 'update_business'").fetch_one(&api.pool).await.unwrap();
    assert!(before["trading_name"].is_null());
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr03_cashier_cannot_change_tax_or_currency(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let (_, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;

    let tax = api.post("/api/v1/business/tax-rates", &cashier, json!({ "name": "VAT", "rate_bps": 750 })).await;
    assert_eq!(tax.status, StatusCode::FORBIDDEN);
    let cur = api.put("/api/v1/business/settings", &cashier, json!({ "base_currency": "USD" })).await;
    assert_eq!(cur.status, StatusCode::FORBIDDEN);
    assert_eq!(api.audit_count("add_tax_rate", "denied").await, 1);
    assert_eq!(api.audit_count("update_business_settings", "denied").await, 1);

    // Unchanged, and the cashier can still read the settings they need.
    let s = api.get("/api/v1/business/settings", &cashier).await;
    assert_eq!(s.body["base_currency"], "NGN");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr03_tax_rates_are_effective_dated_and_never_rewritten(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let first = api.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 750 })).await;
    assert_eq!(first.status, StatusCode::CREATED);

    let later = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
    let scheduled = api.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 1000, "effective_from": later })).await;
    assert_eq!(scheduled.status, StatusCode::CREATED);

    let list = api.get("/api/v1/business/tax-rates", &owner).await;
    assert_eq!(list.body["current"]["rate_bps"], 750, "a future rate is not current yet");
    assert_eq!(list.body["rates"].as_array().unwrap().len(), 2, "the old rate is kept");

    let past = (chrono::Utc::now() - chrono::Duration::days(3)).to_rfc3339();
    let r = api.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 500, "effective_from": past })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let bad = api.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 12000 })).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr03_currency_and_timezone_are_validated(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    assert_eq!(api.put("/api/v1/business/settings", &owner, json!({ "base_currency": "naira" })).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.put("/api/v1/business/settings", &owner, json!({ "timezone": "Mars/Base" })).await.status, StatusCode::BAD_REQUEST);
    let ok = api.put("/api/v1/business/settings", &owner, json!({ "base_currency": "ghs", "receipt_footer": "Thank you" })).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.body["base_currency"], "GHS");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr02_stores_are_created_listed_and_deactivated(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    api.store(&owner, "Abuja").await;
    api.store(&owner, "Kaduna").await;

    let dup = api.post("/api/v1/stores", &owner, json!({ "name": "lagos" })).await;
    assert_eq!(dup.status, StatusCode::CONFLICT, "store names are unique per business, ignoring case");

    let all = api.get("/api/v1/stores", &owner).await;
    assert_eq!(all.body.as_array().unwrap().len(), 3);

    let off = api.patch(&format!("/api/v1/stores/{lagos}"), &owner, json!({ "status": "inactive" })).await;
    assert_eq!(off.body["status"], "inactive");
    let bad = api.patch(&format!("/api/v1/stores/{lagos}"), &owner, json!({ "status": "gone" })).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr03_store_settings_belong_to_the_store_manager(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let (_, mgr) = api.employee(&owner, "mgr@amiable.test", "store_manager", Some(lagos)).await;
    let (_, cashier) = api.employee(&owner, "c@amiable.test", "cashier", Some(lagos)).await;

    let ok = api.put(&format!("/api/v1/stores/{lagos}/settings"), &mgr, json!({ "receipt_prefix": "LAG" })).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.body["receipt_prefix"], "LAG");
    assert_eq!(ok.body["next_receipt_number"], 1);

    let other = api.put(&format!("/api/v1/stores/{abuja}/settings"), &mgr, json!({ "receipt_prefix": "ABJ" })).await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
    let by_cashier = api.put(&format!("/api/v1/stores/{lagos}/settings"), &cashier, json!({ "receipt_prefix": "HAX" })).await;
    assert_eq!(by_cashier.status, StatusCode::FORBIDDEN);
    let junk = api.put(&format!("/api/v1/stores/{lagos}/settings"), &mgr, json!({ "receipt_prefix": "bad prefix" })).await;
    assert_eq!(junk.status, StatusCode::BAD_REQUEST);

    // A manager may edit contact details but not rename or close the store.
    assert_eq!(api.patch(&format!("/api/v1/stores/{lagos}"), &mgr, json!({ "phone": "0801" })).await.status, StatusCode::OK);
    assert_eq!(api.patch(&format!("/api/v1/stores/{lagos}"), &mgr, json!({ "status": "inactive" })).await.status, StatusCode::FORBIDDEN);
}
