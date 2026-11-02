//! FR-04 accounts and sessions, FR-05 roles, FR-06 store-level access.
mod common;
use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_login_logout_and_unauthenticated_requests(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    assert_eq!(api.call(Method::GET, "/api/v1/me", None, None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.get("/api/v1/me", "not-a-real-token").await.status, StatusCode::UNAUTHORIZED);

    let me = api.get("/api/v1/me", &owner).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["roles"][0]["role"], "business_owner");

    assert_eq!(api.post("/api/v1/auth/logout", &owner, json!({})).await.status, StatusCode::OK);
    assert_eq!(api.get("/api/v1/me", &owner).await.status, StatusCode::UNAUTHORIZED, "a logged-out token stops working");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_passwords_and_tokens_are_not_stored_in_the_clear(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let (hash,): (String,) = sqlx::query_as("SELECT password_hash FROM users").fetch_one(&api.pool).await.unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains(PASSWORD));
    let stored: Vec<(String,)> = sqlx::query_as("SELECT token_hash FROM sessions").fetch_all(&api.pool).await.unwrap();
    assert!(stored.iter().all(|(h,)| h != &owner), "only a hash of the token is kept");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_five_wrong_passwords_lock_the_account(pool: PgPool) {
    let api = Api::new(pool);
    api.setup().await;
    for _ in 0..4 {
        assert_eq!(api.login("owner@amiable.test", "wrong-password-1").await.status, StatusCode::UNAUTHORIZED);
    }
    // Not locked yet: the right password still works and clears the count.
    assert_eq!(api.login("owner@amiable.test", PASSWORD).await.status, StatusCode::OK);
    for _ in 0..5 {
        assert_eq!(api.login("owner@amiable.test", "wrong-password-1").await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(api.login("owner@amiable.test", PASSWORD).await.status, StatusCode::UNAUTHORIZED, "locked even for the right password");
    assert_eq!(api.audit_count("login_failed_account_locked", "denied").await, 1);
    assert_eq!(api.audit_count("login_while_locked", "denied").await, 1);

    // The lock ends by itself.
    sqlx::query("UPDATE users SET locked_until = now() - interval '1 minute'").execute(&api.pool).await.unwrap();
    assert_eq!(api.login("owner@amiable.test", PASSWORD).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_unknown_email_and_wrong_password_look_the_same(pool: PgPool) {
    let api = Api::new(pool);
    api.setup().await;
    let a = api.login("nobody@amiable.test", PASSWORD).await;
    let b = api.login("owner@amiable.test", "wrong-password-1").await;
    assert_eq!(a.status, b.status);
    assert_eq!(a.body, b.body);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_password_reset_is_single_use_and_ends_old_sessions(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;

    // Unknown emails get the same answer and no token.
    let unknown = api.call(Method::POST, "/api/v1/auth/password/forgot", None, Some(json!({ "email": "ghost@amiable.test" }))).await;
    assert_eq!(unknown.status, StatusCode::OK);
    assert!(api.notifier.0.lock().unwrap().is_empty());

    let known = api.call(Method::POST, "/api/v1/auth/password/forgot", None, Some(json!({ "email": "owner@amiable.test" }))).await;
    assert_eq!(known.body, unknown.body);
    let token = api.notifier.0.lock().unwrap()[0].1.clone();

    let weak = api.call(Method::POST, "/api/v1/auth/password/reset", None, Some(json!({ "token": token, "new_password": "short" }))).await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);

    let new_pw = "a-brand-new-pass-9";
    let done = api.call(Method::POST, "/api/v1/auth/password/reset", None, Some(json!({ "token": token, "new_password": new_pw }))).await;
    assert_eq!(done.status, StatusCode::OK);
    assert_eq!(api.get("/api/v1/me", &owner).await.status, StatusCode::UNAUTHORIZED, "old sessions are revoked");
    assert_eq!(api.login("owner@amiable.test", PASSWORD).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.login("owner@amiable.test", new_pw).await.status, StatusCode::OK);

    let reuse = api.call(Method::POST, "/api/v1/auth/password/reset", None, Some(json!({ "token": token, "new_password": "yet-another-pass-1" }))).await;
    assert_eq!(reuse.status, StatusCode::BAD_REQUEST, "a token works once");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_expired_reset_token_is_refused(pool: PgPool) {
    let api = Api::new(pool);
    api.setup().await;
    api.call(Method::POST, "/api/v1/auth/password/forgot", None, Some(json!({ "email": "owner@amiable.test" }))).await;
    let token = api.notifier.0.lock().unwrap()[0].1.clone();
    sqlx::query("UPDATE password_reset_tokens SET expires_at = now() - interval '1 minute'").execute(&api.pool).await.unwrap();
    let r = api.call(Method::POST, "/api/v1/auth/password/reset", None, Some(json!({ "token": token, "new_password": "a-brand-new-pass-9" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_changing_password_signs_out_other_devices(pool: PgPool) {
    let api = Api::new(pool);
    let first = api.setup().await;
    let second = api.token("owner@amiable.test").await;
    let wrong = api.post("/api/v1/auth/password/change", &second, json!({ "current_password": "nope-nope-123", "new_password": "a-brand-new-pass-9" })).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    let ok = api.post("/api/v1/auth/password/change", &second, json!({ "current_password": PASSWORD, "new_password": "a-brand-new-pass-9" })).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(api.get("/api/v1/me", &first).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.get("/api/v1/me", &second).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_sessions_expire_when_idle_and_admins_time_out_sooner(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let (_, cashier) = api.employee(&owner, "c@amiable.test", "cashier", Some(lagos)).await;

    // 20 minutes of silence: past the 15-minute admin limit, inside the 60-minute staff limit.
    sqlx::query("UPDATE sessions SET last_used_at = now() - interval '20 minutes'").execute(&api.pool).await.unwrap();
    assert_eq!(api.get("/api/v1/me", &owner).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.get("/api/v1/me", &cashier).await.status, StatusCode::OK);

    sqlx::query("UPDATE sessions SET last_used_at = now() - interval '2 hours'").execute(&api.pool).await.unwrap();
    assert_eq!(api.get("/api/v1/me", &cashier).await.status, StatusCode::UNAUTHORIZED);

    let fresh = api.token("c@amiable.test").await;
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'").execute(&api.pool).await.unwrap();
    assert_eq!(api.get("/api/v1/me", &fresh).await.status, StatusCode::UNAUTHORIZED, "absolute expiry");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr04_deactivated_employee_is_signed_out_and_cannot_sign_in(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let (id, cashier) = api.employee(&owner, "c@amiable.test", "cashier", Some(lagos)).await;

    let off = api.patch(&format!("/api/v1/employees/{id}"), &owner, json!({ "is_active": false })).await;
    assert_eq!(off.status, StatusCode::OK);
    assert_eq!(api.get("/api/v1/me", &cashier).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.login("c@amiable.test", PASSWORD).await.status, StatusCode::UNAUTHORIZED);

    api.patch(&format!("/api/v1/employees/{id}"), &owner, json!({ "is_active": true })).await;
    assert_eq!(api.login("c@amiable.test", PASSWORD).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr05_one_person_can_hold_different_roles_in_different_stores(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let kaduna = api.store(&owner, "Kaduna").await;
    let (id, token) = api.employee(&owner, "bola@amiable.test", "store_manager", Some(lagos)).await;
    let g = api.post(&format!("/api/v1/employees/{id}/roles"), &owner, json!({ "role": "cashier", "store_id": abuja })).await;
    assert_eq!(g.status, StatusCode::CREATED);
    let dup = api.post(&format!("/api/v1/employees/{id}/roles"), &owner, json!({ "role": "cashier", "store_id": abuja })).await;
    assert_eq!(dup.status, StatusCode::CONFLICT);

    // Manager rights in Lagos…
    assert_eq!(api.put(&format!("/api/v1/stores/{lagos}/settings"), &token, json!({ "receipt_prefix": "LAG" })).await.status, StatusCode::OK);
    // …but only cashier rights in Abuja, and nothing in Kaduna.
    assert_eq!(api.put(&format!("/api/v1/stores/{abuja}/settings"), &token, json!({ "receipt_prefix": "ABJ" })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.get(&format!("/api/v1/stores/{abuja}"), &token).await.status, StatusCode::OK);
    assert_eq!(api.get(&format!("/api/v1/stores/{kaduna}"), &token).await.status, StatusCode::FORBIDDEN);

    let visible = api.get("/api/v1/stores", &token).await;
    let names: Vec<&str> = visible.body.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Abuja", "Lagos"]);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr05_role_and_store_must_agree(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let body = |role: &str, store: Option<uuid::Uuid>| json!({ "email": format!("{role}@amiable.test"), "full_name": "X", "password": PASSWORD, "role": role, "store_id": store });
    assert_eq!(api.post("/api/v1/employees", &owner, body("cashier", None)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.post("/api/v1/employees", &owner, body("administrator", Some(lagos))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.post("/api/v1/employees", &owner, body("wizard", Some(lagos))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.post("/api/v1/employees", &owner, body("cashier", Some(uuid::Uuid::new_v4()))).await.status, StatusCode::NOT_FOUND);
    let roles = api.get("/api/v1/roles", &owner).await;
    assert_eq!(roles.body["roles"].as_array().unwrap().len(), 6);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr05_only_the_owner_hands_out_business_wide_roles(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let (admin_id, admin) = api.employee(&owner, "admin@amiable.test", "administrator", None).await;
    let lagos = api.store(&admin, "Lagos").await; // an administrator can run the business day to day

    let try_owner = api.post("/api/v1/employees", &admin, json!({ "email": "o2@amiable.test", "full_name": "O", "password": PASSWORD, "role": "business_owner" })).await;
    assert_eq!(try_owner.status, StatusCode::FORBIDDEN);
    let (_, _) = api.employee(&admin, "m@amiable.test", "store_manager", Some(lagos)).await;

    // An administrator cannot remove or deactivate the owner either.
    let owner_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = 'owner@amiable.test'").fetch_one(&api.pool).await.unwrap();
    assert_eq!(api.patch(&format!("/api/v1/employees/{owner_id}"), &admin, json!({ "is_active": false })).await.status, StatusCode::FORBIDDEN);
    let _ = admin_id;
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr05_the_last_owner_cannot_be_removed(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let owner_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users").fetch_one(&api.pool).await.unwrap();
    assert_eq!(api.patch(&format!("/api/v1/employees/{owner_id}"), &owner, json!({ "is_active": false })).await.status, StatusCode::CONFLICT);
    let m: uuid::Uuid = sqlx::query_scalar("SELECT id FROM memberships").fetch_one(&api.pool).await.unwrap();
    assert_eq!(api.delete(&format!("/api/v1/employees/{owner_id}/roles/{m}"), &owner).await.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr06_a_lagos_cashier_cannot_reach_abuja_by_changing_the_store_id(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let (cashier_id, cashier) = api.employee(&owner, "c@amiable.test", "cashier", Some(lagos)).await;

    assert_eq!(api.get(&format!("/api/v1/stores/{lagos}"), &cashier).await.status, StatusCode::OK);
    let swapped = api.get(&format!("/api/v1/stores/{abuja}"), &cashier).await;
    assert_eq!(swapped.status, StatusCode::FORBIDDEN);
    assert_eq!(api.get(&format!("/api/v1/stores/{abuja}/settings"), &cashier).await.status, StatusCode::FORBIDDEN);

    // The refusal is recorded against the cashier and the targeted store.
    let (actor, store): (uuid::Uuid, uuid::Uuid) = sqlx::query_as("SELECT actor_user_id, store_id FROM audit_log WHERE action = 'view_store' AND outcome = 'denied'").fetch_one(&api.pool).await.unwrap();
    assert_eq!((actor, store), (cashier_id, abuja));

    // The cashier's store list only ever contains their own store.
    let list = api.get("/api/v1/stores", &cashier).await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    assert_eq!(list.body[0]["name"], "Lagos");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr06_staff_lists_are_scoped_to_the_managers_stores(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let (_, mgr) = api.employee(&owner, "m@amiable.test", "store_manager", Some(lagos)).await;
    let (_, _) = api.employee(&owner, "lc@amiable.test", "cashier", Some(lagos)).await;
    let (abuja_id, _) = api.employee(&owner, "ac@amiable.test", "cashier", Some(abuja)).await;
    let (_, cashier) = api.employee(&owner, "lc2@amiable.test", "cashier", Some(lagos)).await;

    let list = api.get("/api/v1/employees", &mgr).await;
    let emails: Vec<&str> = list.body.as_array().unwrap().iter().map(|e| e["email"].as_str().unwrap()).collect();
    assert!(emails.contains(&"lc@amiable.test") && emails.contains(&"m@amiable.test"));
    assert!(!emails.contains(&"ac@amiable.test") && !emails.contains(&"owner@amiable.test"));

    assert_eq!(api.get(&format!("/api/v1/employees/{abuja_id}"), &mgr).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.get("/api/v1/employees", &cashier).await.status, StatusCode::FORBIDDEN);
    // Staff cannot create or change accounts at all.
    assert_eq!(api.patch(&format!("/api/v1/employees/{abuja_id}"), &mgr, json!({ "is_active": false })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.get("/api/v1/audit", &mgr).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr06_the_audit_trail_cannot_be_edited_or_deleted(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log").fetch_one(&api.pool).await.unwrap();
    assert!(before > 0);
    assert!(sqlx::query("UPDATE audit_log SET action = 'x'").execute(&api.pool).await.is_err());
    assert!(sqlx::query("DELETE FROM audit_log").execute(&api.pool).await.is_err());
    assert!(sqlx::query("TRUNCATE audit_log").execute(&api.pool).await.is_err());
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log").fetch_one(&api.pool).await.unwrap();
    assert_eq!(before, after);
    assert_eq!(api.get("/api/v1/audit?limit=5", &owner).await.status, StatusCode::OK);
}
