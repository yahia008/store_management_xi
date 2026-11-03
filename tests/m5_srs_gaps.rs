//! Behaviour added after checking the build against the SRS text: FR-02 deactivated stores, FR-07 product
//! details, FR-09 previous prices, FR-10 stock filters, FR-12/14 references and expiry warnings,
//! FR-15 discounts and barcode lines, FR-16 payment statuses and reconciliation, FR-17 search and
//! reprints, FR-18 approvals.
mod common;
use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

struct Shop {
    api: Api,
    owner: String,
    lagos: Uuid,
    mgr: String,
    mgr_id: Uuid,
    cashier: String,
    officer: String,
    rice: Uuid,
    book: Uuid,
}

async fn shop(pool: PgPool) -> Shop {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let (mgr_id, mgr) = api.employee(&owner, "mgr@amiable.test", "store_manager", Some(lagos)).await;
    let (_, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    let (_, officer) = api.employee(&owner, "inv@amiable.test", "inventory_officer", Some(lagos)).await;
    api.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 750 })).await;
    let cat = api.post("/api/v1/categories", &owner, json!({ "name": "Grocery" })).await.id("id");
    let rice = api.post("/api/v1/products", &owner, json!({ "name": "Rice", "category_id": cat, "variants": [{ "sku": "RICE", "price_minor": 1000 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse().unwrap();
    let book = api.post("/api/v1/products", &owner, json!({ "name": "Book", "tax_exempt": true, "variants": [{ "sku": "BOOK", "barcode": "B-1", "price_minor": 500 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse().unwrap();
    for v in [rice, book] {
        let r = api.post(&format!("/api/v1/stores/{lagos}/stock/receipts"), &officer, json!({ "lines": [{ "variant_id": v, "quantity": 100 }] })).await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    Shop { api, owner, lagos, mgr, mgr_id, cashier, officer, rice, book }
}

impl Shop {
    fn sales(&self) -> String {
        format!("/api/v1/stores/{}/sales", self.lagos)
    }
    fn stock(&self, tail: &str) -> String {
        format!("/api/v1/stores/{}/stock{tail}", self.lagos)
    }
    async fn sell_as(&self, token: &str, key: &str, lines: Value, payments: Value) -> Resp {
        self.api.post_idem(&self.sales(), token, key, json!({ "lines": lines, "payments": payments })).await
    }
    async fn sell(&self, key: &str, lines: Value, payments: Value) -> Resp {
        self.sell_as(&self.cashier, key, lines, payments).await
    }
}

fn cash(n: i64) -> Value {
    json!([{ "method": "cash", "amount_minor": n }])
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr02_a_deactivated_store_takes_no_new_sales_or_stock_movements(pool: PgPool) {
    let s = shop(pool).await;
    let sale = s.sell("deact-key-001", json!([{ "variant_id": s.rice, "quantity": 2 }]), cash(3000)).await;
    let (sid, line) = (sale.body["id"].as_str().unwrap().to_string(), sale.body["lines"][0]["id"].as_str().unwrap().to_string());
    s.api.patch(&format!("/api/v1/stores/{}", s.lagos), &s.owner, json!({ "status": "inactive" })).await;

    let receipt = json!({ "lines": [{ "variant_id": s.rice, "quantity": 1 }] });
    assert_eq!(s.api.post(&s.stock("/receipts"), &s.officer, receipt.clone()).await.status, StatusCode::CONFLICT);
    assert_eq!(s.api.post(&s.stock("/adjustments"), &s.officer, json!({ "variant_id": s.rice, "kind": "decrease", "quantity": 1, "reason": "x" })).await.status, StatusCode::CONFLICT);
    assert_eq!(s.api.post(&s.stock("/counts"), &s.officer, json!({ "lines": [{ "variant_id": s.rice, "counted": 1 }] })).await.status, StatusCode::CONFLICT);
    assert_eq!(s.api.post(&s.stock("/expire"), &s.mgr, json!({})).await.status, StatusCode::CONFLICT);
    assert_eq!(s.sell("deact-key-002", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await.status, StatusCode::CONFLICT);
    let ret = json!({ "reason": "x", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] });
    assert_eq!(s.api.post_idem(&format!("/api/v1/sales/{sid}/returns"), &s.cashier, "deact-key-003", ret).await.status, StatusCode::CONFLICT);

    // History stays readable, and an owner can still do administrative stock work.
    assert_eq!(s.api.get(&format!("/api/v1/sales/{sid}"), &s.mgr).await.status, StatusCode::OK);
    assert_eq!(s.api.get(&s.stock(""), &s.mgr).await.status, StatusCode::OK);
    assert_eq!(s.api.post(&s.stock("/receipts"), &s.owner, receipt).await.status, StatusCode::CREATED);
    s.api.patch(&format!("/api/v1/stores/{}", s.lagos), &s.owner, json!({ "status": "active" })).await;
    assert_eq!(s.api.post(&s.stock("/adjustments"), &s.officer, json!({ "variant_id": s.rice, "kind": "decrease", "quantity": 1, "reason": "x" })).await.status, StatusCode::CREATED);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr02_stores_can_be_searched_by_name_or_identifier(pool: PgPool) {
    let s = shop(pool).await;
    s.api.store(&s.owner, "Abuja").await;
    assert_eq!(s.api.get("/api/v1/stores?q=lag", &s.owner).await.body.as_array().unwrap().len(), 1);
    assert_eq!(s.api.get(&format!("/api/v1/stores?q={}", s.lagos), &s.owner).await.body[0]["name"], "Lagos");
    assert_eq!(s.api.get("/api/v1/stores?q=nowhere", &s.owner).await.body.as_array().unwrap().len(), 0);
    assert_eq!(s.api.get("/api/v1/stores", &s.owner).await.body.as_array().unwrap().len(), 2);
    // A manager searching still only sees their own store.
    assert_eq!(s.api.get("/api/v1/stores?q=abuja", &s.mgr).await.body.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr07_products_carry_brand_image_and_extra_attributes(pool: PgPool) {
    let s = shop(pool).await;
    let r = s.api.post("/api/v1/products", &s.owner, json!({
        "name": "Long-grain rice", "brand": "Mama Gold", "image_url": "https://img.test/rice.png", "attributes": { "origin": "Kebbi", "organic": true },
        "variants": [{ "sku": "MG-RICE", "price_minor": 900 }] })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!((r.body["brand"].as_str(), r.body["attributes"]["origin"].as_str()), (Some("Mama Gold"), Some("Kebbi")));
    assert_eq!(s.api.get("/api/v1/products?q=mama", &s.cashier).await.body["total"], 1, "searchable by brand");
    let upd = s.api.patch(&format!("/api/v1/products/{}", r.id("id")), &s.owner, json!({ "brand": "Golden Penny", "returnable": false })).await;
    assert_eq!((upd.body["brand"].as_str(), upd.body["returnable"].as_bool()), (Some("Golden Penny"), Some(false)));
    assert_eq!(s.api.post("/api/v1/products", &s.owner, json!({ "name": "X", "attributes": [1], "variants": [{ "sku": "X1", "price_minor": 1 }] })).await.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr09_each_price_records_the_price_it_replaced(pool: PgPool) {
    let s = shop(pool).await;
    let prices = format!("/api/v1/variants/{}/prices", s.rice);
    s.api.post(&prices, &s.owner, json!({ "price_minor": 1200 })).await;
    let store_first = s.api.post(&prices, &s.mgr, json!({ "price_minor": 1500, "store_id": s.lagos })).await;
    assert_eq!(store_first.body["previous_price_minor"], 1200, "a store's first own price replaces the business price in force");
    let store_next = s.api.post(&prices, &s.mgr, json!({ "price_minor": 1600, "store_id": s.lagos })).await;
    assert_eq!(store_next.body["previous_price_minor"], 1500);
    let hist = s.api.get(&prices, &s.owner).await;
    let rows: Vec<(i64, Option<i64>)> = hist.body["history"].as_array().unwrap().iter().map(|r| (r["price_minor"].as_i64().unwrap(), r["previous_price_minor"].as_i64())).collect();
    assert_eq!(rows, vec![(1600, Some(1500)), (1500, Some(1200)), (1200, Some(1000)), (1000, None)]);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr10_stock_can_be_filtered_and_shows_what_is_really_available(pool: PgPool) {
    let s = shop(pool).await;
    let milk = s.api.post("/api/v1/products", &s.owner, json!({ "name": "Milk", "tracks_expiry": true, "variants": [{ "sku": "MILK", "price_minor": 400 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let expiry = (chrono::Utc::now().date_naive() + chrono::Duration::days(5)).to_string();
    s.api.post(&s.stock("/receipts"), &s.officer, json!({ "lines": [{ "variant_id": milk, "quantity": 3, "batch_code": "M1", "expiry_date": expiry }] })).await;
    s.sell("filter-key-01", json!([{ "variant_id": s.book, "quantity": 100 }]), cash(60_000)).await; // book is now out of stock

    let items = |v: Value| -> Vec<String> { v["items"].as_array().unwrap().iter().map(|i| i["sku"].as_str().unwrap().to_string()).collect() };
    assert_eq!(items(s.api.get(&s.stock("?q=ric"), &s.cashier).await.body), vec!["RICE"]);
    assert_eq!(items(s.api.get(&s.stock("?status=out_of_stock"), &s.cashier).await.body), vec!["BOOK"]);
    assert_eq!(items(s.api.get(&s.stock("?status=in_stock"), &s.cashier).await.body), vec!["MILK", "RICE"]);
    let cat = s.api.get("/api/v1/categories", &s.owner).await.body[0]["id"].as_str().unwrap().to_string();
    assert_eq!(items(s.api.get(&s.stock(&format!("?category_id={cat}")), &s.cashier).await.body), vec!["RICE"]);
    assert_eq!(s.api.get(&s.stock("?status=bogus"), &s.cashier).await.status, StatusCode::BAD_REQUEST);

    // Once a batch has expired it is still on hand until swept, but it is no longer available to sell.
    sqlx::query("UPDATE stock_batches SET expiry_date = current_date - 2").execute(&s.api.pool).await.unwrap();
    let all = s.api.get(&s.stock("?q=milk"), &s.cashier).await;
    assert_eq!((all.body["items"][0]["on_hand"].as_i64(), all.body["items"][0]["available"].as_i64()), (Some(3), Some(0)));
    assert_eq!(all.body["items"][0]["reserved"], 0);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr12_fr14_adjustments_keep_a_reference_and_the_expiry_warning_is_a_setting(pool: PgPool) {
    let s = shop(pool).await;
    let adj = s.api.post(&s.stock("/adjustments"), &s.officer, json!({ "variant_id": s.rice, "kind": "decrease", "quantity": 2, "reason": "water damage", "reference": "INC-77" })).await;
    assert_eq!(adj.body["reference"], "INC-77");
    assert_eq!(s.api.get(&s.stock("/adjustments"), &s.mgr).await.body[0]["reference"], "INC-77");

    let milk = s.api.post("/api/v1/products", &s.owner, json!({ "name": "Milk", "tracks_expiry": true, "variants": [{ "sku": "MILK", "price_minor": 400 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let day = |n: i64| (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string();
    for (code, days) in [("SOON", 7), ("FAR", 20)] {
        s.api.post(&s.stock("/receipts"), &s.officer, json!({ "lines": [{ "variant_id": milk, "quantity": 1, "batch_code": code, "expiry_date": day(days) }] })).await;
    }
    let default = s.api.get(&s.stock("/expiring"), &s.mgr).await;
    assert_eq!((default.body["warning_days"].as_i64(), default.body["items"].as_array().unwrap().len()), (Some(30), 2));
    assert_eq!(s.api.put(&s.stock("/settings"), &s.mgr, json!({ "expiry_warning_days": 10 })).await.body["adjustment_approval_threshold"], 50, "other settings are kept");
    let narrow = s.api.get(&s.stock("/expiring"), &s.mgr).await;
    assert_eq!((narrow.body["warning_days"].as_i64(), narrow.body["items"].as_array().unwrap().len()), (Some(10), 1));
    assert_eq!(s.api.put(&s.stock("/settings"), &s.mgr, json!({ "expiry_warning_days": -1 })).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.api.put(&s.stock("/settings"), &s.officer, json!({ "expiry_warning_days": 5 })).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_discounts_barcodes_and_the_quote_before_payment(pool: PgPool) {
    let s = shop(pool).await;
    let quote_url = format!("{}/quote", s.sales());
    let basket = json!([{ "variant_id": s.rice, "quantity": 2, "discount_minor": 200 }, { "barcode": "B-1", "quantity": 1 }, { "sku": "rice", "quantity": 1 }]);
    let quote = s.api.post(&quote_url, &s.cashier, json!({ "lines": basket })).await;
    assert_eq!(quote.status, StatusCode::OK, "{}", quote.body);
    // rice: 2000 - 200 = 1800 (+135 tax); book 500 exempt; rice again 1000 (+75)
    assert_eq!((quote.body["subtotal_minor"].as_i64(), quote.body["tax_minor"].as_i64(), quote.body["total_minor"].as_i64()), (Some(3300), Some(210), Some(3510)));
    let on_hand: i64 = s.api.get(&s.stock(&format!("/{}", s.rice)), &s.owner).await.body["on_hand"].as_i64().unwrap();
    assert_eq!(on_hand, 100, "a quote sells nothing");

    let sale = s.sell("disc-key-0001", basket_for_sale(&s), cash(4000)).await;
    assert_eq!(sale.status, StatusCode::CREATED, "{}", sale.body);
    assert_eq!(sale.body["total_minor"], quote.body["total_minor"], "the sale matches the quote");
    assert!(sale.body["customer_id"].is_null(), "a walk-in sale needs no customer");
    let disc = sale.body["lines"].as_array().unwrap().iter().find(|l| l["discount_minor"] == 200).unwrap();
    assert_eq!((disc["unit_price_minor"].as_i64(), disc["line_subtotal_minor"].as_i64(), disc["line_tax_minor"].as_i64()), (Some(1000), Some(1800), Some(135)));

    // Over the cashier's 10% limit needs a manager.
    let big = json!([{ "variant_id": s.rice, "quantity": 2, "discount_minor": 201 }]);
    assert_eq!(s.sell("disc-key-0002", big.clone(), cash(4000)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.sell_as(&s.mgr, "disc-key-0003", big, cash(4000)).await.status, StatusCode::CREATED);
    assert!(s.api.audit_count("discount_needs_manager", "denied").await >= 1);

    for (bad, why) in [
        (json!([{ "variant_id": s.rice, "sku": "RICE", "quantity": 1 }]), "two identifiers"),
        (json!([{ "quantity": 1 }]), "no identifier"),
        (json!([{ "variant_id": s.rice, "quantity": 1, "discount_minor": 5000 }]), "discount above the line"),
        (json!([{ "variant_id": s.rice, "quantity": 1, "discount_minor": -1 }]), "negative discount"),
    ] {
        assert_eq!(s.api.post(&quote_url, &s.mgr, json!({ "lines": bad })).await.status, StatusCode::BAD_REQUEST, "{why}");
    }
    assert_eq!(s.api.post(&quote_url, &s.mgr, json!({ "lines": [{ "barcode": "NOPE", "quantity": 1 }] })).await.status, StatusCode::NOT_FOUND);
}

fn basket_for_sale(s: &Shop) -> Value {
    json!([{ "variant_id": s.rice, "quantity": 2, "discount_minor": 200 }, { "barcode": "B-1", "quantity": 1 }, { "sku": "rice", "quantity": 1 }])
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr16_a_sale_is_paid_only_when_successful_payments_cover_it(pool: PgPool) {
    let s = shop(pool).await;
    let rice2 = json!([{ "variant_id": s.rice, "quantity": 2 }]); // total 2150
    let pending = s.sell("pay-key-00001", rice2.clone(), json!([{ "method": "card", "amount_minor": 2150, "reference": "POS-9", "status": "pending" }])).await;
    assert_eq!(pending.status, StatusCode::CREATED, "{}", pending.body);
    assert_eq!(pending.body["payment_status"], "pending");
    let (sid, pid) = (pending.body["id"].as_str().unwrap().to_string(), pending.body["payments"][0]["id"].as_str().unwrap().to_string());
    let confirm = s.api.post(&format!("/api/v1/sales/{sid}/payments/{pid}/confirm"), &s.cashier, json!({})).await;
    assert_eq!((confirm.status, confirm.body["payment_status"].as_str()), (StatusCode::OK, Some("paid")));
    assert_eq!(s.api.post(&format!("/api/v1/sales/{sid}/payments/{pid}/confirm"), &s.cashier, json!({})).await.status, StatusCode::CONFLICT, "only a pending payment can be confirmed");

    // A declined card leaves the sale unpaid; the balance can then be paid another way.
    let declined = s.sell("pay-key-00002", rice2.clone(), json!([{ "method": "transfer", "amount_minor": 2150, "status": "pending" }])).await;
    let (did, dpid) = (declined.body["id"].as_str().unwrap().to_string(), declined.body["payments"][0]["id"].as_str().unwrap().to_string());
    let failed = s.api.post(&format!("/api/v1/sales/{did}/payments/{dpid}/fail"), &s.cashier, json!({})).await;
    assert_eq!(failed.body["payment_status"], "unpaid");
    assert_eq!(s.api.post(&format!("/api/v1/sales/{did}/payments"), &s.cashier, json!({ "method": "cash", "amount_minor": 9999 })).await.status, StatusCode::BAD_REQUEST, "more than the balance");
    let settle = s.api.post(&format!("/api/v1/sales/{did}/payments"), &s.cashier, json!({ "method": "other", "amount_minor": 2150, "reference": "VOUCHER-1" })).await;
    assert_eq!(settle.status, StatusCode::CREATED, "{}", settle.body);
    assert_eq!(settle.body["payment_status"], "paid");
    assert_eq!(s.api.post(&format!("/api/v1/sales/{did}/payments"), &s.cashier, json!({ "method": "cash", "amount_minor": 1 })).await.status, StatusCode::CONFLICT, "nothing left to pay");

    // Cash cannot be pending, and unknown statuses are refused.
    assert_eq!(s.sell("pay-key-00003", rice2.clone(), json!([{ "method": "cash", "amount_minor": 2150, "status": "pending" }])).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.sell("pay-key-00004", rice2, json!([{ "method": "card", "amount_minor": 2150, "status": "reversed" }])).await.status, StatusCode::BAD_REQUEST);

    // A succeeded payment can be reversed, but only by a manager.
    assert_eq!(s.api.post(&format!("/api/v1/sales/{sid}/payments/{pid}/reverse"), &s.cashier, json!({})).await.status, StatusCode::FORBIDDEN);
    let rev = s.api.post(&format!("/api/v1/sales/{sid}/payments/{pid}/reverse"), &s.mgr, json!({})).await;
    assert_eq!((rev.status, rev.body["payment_status"].as_str()), (StatusCode::OK, Some("unpaid")));
    assert!(sqlx::query("UPDATE sale_payments SET amount_minor = 1").execute(&s.api.pool).await.is_err(), "only a payment's status may change");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr16_a_repeated_payment_reference_is_not_counted_twice(pool: PgPool) {
    let s = shop(pool).await;
    let sale = s.sell("ref-key-00001", json!([{ "variant_id": s.rice, "quantity": 1 }]), json!([{ "method": "cash", "amount_minor": 575 }, { "method": "card", "amount_minor": 500, "reference": "R1", "status": "pending" }])).await;
    assert_eq!(sale.status, StatusCode::CREATED, "{}", sale.body);
    let sid = sale.body["id"].as_str().unwrap().to_string();
    let url = format!("/api/v1/sales/{sid}/payments");
    let same = json!({ "method": "card", "amount_minor": 500, "reference": "R1" });
    let results = futures::future::join_all((0..10).map(|_| s.api.post(&url, &s.cashier, same.clone()))).await;
    assert!(results.iter().all(|r| r.status == StatusCode::OK && r.body["duplicate"] == true), "ten replays, no new payment");
    assert_eq!(s.api.post(&url, &s.cashier, json!({ "method": "card", "amount_minor": 400, "reference": "R1" })).await.status, StatusCode::CONFLICT);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sale_payments").fetch_one(&s.api.pool).await.unwrap();
    assert_eq!(n, 2);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr16_reconciliation_ties_payments_back_to_sales(pool: PgPool) {
    let s = shop(pool).await;
    let report = || async { s.api.get(&format!("/api/v1/stores/{}/reports/payment-reconciliation", s.lagos), &s.mgr).await.body };
    let a = s.sell("recon-key-001", json!([{ "variant_id": s.rice, "quantity": 2 }]), cash(3000)).await; // total 2150, change 850
    let b = s.sell("recon-key-002", json!([{ "variant_id": s.rice, "quantity": 1 }]), json!([{ "method": "card", "amount_minor": 1075, "status": "pending" }])).await;

    let r = report().await;
    assert_eq!((r["sales_count"].as_i64(), r["sales_total_minor"].as_i64(), r["change_given_minor"].as_i64()), (Some(2), Some(3225), Some(850)));
    assert_eq!((r["collected_minor"].as_i64(), r["pending_minor"].as_i64(), r["unpaid_minor"].as_i64()), (Some(2150), Some(1075), Some(0)));
    assert_eq!(r["reconciled"], false, "a payment is still pending");
    assert_eq!(r["by_method"]["cash"]["successful"], 3000);

    let (bid, bpid) = (b.body["id"].as_str().unwrap().to_string(), b.body["payments"][0]["id"].as_str().unwrap().to_string());
    s.api.post(&format!("/api/v1/sales/{bid}/payments/{bpid}/confirm"), &s.cashier, json!({})).await;
    let r = report().await;
    assert_eq!((r["collected_minor"].as_i64(), r["pending_minor"].as_i64(), r["reconciled"].as_bool()), (Some(3225), Some(0), Some(true)));
    assert_eq!(r["collected_minor"], r["sales_total_minor"], "takings equal sales");

    // A refund shows up separately; a failed payment leaves an unpaid amount.
    let line = a.body["lines"][0]["id"].as_str().unwrap();
    s.api.post_idem(&format!("/api/v1/sales/{}/returns", a.body["id"].as_str().unwrap()), &s.cashier, "recon-ret-001", json!({ "reason": "x", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] })).await;
    let c = s.sell("recon-key-003", json!([{ "variant_id": s.rice, "quantity": 1 }]), json!([{ "method": "card", "amount_minor": 1075, "status": "pending" }])).await;
    s.api.post(&format!("/api/v1/sales/{}/payments/{}/fail", c.body["id"].as_str().unwrap(), c.body["payments"][0]["id"].as_str().unwrap()), &s.cashier, json!({})).await;
    let r = report().await;
    assert_eq!((r["refunds_issued_minor"].as_i64(), r["net_collected_minor"].as_i64()), (Some(1075), Some(3225 - 1075)));
    assert_eq!((r["unpaid_minor"].as_i64(), r["reconciled"].as_bool()), (Some(1075), Some(false)));
    assert_eq!(s.api.get(&format!("/api/v1/stores/{}/reports/payment-reconciliation", s.lagos), &s.cashier).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr17_sales_can_be_searched_and_reprints_are_marked(pool: PgPool) {
    let s = shop(pool).await;
    let (cashier2_id, cashier2) = s.api.employee(&s.owner, "cash2@amiable.test", "cashier", Some(s.lagos)).await;
    let one = s.sell("find-key-0001", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await;
    s.sell_as(&cashier2, "find-key-0002", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await;
    s.sell("find-key-0003", json!([{ "variant_id": s.rice, "quantity": 1 }]), json!([{ "method": "card", "amount_minor": 1075, "status": "pending" }])).await;
    async fn list(s: &Shop, q: String) -> usize {
        s.api.get(&format!("{}{q}", s.sales()), &s.mgr).await.body["items"].as_array().unwrap().len()
    }

    assert_eq!(list(&s, String::new()).await, 3);
    assert_eq!(list(&s, format!("?transaction={}", one.body["receipt_number"].as_str().unwrap())).await, 1);
    assert_eq!(list(&s, format!("?transaction={}", one.body["transaction_number"].as_str().unwrap())).await, 1);
    assert_eq!(list(&s, format!("?transaction={}", one.body["id"].as_str().unwrap())).await, 1);
    assert_eq!(list(&s, format!("?cashier_id={cashier2_id}")).await, 1);
    assert_eq!(list(&s, "?payment_status=pending".into()).await, 1);
    assert_eq!(list(&s, "?payment_status=paid".into()).await, 2);
    let today = chrono::Utc::now().date_naive();
    assert_eq!(list(&s, format!("?from={}&to={}", today - chrono::Duration::days(1), today + chrono::Duration::days(1))).await, 3);
    assert_eq!(list(&s, format!("?from={}", today + chrono::Duration::days(2))).await, 0);
    assert_eq!(list(&s, format!("?to={}", today - chrono::Duration::days(2))).await, 0);
    assert_eq!(s.api.get(&format!("{}?payment_status=bogus", s.sales()), &s.mgr).await.status, StatusCode::BAD_REQUEST);

    // The first print is the original; a later one is a reprint, limited to managers and finance staff.
    let id = one.body["id"].as_str().unwrap();
    let print = format!("/api/v1/sales/{id}/receipt/print");
    let first = s.api.post(&print, &s.cashier, json!({})).await;
    assert_eq!((first.body["reprint"].as_bool(), first.body["copy_label"].as_str()), (Some(false), Some("ORIGINAL")));
    assert_eq!(s.api.post(&print, &s.cashier, json!({})).await.status, StatusCode::FORBIDDEN, "a cashier cannot reprint");
    let again = s.api.post(&print, &s.mgr, json!({})).await;
    assert_eq!((again.body["reprint"].as_bool(), again.body["copy_label"].as_str(), again.body["print_count"].as_i64()), (Some(true), Some("REPRINT #1"), Some(2)), "the second print overall is the first reprint");
    assert_eq!(s.api.get(&format!("/api/v1/sales/{id}/receipt"), &s.cashier).await.body["print_count"], 2);
    assert_eq!(s.api.audit_count("reprint_receipt", "success").await, 1);
    assert_eq!(s.api.audit_count("reprint_receipt", "denied").await, 1);
    // The reprint carries the original values.
    assert_eq!(again.body["total_minor"], one.body["total_minor"]);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr18_refunds_carry_a_status_and_their_approver(pool: PgPool) {
    let s = shop(pool).await;
    let gift = s.api.post("/api/v1/products", &s.owner, json!({ "name": "Gift card", "returnable": false, "variants": [{ "sku": "GIFT", "price_minor": 500 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    s.api.post(&s.stock("/receipts"), &s.officer, json!({ "lines": [{ "variant_id": gift, "quantity": 5 }] })).await;
    let sale = s.sell("appr-key-0001", json!([{ "variant_id": gift, "quantity": 1 }, { "variant_id": s.rice, "quantity": 1 }]), cash(5000)).await;
    let sid = sale.body["id"].as_str().unwrap().to_string();
    let line_of = |sku: &str| sale.body["lines"].as_array().unwrap().iter().find(|l| l["sku"] == sku).unwrap()["id"].as_str().unwrap().to_string();
    let url = format!("/api/v1/sales/{sid}/returns");
    let body = |line: &str| json!({ "reason": "customer request", "refund_method": "other", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] });

    assert_eq!(s.api.post_idem(&url, &s.cashier, "appr-key-0002", body(&line_of("GIFT"))).await.status, StatusCode::FORBIDDEN, "non-returnable goods need a manager");
    let approved = s.api.post_idem(&url, &s.mgr, "appr-key-0003", body(&line_of("GIFT"))).await;
    assert_eq!(approved.status, StatusCode::CREATED, "{}", approved.body);
    assert_eq!((approved.body["refund_status"].as_str(), approved.body["approved_by"].as_str().map(str::to_string)), (Some("refunded"), Some(s.mgr_id.to_string())));
    let ordinary = s.api.post_idem(&url, &s.cashier, "appr-key-0004", body(&line_of("RICE"))).await;
    assert!(ordinary.body["approved_by"].is_null(), "no approval was needed");

    // Everything on the sale is back, so the sale reads as refunded and its payments are marked refunded.
    let shown = s.api.get(&format!("/api/v1/sales/{sid}"), &s.mgr).await;
    assert_eq!(shown.body["payment_status"], "refunded");
    assert_eq!(shown.body["payments"][0]["status"], "refunded");
    assert_eq!(shown.body["returns"][0]["refund_status"], "refunded");
}
