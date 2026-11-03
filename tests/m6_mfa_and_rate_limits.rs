//! NFR-17 multi-factor authentication for staff, NFR-20 API abuse protection.
mod common;
use axum::http::{Method, StatusCode};
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use store_api::{totp, Config};
use uuid::Uuid;

const VERIFY: &str = "/api/v1/auth/mfa/verify";

fn mfa_config() -> Config {
    Config { require_admin_mfa: true, ..Config::default() }
}

/// The code an authenticator app would show `steps_ahead` 30-second steps from now.
fn code(secret_b32: &str, steps_ahead: i64) -> String {
    totp::code_at(&totp::decode_secret(secret_b32).unwrap(), chrono::Utc::now().timestamp() + steps_ahead * 30)
}

/// Enrols the signed-in user and returns (secret, recovery codes). The session becomes verified.
async fn enrol(api: &Api, token: &str) -> (String, Vec<String>) {
    let e = api.post("/api/v1/auth/mfa/enroll", token, json!({})).await;
    assert_eq!(e.status, StatusCode::OK, "{}", e.body);
    let secret = e.body["secret"].as_str().unwrap().to_string();
    let c = api.post("/api/v1/auth/mfa/confirm", token, json!({ "code": code(&secret, 0) })).await;
    assert_eq!(c.status, StatusCode::OK, "{}", c.body);
    let codes = c.body["recovery_codes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    (secret, codes)
}

async fn verify(api: &Api, body: Value) -> Resp {
    api.call(Method::POST, VERIFY, None, Some(body)).await
}

/// Forget the last accepted step, as if time had moved on, so another code can be used in a test.
async fn move_time_on(api: &Api) {
    sqlx::query("UPDATE users SET mfa_last_step = mfa_last_step - 10 WHERE mfa_last_step IS NOT NULL").execute(&api.pool).await.unwrap();
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr17_admins_cannot_use_admin_functions_until_mfa_is_set_up_and_verified(pool: PgPool) {
    let api = Api::with_config(pool, mfa_config());
    let owner = api.setup().await;

    let blocked = api.get("/api/v1/business", &owner).await;
    assert_eq!((blocked.status, blocked.body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some("mfa_required")));
    assert_eq!(api.post("/api/v1/stores", &owner, json!({ "name": "Lagos" })).await.status, StatusCode::FORBIDDEN);
    assert!(api.audit_count("admin_function_without_mfa", "denied").await >= 2);

    // Enough still works to get set up: look at yourself, change your password, enrol.
    let me = api.get("/api/v1/me", &owner).await;
    assert_eq!((me.status, me.body["mfa_required"].as_bool(), me.body["mfa_enabled"].as_bool()), (StatusCode::OK, Some(true), Some(false)));
    assert_eq!(api.post("/api/v1/auth/mfa/confirm", &owner, json!({ "code": "123456" })).await.status, StatusCode::BAD_REQUEST, "enrol first");

    let e = api.post("/api/v1/auth/mfa/enroll", &owner, json!({})).await;
    assert!(e.body["otpauth_uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    let secret = e.body["secret"].as_str().unwrap().to_string();
    assert_eq!(api.get("/api/v1/business", &owner).await.status, StatusCode::FORBIDDEN, "enrolling is not enough");
    let wrong = api.post("/api/v1/auth/mfa/confirm", &owner, json!({ "code": code(&secret, 100) })).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    let ok = api.post("/api/v1/auth/mfa/confirm", &owner, json!({ "code": code(&secret, 0) })).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.body["recovery_codes"].as_array().unwrap().len(), 8);

    assert_eq!(api.get("/api/v1/business", &owner).await.status, StatusCode::OK, "this session is now verified");
    assert_eq!(api.get("/api/v1/me", &owner).await.body["mfa_verified"], true);
    assert_eq!(api.post("/api/v1/auth/mfa/enroll", &owner, json!({})).await.status, StatusCode::CONFLICT, "already on");
    assert_eq!(api.audit_count("mfa_enabled", "success").await, 1);

    // Staff who are not owners or administrators are not forced to use MFA.
    let lagos = api.store(&owner, "Lagos").await;
    let (_, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    assert_eq!(api.get("/api/v1/stores", &cashier).await.status, StatusCode::OK);
    // Secrets and recovery codes are stored hashed or only handed out once.
    let stored: Vec<(String,)> = sqlx::query_as("SELECT code_hash FROM mfa_recovery_codes").fetch_all(&api.pool).await.unwrap();
    let plain = ok.body["recovery_codes"].as_array().unwrap();
    assert!(stored.iter().all(|(h,)| plain.iter().all(|c| c.as_str().unwrap() != h && h.len() == 64)));
    assert!(!api.get("/api/v1/me", &owner).await.body.to_string().contains(&secret));
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr17_signing_in_with_mfa_needs_a_valid_code_and_each_code_works_once(pool: PgPool) {
    let api = Api::with_config(pool, mfa_config());
    let owner = api.setup().await;
    let (secret, _) = enrol(&api, &owner).await;

    let step1 = api.login("owner@amiable.test", PASSWORD).await;
    assert_eq!(step1.status, StatusCode::OK);
    assert_eq!(step1.body["mfa_required"], true);
    assert!(step1.body.get("token").is_none(), "a password alone no longer gives a session");
    let challenge = step1.body["mfa_token"].as_str().unwrap().to_string();

    assert_eq!(verify(&api, json!({ "mfa_token": challenge, "code": code(&secret, 100) })).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(verify(&api, json!({ "mfa_token": "nonsense", "code": code(&secret, 1) })).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(verify(&api, json!({ "mfa_token": challenge })).await.status, StatusCode::BAD_REQUEST, "a code is needed");
    // The code used to enrol is already spent; the next step's code is fine.
    assert_eq!(verify(&api, json!({ "mfa_token": challenge, "code": code(&secret, 0) })).await.status, StatusCode::UNAUTHORIZED, "no replay of the enrolment code");
    let good = verify(&api, json!({ "mfa_token": challenge, "code": code(&secret, 1) })).await;
    assert_eq!(good.status, StatusCode::OK, "{}", good.body);
    let token = good.body["token"].as_str().unwrap().to_string();
    assert_eq!(api.get("/api/v1/business", &token).await.status, StatusCode::OK, "the new session is verified");

    // The same code cannot be used again, nor the same challenge.
    let again = api.login("owner@amiable.test", PASSWORD).await.body["mfa_token"].as_str().unwrap().to_string();
    assert_eq!(verify(&api, json!({ "mfa_token": again, "code": code(&secret, 1) })).await.status, StatusCode::UNAUTHORIZED);
    move_time_on(&api).await;
    assert_eq!(verify(&api, json!({ "mfa_token": challenge, "code": code(&secret, 1) })).await.status, StatusCode::UNAUTHORIZED, "a challenge works once");

    // Challenges expire.
    let late = api.login("owner@amiable.test", PASSWORD).await.body["mfa_token"].as_str().unwrap().to_string();
    sqlx::query("UPDATE mfa_challenges SET expires_at = now() - interval '1 second'").execute(&api.pool).await.unwrap();
    assert_eq!(verify(&api, json!({ "mfa_token": late, "code": code(&secret, 1) })).await.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr17_recovery_codes_work_once_each(pool: PgPool) {
    let api = Api::with_config(pool, mfa_config());
    let owner = api.setup().await;
    let (_, codes) = enrol(&api, &owner).await;
    let challenge = || async { api.login("owner@amiable.test", PASSWORD).await.body["mfa_token"].as_str().unwrap().to_string() };

    let used = verify(&api, json!({ "mfa_token": challenge().await, "recovery_code": codes[0] })).await;
    assert_eq!(used.status, StatusCode::OK, "{}", used.body);
    assert_eq!(api.get("/api/v1/business", used.body["token"].as_str().unwrap()).await.status, StatusCode::OK);
    assert_eq!(verify(&api, json!({ "mfa_token": challenge().await, "recovery_code": codes[0] })).await.status, StatusCode::UNAUTHORIZED, "spent");
    // Case, dashes and spaces do not matter.
    let loose = codes[1].replace('-', " ").to_uppercase();
    assert_eq!(verify(&api, json!({ "mfa_token": challenge().await, "recovery_code": loose })).await.status, StatusCode::OK);
    assert_eq!(verify(&api, json!({ "mfa_token": challenge().await, "code": "123456", "recovery_code": codes[2] })).await.status, StatusCode::BAD_REQUEST, "one or the other");
    assert_eq!(api.audit_count("login_with_recovery_code", "success").await, 2);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr17_wrong_codes_count_toward_the_account_lockout(pool: PgPool) {
    let api = Api::with_config(pool, mfa_config());
    let owner = api.setup().await;
    let (secret, _) = enrol(&api, &owner).await;
    let challenge = api.login("owner@amiable.test", PASSWORD).await.body["mfa_token"].as_str().unwrap().to_string();
    for _ in 0..5 {
        assert_eq!(verify(&api, json!({ "mfa_token": challenge, "code": code(&secret, 100) })).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(api.audit_count("mfa_failed_account_locked", "denied").await, 1);
    assert_eq!(api.login("owner@amiable.test", PASSWORD).await.status, StatusCode::UNAUTHORIZED, "locked, even with the right password");
    assert_eq!(verify(&api, json!({ "mfa_token": challenge, "code": code(&secret, 1) })).await.status, StatusCode::UNAUTHORIZED, "and the challenge is spent");

    sqlx::query("UPDATE users SET locked_until = NULL, failed_login_count = 0").execute(&api.pool).await.unwrap();
    let fresh = api.login("owner@amiable.test", PASSWORD).await.body["mfa_token"].as_str().unwrap().to_string();
    assert_eq!(verify(&api, json!({ "mfa_token": fresh, "code": code(&secret, 1) })).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr17_staff_can_opt_in_and_an_owner_can_reset_a_lost_phone(pool: PgPool) {
    let api = Api::with_config(pool, mfa_config());
    let owner = api.setup().await;
    enrol(&api, &owner).await; // an owner must be verified before doing anything administrative
    let lagos = api.store(&owner, "Lagos").await;
    let (cashier_id, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    let (_, mgr) = api.employee(&owner, "mgr@amiable.test", "store_manager", Some(lagos)).await;
    let (admin_id, _) = api.employee(&owner, "admin@amiable.test", "administrator", None).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = 'owner@amiable.test'").fetch_one(&api.pool).await.unwrap();

    let (secret, _) = enrol(&api, &cashier).await;
    let login = api.login("cash@amiable.test", PASSWORD).await;
    assert_eq!(login.body["mfa_required"], true, "a cashier who turned MFA on must use it");

    // A store manager cannot reset someone's MFA; an owner can, and it signs them out.
    assert_eq!(api.post(&format!("/api/v1/employees/{cashier_id}/mfa/reset"), &mgr, json!({})).await.status, StatusCode::FORBIDDEN);
    let admin_token = {
        let tok = api.login("admin@amiable.test", PASSWORD).await.body["token"].as_str().unwrap().to_string();
        let (s, _) = enrol(&api, &tok).await;
        let _ = s;
        tok
    };
    assert_eq!(api.post(&format!("/api/v1/employees/{cashier_id}/mfa/reset"), &admin_token, json!({})).await.status, StatusCode::OK, "an administrator can reset staff");
    assert_eq!(api.get("/api/v1/me", &cashier).await.status, StatusCode::UNAUTHORIZED, "their sessions are gone");
    assert!(api.login("cash@amiable.test", PASSWORD).await.body["token"].is_string(), "and they can sign in with just a password again");
    assert_eq!(api.audit_count("mfa_reset", "success").await, 1);
    let _ = secret;

    // Only an owner can reset another administrator or the owner.
    assert_eq!(api.post(&format!("/api/v1/employees/{owner_id}/mfa/reset"), &admin_token, json!({})).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.post(&format!("/api/v1/employees/{admin_id}/mfa/reset"), &owner, json!({})).await.status, StatusCode::OK);
    assert_eq!(api.post(&format!("/api/v1/employees/{}/mfa/reset", Uuid::new_v4()), &owner, json!({})).await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr17_turning_mfa_off_needs_password_and_code_and_is_refused_for_admins(pool: PgPool) {
    let api = Api::with_config(pool, mfa_config());
    let owner = api.setup().await;
    let (osecret, _) = enrol(&api, &owner).await;
    let lagos = api.store(&owner, "Lagos").await;
    let (_, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    let (secret, _) = enrol(&api, &cashier).await;
    let off = |password: &str, c: String| api.post("/api/v1/auth/mfa/disable", &cashier, json!({ "password": password, "code": c }));
    assert_eq!(off("wrong-password-1", code(&secret, 1)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(off(PASSWORD, code(&secret, 100)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(off(PASSWORD, code(&secret, 1)).await.status, StatusCode::OK);
    assert!(api.login("cash@amiable.test", PASSWORD).await.body["token"].is_string());
    let refused = api.post("/api/v1/auth/mfa/disable", &owner, json!({ "password": PASSWORD, "code": code(&osecret, 1) })).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "admins must keep MFA on");
}

// ------------------------------------------------------------------ rate limiting (NFR-20)

fn limits() -> Config {
    // A day-long window so a test cannot straddle a window boundary.
    Config { rate_limit_enabled: true, rate_window_seconds: 86_400, trust_proxy_headers: true, ..Config::default() }
}

async fn from_ip(api: &Api, ip: &str, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> Resp {
    api.call_h(method, path, token, body, &[("x-forwarded-for", ip)]).await
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr20_sign_in_and_password_reset_are_limited_per_address(pool: PgPool) {
    let setup = Api::new(pool.clone());
    setup.setup().await;
    let api = Api::with_config(pool, Config { rate_auth_per_window: 3, ..limits() });
    let try_login = |ip: &'static str, pw: &'static str| from_ip(&api, ip, Method::POST, "/api/v1/auth/login", None, Some(json!({ "email": "owner@amiable.test", "password": pw })));

    for _ in 0..3 {
        assert_eq!(try_login("10.0.0.1", "wrong-password-1").await.status, StatusCode::UNAUTHORIZED, "within the limit, requests are handled normally");
    }
    let limited = try_login("10.0.0.1", PASSWORD).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS, "even the right password is turned away once over the limit");
    assert_eq!(limited.body["error"]["code"], "rate_limited");
    assert!(limited.retry_after.as_deref().and_then(|s| s.parse::<i64>().ok()).is_some_and(|n| n >= 1));
    // The reset endpoint shares the bucket; another address is unaffected.
    assert_eq!(from_ip(&api, "10.0.0.1", Method::POST, "/api/v1/auth/password/forgot", None, Some(json!({ "email": "a@b.test" }))).await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(try_login("10.0.0.2", PASSWORD).await.status, StatusCode::OK);
    // Spoofed early entries in X-Forwarded-For do not help: only the address the proxy appended counts.
    let spoof = api.call_h(Method::POST, "/api/v1/auth/login", None, Some(json!({ "email": "owner@amiable.test", "password": PASSWORD })), &[("x-forwarded-for", "9.9.9.9, 10.0.0.1")]).await;
    assert_eq!(spoof.status, StatusCode::TOO_MANY_REQUESTS);
    // A new window starts fresh.
    sqlx::query("DELETE FROM rate_limits").execute(&api.pool).await.unwrap();
    assert_eq!(try_login("10.0.0.1", PASSWORD).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr20_signed_in_users_are_limited_separately_and_writes_are_stricter(pool: PgPool) {
    let setup = Api::new(pool.clone());
    let owner = setup.setup().await;
    let lagos = setup.store(&owner, "Lagos").await;
    let (_, cashier) = setup.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    let api = Api::with_config(pool, Config { rate_read_per_window: 5, rate_write_per_window: 2, ..limits() });

    for _ in 0..5 {
        assert_eq!(api.get("/api/v1/me", &cashier).await.status, StatusCode::OK);
    }
    assert_eq!(api.get("/api/v1/me", &cashier).await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(api.get("/api/v1/me", &owner).await.status, StatusCode::OK, "another user has their own allowance");
    let customer = |n: u32| json!({ "name": format!("Customer {n}") });
    assert_eq!(api.post("/api/v1/customers", &cashier, customer(1)).await.status, StatusCode::CREATED, "reads and writes are counted apart");
    assert_eq!(api.post("/api/v1/customers", &cashier, customer(2)).await.status, StatusCode::CREATED);
    let third = api.post("/api/v1/customers", &cashier, customer(3)).await;
    assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM customers").fetch_one(&api.pool).await.unwrap();
    assert_eq!(n, 2, "a rejected request creates nothing");

    for _ in 0..30 {
        assert_eq!(api.get("/health", "x").await.status, StatusCode::OK, "the health check is never limited");
    }
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr20_inventing_new_tokens_does_not_escape_the_per_address_ceiling(pool: PgPool) {
    let api = Api::with_config(pool, Config { rate_ip_per_window: 4, ..limits() });
    for i in 0..4 {
        let r = from_ip(&api, "10.0.0.9", Method::GET, "/api/v1/me", Some(&format!("made-up-token-{i}")), None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(from_ip(&api, "10.0.0.9", Method::GET, "/api/v1/me", Some("yet-another-token"), None).await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(from_ip(&api, "10.0.0.10", Method::GET, "/api/v1/me", Some("yet-another-token"), None).await.status, StatusCode::UNAUTHORIZED, "other addresses are fine");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn nfr20_a_rate_limited_sale_changes_nothing(pool: PgPool) {
    let setup = Api::new(pool.clone());
    let owner = setup.setup().await;
    let lagos = setup.store(&owner, "Lagos").await;
    let (_, cashier) = setup.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    let (_, officer) = setup.employee(&owner, "inv@amiable.test", "inventory_officer", Some(lagos)).await;
    setup.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 0 })).await;
    let rice: Uuid = setup.post("/api/v1/products", &owner, json!({ "name": "Rice", "variants": [{ "sku": "RICE", "price_minor": 1000 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse().unwrap();
    setup.post(&format!("/api/v1/stores/{lagos}/stock/receipts"), &officer, json!({ "lines": [{ "variant_id": rice, "quantity": 10 }] })).await;

    let api = Api::with_config(pool, Config { rate_write_per_window: 1, ..limits() });
    let url = format!("/api/v1/stores/{lagos}/sales");
    let body = json!({ "lines": [{ "variant_id": rice, "quantity": 1 }], "payments": [{ "method": "cash", "amount_minor": 1000 }] });
    assert_eq!(api.post_idem(&url, &cashier, "limit-key-001", body.clone()).await.status, StatusCode::CREATED);
    assert_eq!(api.post_idem(&url, &cashier, "limit-key-002", body).await.status, StatusCode::TOO_MANY_REQUESTS);
    let sales: i64 = sqlx::query_scalar("SELECT count(*) FROM sales").fetch_one(&api.pool).await.unwrap();
    let on_hand: i64 = sqlx::query_scalar("SELECT on_hand FROM stock_balances WHERE variant_id = $1").bind(rice).fetch_one(&api.pool).await.unwrap();
    assert_eq!((sales, on_hand), (1, 9));
}
