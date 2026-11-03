//! FR-10 store stock, FR-11 movement ledger, FR-12 adjustments and counts, FR-13 batches and expiry,
//! FR-14 low-stock alerts.
mod common;
use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

struct World {
    api: Api,
    owner: String,
    lagos: Uuid,
    abuja: Uuid,
    mgr: String,
    officer: String,
    cashier: String,
}

async fn world(pool: PgPool) -> World {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let (_, mgr) = api.employee(&owner, "mgr@amiable.test", "store_manager", Some(lagos)).await;
    let (_, officer) = api.employee(&owner, "inv@amiable.test", "inventory_officer", Some(lagos)).await;
    let (_, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    World { api, owner, lagos, abuja, mgr, officer, cashier }
}

async fn variant(w: &World, sku: &str, perishable: bool) -> Uuid {
    let r = w.api.post("/api/v1/products", &w.owner, json!({ "name": sku, "tracks_expiry": perishable, "variants": [{ "sku": sku, "price_minor": 1000 }] })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body["variants"][0]["id"].as_str().unwrap().parse().unwrap()
}

fn path(store: Uuid, tail: &str) -> String {
    format!("/api/v1/stores/{store}/stock{tail}")
}

async fn receive(w: &World, store: Uuid, v: Uuid, qty: i64) {
    let r = w.api.post(&path(store, "/receipts"), &w.officer, json!({ "lines": [{ "variant_id": v, "quantity": qty }] })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
}

async fn on_hand(w: &World, store: Uuid, v: Uuid) -> i64 {
    w.api.get(&path(store, &format!("/{v}")), &w.owner).await.body["on_hand"].as_i64().unwrap()
}

fn in_days(n: i64) -> String {
    (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string()
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr10_each_store_holds_its_own_stock(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    receive(&w, w.lagos, v, 100).await;
    let abuja_officer = w.api.employee(&w.owner, "ai@amiable.test", "inventory_officer", Some(w.abuja)).await.1;
    let r = w.api.post(&path(w.abuja, "/receipts"), &abuja_officer, json!({ "lines": [{ "variant_id": v, "quantity": 20 }] })).await;
    assert_eq!(r.status, StatusCode::CREATED);

    assert_eq!(on_hand(&w, w.lagos, v).await, 100);
    assert_eq!(on_hand(&w, w.abuja, v).await, 20);

    // The Lagos cashier can see Lagos availability but nothing of Abuja, and cannot receive stock.
    assert_eq!(w.api.get(&path(w.lagos, ""), &w.cashier).await.status, StatusCode::OK);
    assert_eq!(w.api.get(&path(w.abuja, ""), &w.cashier).await.status, StatusCode::FORBIDDEN);
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.cashier, json!({ "lines": [{ "variant_id": v, "quantity": 1 }] })).await.status, StatusCode::FORBIDDEN);
    // And the Lagos officer cannot touch Abuja's stock.
    assert_eq!(w.api.post(&path(w.abuja, "/receipts"), &w.officer, json!({ "lines": [{ "variant_id": v, "quantity": 1 }] })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(on_hand(&w, w.abuja, v).await, 20);
    assert!(w.audit_denied("receive_stock").await >= 2);
}

impl World {
    async fn audit_denied(&self, action: &str) -> i64 {
        self.api.audit_count(action, "denied").await
    }
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr11_every_change_is_a_ledger_row_and_the_ledger_adds_up(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    receive(&w, w.lagos, v, 100).await;
    let adj = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 7, "reason": "spillage" })).await;
    assert_eq!(adj.status, StatusCode::CREATED);

    let m = w.api.get(&path(w.lagos, &format!("/movements?variant_id={v}")), &w.mgr).await;
    let items = m.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["kind"], "adjust_out");
    assert_eq!(items[0]["quantity_delta"], -7);
    assert_eq!(items[0]["balance_after"], 93);
    assert_eq!(items[0]["reference_type"], "adjustment");
    assert!(items[0]["actor_user_id"].is_string());

    let sum: i64 = sqlx::query_scalar("SELECT COALESCE(sum(quantity_delta),0)::bigint FROM stock_movements WHERE bucket = 'on_hand'").fetch_one(&w.api.pool).await.unwrap();
    assert_eq!(sum, on_hand(&w, w.lagos, v).await);

    // The ledger cannot be rewritten, and neither can balances go below zero at the database level.
    assert!(sqlx::query("UPDATE stock_movements SET quantity_delta = 1").execute(&w.api.pool).await.is_err());
    assert!(sqlx::query("DELETE FROM stock_movements").execute(&w.api.pool).await.is_err());
    assert!(sqlx::query("UPDATE stock_balances SET on_hand = -1").execute(&w.api.pool).await.is_err());
    assert_eq!(w.api.get(&path(w.lagos, "/movements"), &w.cashier).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr11_stock_cannot_go_negative_even_when_requests_race(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    receive(&w, w.lagos, v, 50).await;
    let too_many = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 51, "reason": "x" })).await;
    // 51 is above the approval threshold, so it queues; approving it must then fail rather than go negative.
    assert_eq!(too_many.status, StatusCode::ACCEPTED);
    let approve = w.api.post(&path(w.lagos, &format!("/adjustments/{}/approve", too_many.id("id"))), &w.mgr, json!({})).await;
    assert_eq!(approve.status, StatusCode::CONFLICT);
    assert_eq!(on_hand(&w, w.lagos, v).await, 50);

    // Eight people each try to take 10 of the 50: exactly five can succeed.
    let body = json!({ "variant_id": v, "kind": "decrease", "quantity": 10, "reason": "race" });
    let p = path(w.lagos, "/adjustments");
    let results = futures::future::join_all((0..8).map(|_| w.api.post(&p, &w.officer, body.clone()))).await;
    let ok = results.iter().filter(|r| r.status == StatusCode::CREATED).count();
    let refused = results.iter().filter(|r| r.status == StatusCode::CONFLICT).count();
    assert_eq!((ok, refused), (5, 3), "{:?}", results.iter().map(|r| r.status).collect::<Vec<_>>());
    assert_eq!(on_hand(&w, w.lagos, v).await, 0);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr12_big_adjustments_wait_for_someone_else_to_approve(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    receive(&w, w.lagos, v, 100).await;

    let no_reason = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 1, "reason": "  " })).await;
    assert_eq!(no_reason.status, StatusCode::BAD_REQUEST);

    let small = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 10, "reason": "loss" })).await;
    assert_eq!((small.status, small.body["status"].as_str()), (StatusCode::CREATED, Some("approved")));
    assert_eq!(on_hand(&w, w.lagos, v).await, 90);

    let big = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 60, "reason": "flood damage" })).await;
    assert_eq!((big.status, big.body["status"].as_str()), (StatusCode::ACCEPTED, Some("pending")));
    assert_eq!(on_hand(&w, w.lagos, v).await, 90, "nothing moves until it is approved");
    let id = big.id("id");
    let approve_path = path(w.lagos, &format!("/adjustments/{id}/approve"));
    assert_eq!(w.api.post(&approve_path, &w.officer, json!({})).await.status, StatusCode::FORBIDDEN, "the requester's own role cannot approve");
    assert_eq!(w.api.post(&approve_path, &w.cashier, json!({})).await.status, StatusCode::FORBIDDEN);
    let done = w.api.post(&approve_path, &w.mgr, json!({})).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(on_hand(&w, w.lagos, v).await, 30);
    assert_eq!(w.api.post(&approve_path, &w.mgr, json!({})).await.status, StatusCode::CONFLICT, "decided once");

    // A manager cannot approve what they asked for themselves.
    let own = w.api.post(&path(w.lagos, "/adjustments"), &w.mgr, json!({ "variant_id": v, "kind": "increase", "quantity": 80, "reason": "found stock" })).await;
    assert_eq!(own.status, StatusCode::ACCEPTED);
    let own_path = path(w.lagos, &format!("/adjustments/{}/approve", own.id("id")));
    assert_eq!(w.api.post(&own_path, &w.mgr, json!({})).await.status, StatusCode::FORBIDDEN);
    assert_eq!(w.api.post(&own_path, &w.owner, json!({})).await.status, StatusCode::OK);
    assert_eq!(on_hand(&w, w.lagos, v).await, 110);

    let rej = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 100, "reason": "x" })).await;
    let rejected = w.api.post(&path(w.lagos, &format!("/adjustments/{}/reject", rej.id("id"))), &w.mgr, json!({ "note": "not credible" })).await;
    assert_eq!(rejected.body["status"], "rejected");
    assert_eq!(on_hand(&w, w.lagos, v).await, 110);
    let pending = w.api.get(&path(w.lagos, "/adjustments?status=pending"), &w.mgr).await;
    assert_eq!(pending.body.as_array().unwrap().len(), 0);
    assert!(w.api.audit_count("approve_adjustment", "success").await >= 2);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr12_the_approval_threshold_is_set_per_store_by_the_manager(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    receive(&w, w.lagos, v, 100).await;
    assert_eq!(w.api.put(&path(w.lagos, "/settings"), &w.officer, json!({ "adjustment_approval_threshold": 1000 })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(w.api.put(&path(w.lagos, "/settings"), &w.mgr, json!({ "adjustment_approval_threshold": 5 })).await.status, StatusCode::OK);
    let r = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 6, "reason": "loss" })).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(w.api.get(&path(w.lagos, "/settings"), &w.cashier).await.body["adjustment_approval_threshold"], 5);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr12_a_count_fixes_small_differences_and_queues_large_ones(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    receive(&w, w.lagos, v, 100).await;

    let small = w.api.post(&path(w.lagos, "/counts"), &w.officer, json!({ "lines": [{ "variant_id": v, "counted": 97 }] })).await;
    assert_eq!(small.status, StatusCode::CREATED, "{}", small.body);
    assert_eq!(small.body["lines"][0]["system"], 100);
    assert_eq!(small.body["lines"][0]["variance"], -3);
    assert_eq!(small.body["lines"][0]["adjustment_status"], "approved");
    assert_eq!(on_hand(&w, w.lagos, v).await, 97);

    let same = w.api.post(&path(w.lagos, "/counts"), &w.officer, json!({ "lines": [{ "variant_id": v, "counted": 97 }] })).await;
    assert_eq!(same.body["lines"][0]["variance"], 0);
    assert!(same.body["lines"][0]["adjustment_id"].is_null());

    let big = w.api.post(&path(w.lagos, "/counts"), &w.officer, json!({ "lines": [{ "variant_id": v, "counted": 10 }] })).await;
    assert_eq!(big.body["lines"][0]["adjustment_status"], "pending");
    assert_eq!(on_hand(&w, w.lagos, v).await, 97, "a large variance waits for approval");
    let adj_id = big.body["lines"][0]["adjustment_id"].as_str().unwrap();
    w.api.post(&path(w.lagos, &format!("/adjustments/{adj_id}/approve")), &w.mgr, json!({})).await;
    assert_eq!(on_hand(&w, w.lagos, v).await, 10);

    let dup = w.api.post(&path(w.lagos, "/counts"), &w.officer, json!({ "lines": [{ "variant_id": v, "counted": 1 }, { "variant_id": v, "counted": 2 }] })).await;
    assert_eq!(dup.status, StatusCode::BAD_REQUEST);
    assert_eq!(w.api.post(&path(w.lagos, "/counts"), &w.cashier, json!({ "lines": [{ "variant_id": v, "counted": 1 }] })).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr13_batches_are_used_earliest_expiry_first(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "MILK", true).await;
    let line = |code: &str, qty: i64, expiry: Option<String>| {
        let mut l = json!({ "variant_id": v, "quantity": qty, "batch_code": code });
        if let Some(e) = expiry { l["expiry_date"] = json!(e); }
        json!({ "lines": [l] })
    };
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, json!({ "lines": [{ "variant_id": v, "quantity": 5 }] })).await.status, StatusCode::BAD_REQUEST, "a perishable needs a batch");
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, line("A", 5, None)).await.status, StatusCode::BAD_REQUEST, "and an expiry date");
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, line("OLD", 5, Some(in_days(-1)))).await.status, StatusCode::BAD_REQUEST, "expired stock is not received");

    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, line("LATE", 5, Some(in_days(20)))).await.status, StatusCode::CREATED);
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, line("SOON", 5, Some(in_days(5)))).await.status, StatusCode::CREATED);
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, line("SOON", 2, Some(in_days(9)))).await.status, StatusCode::CONFLICT, "same code, different expiry");
    assert_eq!(w.api.post(&path(w.lagos, "/receipts"), &w.officer, line("SOON", 3, Some(in_days(5)))).await.status, StatusCode::CREATED, "topping up a batch");
    assert_eq!(on_hand(&w, w.lagos, v).await, 13);

    // Take 10 with no batch named: the earliest-expiring batch goes first.
    let r = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 10, "reason": "use" })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let detail = w.api.get(&path(w.lagos, &format!("/{v}")), &w.owner).await;
    let left: Vec<(String, i64)> = detail.body["batches"].as_array().unwrap().iter().map(|b| (b["batch_code"].as_str().unwrap().to_string(), b["quantity"].as_i64().unwrap())).collect();
    assert_eq!(left, vec![("LATE".to_string(), 3)], "SOON (8) went first, then 2 from LATE");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr13_fefo_skips_expired_batches_and_the_sweep_separates_them(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "MILK", true).await;
    for (code, qty, days) in [("LATE", 5, 20), ("SOON", 8, 5)] {
        let r = w.api.post(&path(w.lagos, "/receipts"), &w.officer, json!({ "lines": [{ "variant_id": v, "quantity": qty, "batch_code": code, "expiry_date": in_days(days) }] })).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    }
    let r = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 10, "reason": "use" })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let detail = w.api.get(&path(w.lagos, &format!("/{v}")), &w.owner).await;
    assert_eq!(detail.body["on_hand"], 3);
    let batches = detail.body["batches"].as_array().unwrap();
    assert_eq!(batches.len(), 1, "SOON is used up first");
    assert_eq!((batches[0]["batch_code"].as_str(), batches[0]["quantity"].as_i64()), (Some("LATE"), Some(3)));

    // The batch quantities always add up to on_hand.
    let sum: i64 = sqlx::query_scalar("SELECT COALESCE(sum(quantity),0)::bigint FROM stock_batches").fetch_one(&w.api.pool).await.unwrap();
    assert_eq!(sum, 3);

    // Time passes: LATE expires. It can no longer be used, and a sweep moves it out of sellable stock.
    sqlx::query("UPDATE stock_batches SET expiry_date = current_date - 3").execute(&w.api.pool).await.unwrap();
    let blocked = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 1, "reason": "use" })).await;
    assert_eq!(blocked.status, StatusCode::CONFLICT, "expired stock is never handed out");
    let listed = w.api.get(&path(w.lagos, "/expiring?days=0"), &w.officer).await;
    assert_eq!(listed.body["items"][0]["expired"], true);

    let sweep = w.api.post(&path(w.lagos, "/expire"), &w.officer, json!({})).await;
    assert_eq!(sweep.body["moved"].as_array().unwrap().len(), 1);
    let after = w.api.get(&path(w.lagos, &format!("/{v}")), &w.owner).await;
    assert_eq!((after.body["on_hand"].as_i64(), after.body["expired"].as_i64()), (Some(0), Some(3)));
    assert_eq!(w.api.post(&path(w.lagos, "/expire"), &w.officer, json!({})).await.body["moved"].as_array().unwrap().len(), 0, "running it again changes nothing");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr13_damaged_goods_are_kept_apart_from_sellable_stock(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "GLASS", false).await;
    receive(&w, w.lagos, v, 20).await;
    let r = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "damage", "quantity": 5, "reason": "dropped crate" })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let b = w.api.get(&path(w.lagos, &format!("/{v}")), &w.owner).await;
    assert_eq!((b.body["on_hand"].as_i64(), b.body["damaged"].as_i64()), (Some(15), Some(5)));

    // Damaged units cannot be written off again as sellable, and the two ledger rows are linked.
    let m = w.api.get(&path(w.lagos, "/movements"), &w.mgr).await;
    let rows: Vec<&Value> = m.body["items"].as_array().unwrap().iter().filter(|r| r["kind"] == "damage").collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["group_id"], rows[1]["group_id"]);
    let too_many = w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": 16, "reason": "x" })).await;
    assert_eq!(too_many.status, StatusCode::CONFLICT, "only the 15 sellable units can be taken, not the damaged ones");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr14_low_stock_raises_one_alert_and_clears_when_restocked(pool: PgPool) {
    let w = world(pool).await;
    let v = variant(&w, "RICE", false).await;
    let set = w.api.put(&path(w.lagos, &format!("/reorder-levels/{v}")), &w.officer, json!({ "reorder_level": 10 })).await;
    assert_eq!(set.status, StatusCode::OK);
    receive(&w, w.lagos, v, 12).await;
    let open = || async { w.api.get(&path(w.lagos, "/alerts"), &w.mgr).await.body["items"].as_array().unwrap().len() };
    assert_eq!(open().await, 0);

    async fn dec(w: &World, v: Uuid, q: i64) {
        w.api.post(&path(w.lagos, "/adjustments"), &w.officer, json!({ "variant_id": v, "kind": "decrease", "quantity": q, "reason": "sale" })).await;
    }
    dec(&w, v, 1).await; // 11: still above
    assert_eq!(open().await, 0);
    dec(&w, v, 2).await; // 9: at or below the level
    assert_eq!(open().await, 1);
    dec(&w, v, 2).await; // 7: still low, no second alert
    dec(&w, v, 1).await;
    assert_eq!(open().await, 1, "one alert, not one per sale");
    let low = w.api.get(&path(w.lagos, "?low=true"), &w.cashier).await;
    assert_eq!(low.body["items"].as_array().unwrap().len(), 1);

    receive(&w, w.lagos, v, 20).await;
    assert_eq!(open().await, 0, "restocking closes the alert");
    let resolved = w.api.get(&path(w.lagos, "/alerts?status=resolved"), &w.mgr).await;
    // One from when the level was set on an empty shelf (cleared by the first delivery), one from the dip.
    assert_eq!(resolved.body["items"].as_array().unwrap().len(), 2);

    dec(&w, v, 25).await; // 1 left
    assert_eq!(open().await, 1, "a new dip raises a fresh alert");
    let cleared = w.api.put(&path(w.lagos, &format!("/reorder-levels/{v}")), &w.mgr, json!({ "reorder_level": null })).await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(open().await, 0, "removing the level removes the alert");
    assert_eq!(w.api.put(&path(w.lagos, &format!("/reorder-levels/{v}")), &w.cashier, json!({ "reorder_level": 1 })).await.status, StatusCode::FORBIDDEN);
}
