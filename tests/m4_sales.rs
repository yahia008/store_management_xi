//! FR-15 sales, FR-16 payments, FR-17 receipts, FR-18 returns and refunds, FR-19 customers.
mod common;
use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use store_api::Config;
use uuid::Uuid;

struct Shop {
    api: Api,
    owner: String,
    lagos: Uuid,
    abuja: Uuid,
    mgr: String,
    cashier: String,
    rice: Uuid,
    book: Uuid,
}

async fn shop_with(pool: PgPool, config: Config) -> Shop {
    let api = Api::with_config(pool, config);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let (_, mgr) = api.employee(&owner, "mgr@amiable.test", "store_manager", Some(lagos)).await;
    let (_, cashier) = api.employee(&owner, "cash@amiable.test", "cashier", Some(lagos)).await;
    let (_, officer) = api.employee(&owner, "inv@amiable.test", "inventory_officer", Some(lagos)).await;
    assert_eq!(api.post("/api/v1/business/tax-rates", &owner, json!({ "name": "VAT", "rate_bps": 750 })).await.status, StatusCode::CREATED);
    let product = |sku: &str, price: i64, exempt: bool| json!({ "name": sku, "tax_exempt": exempt, "variants": [{ "sku": sku, "price_minor": price }] });
    let rice = api.post("/api/v1/products", &owner, product("RICE", 1000, false)).await.body["variants"][0]["id"].as_str().unwrap().parse().unwrap();
    let book = api.post("/api/v1/products", &owner, product("BOOK", 500, true)).await.body["variants"][0]["id"].as_str().unwrap().parse().unwrap();
    for v in [rice, book] {
        let r = api.post(&format!("/api/v1/stores/{lagos}/stock/receipts"), &officer, json!({ "lines": [{ "variant_id": v, "quantity": 100 }] })).await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    Shop { api, owner, lagos, abuja, mgr, cashier, rice, book }
}

async fn shop(pool: PgPool) -> Shop {
    shop_with(pool, Config::default()).await
}

impl Shop {
    fn sales(&self) -> String {
        format!("/api/v1/stores/{}/sales", self.lagos)
    }
    async fn sell(&self, key: &str, lines: Value, payments: Value) -> Resp {
        self.api.post_idem(&self.sales(), &self.cashier, key, json!({ "lines": lines, "payments": payments })).await
    }
    async fn stock(&self, v: Uuid) -> Value {
        self.api.get(&format!("/api/v1/stores/{}/stock/{v}", self.lagos), &self.owner).await.body
    }
    async fn on_hand(&self, v: Uuid) -> i64 {
        self.stock(v).await["on_hand"].as_i64().unwrap()
    }
}

fn cash(n: i64) -> Value {
    json!([{ "method": "cash", "amount_minor": n }])
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_a_sale_prices_taxes_numbers_and_takes_stock(pool: PgPool) {
    let s = shop(pool).await;
    // 3 rice at 10.00 (taxed 7.5%) + 1 book at 5.00 (exempt)
    let r = s.sell("sale-key-0001", json!([{ "variant_id": s.rice, "quantity": 3 }, { "variant_id": s.book, "quantity": 1 }]), cash(4000)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["subtotal_minor"], 3500);
    assert_eq!(r.body["tax_minor"], 225);
    assert_eq!(r.body["total_minor"], 3725);
    assert_eq!(r.body["change_minor"], 275);
    assert_eq!(r.body["receipt_number"], "RCT-000001");
    assert_eq!(r.body["currency"], "NGN");
    let line = |sku: &str| r.body["lines"].as_array().unwrap().iter().find(|l| l["sku"] == sku).unwrap().clone();
    assert_eq!((line("RICE")["line_tax_minor"].as_i64(), line("RICE")["tax_rate_bps"].as_i64()), (Some(225), Some(750)));
    assert_eq!((line("BOOK")["line_tax_minor"].as_i64(), line("BOOK")["tax_rate_bps"].as_i64()), (Some(0), Some(0)));
    assert_eq!(s.on_hand(s.rice).await, 97);
    assert_eq!(s.on_hand(s.book).await, 99);

    let second = s.sell("sale-key-0002", json!([{ "variant_id": s.book, "quantity": 1 }]), cash(500)).await;
    assert_eq!(second.body["receipt_number"], "RCT-000002");
    assert_eq!(second.body["transaction_number"], "TXN-000002");

    // Every stock change a sale made is in the ledger and points back at the sale.
    let m = s.api.get(&format!("/api/v1/stores/{}/stock/movements?variant_id={}", s.lagos, s.rice), &s.mgr).await;
    assert_eq!(m.body["items"][0]["kind"], "sale");
    assert_eq!(m.body["items"][0]["reference_id"], r.body["id"]);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_tax_is_rounded_per_line_and_lines_add_up_to_the_total(pool: PgPool) {
    let s = shop(pool).await;
    let odd = s.api.post("/api/v1/products", &s.owner, json!({ "name": "Odd", "variants": [{ "sku": "ODD", "price_minor": 1999 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let r = s.api.post(&format!("/api/v1/stores/{}/stock/receipts", s.lagos), &s.api.token("inv@amiable.test").await, json!({ "lines": [{ "variant_id": odd, "quantity": 10 }] })).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = s.sell("sale-key-0003", json!([{ "variant_id": odd, "quantity": 1 }, { "variant_id": odd, "quantity": 3 }]), cash(100_000)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let taxes: Vec<i64> = r.body["lines"].as_array().unwrap().iter().map(|l| l["line_tax_minor"].as_i64().unwrap()).collect();
    assert!(taxes.contains(&150) && taxes.contains(&450), "1999 -> 149.925 -> 150 and 5997 -> 449.775 -> 450, got {taxes:?}");
    let line_totals: i64 = r.body["lines"].as_array().unwrap().iter().map(|l| l["line_total_minor"].as_i64().unwrap()).sum();
    assert_eq!(line_totals, r.body["total_minor"].as_i64().unwrap());
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_the_server_sets_the_price_not_the_till(pool: PgPool) {
    let s = shop(pool).await;
    let r = s.sell("sale-key-0004", json!([{ "variant_id": s.rice, "quantity": 1, "unit_price_minor": 1 }]), cash(2000)).await;
    assert_eq!(r.body["lines"][0]["unit_price_minor"], 1000, "a price sent by the client is ignored");
    let missing = s.sell("sale-key-0005", json!([{ "variant_id": Uuid::new_v4(), "quantity": 1 }]), cash(2000)).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    for bad in [json!([{ "variant_id": s.rice, "quantity": 0 }]), json!([{ "variant_id": s.rice, "quantity": -2 }]), json!([])] {
        assert_eq!(s.sell("sale-key-0006", bad, cash(2000)).await.status, StatusCode::BAD_REQUEST);
    }
    s.api.patch(&format!("/api/v1/products/{}", s.api.get("/api/v1/products?q=BOOK", &s.owner).await.body["items"][0]["id"].as_str().unwrap()), &s.owner, json!({ "status": "archived" })).await;
    assert_eq!(s.sell("sale-key-0007", json!([{ "variant_id": s.book, "quantity": 1 }]), cash(500)).await.status, StatusCode::CONFLICT, "archived products are not for sale");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_a_store_price_override_applies_only_in_that_store(pool: PgPool) {
    let s = shop(pool).await;
    let lagos_price = s.api.post(&format!("/api/v1/variants/{}/prices", s.rice), &s.mgr, json!({ "price_minor": 1200, "store_id": s.lagos })).await;
    assert_eq!(lagos_price.status, StatusCode::CREATED);
    let r = s.sell("sale-key-0008", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await;
    assert_eq!(r.body["lines"][0]["unit_price_minor"], 1200);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_old_sales_keep_the_price_and_tax_they_were_made_with(pool: PgPool) {
    let s = shop(pool).await;
    let old = s.sell("sale-key-0009", json!([{ "variant_id": s.rice, "quantity": 2 }]), cash(5000)).await;
    assert_eq!((old.body["total_minor"].as_i64(), old.body["tax_minor"].as_i64()), (Some(2150), Some(150)));

    s.api.post(&format!("/api/v1/variants/{}/prices", s.rice), &s.owner, json!({ "price_minor": 2000 })).await;
    s.api.post("/api/v1/business/tax-rates", &s.owner, json!({ "name": "VAT", "rate_bps": 1000 })).await;

    let id = old.body["id"].as_str().unwrap();
    let again = s.api.get(&format!("/api/v1/sales/{id}"), &s.mgr).await;
    assert_eq!(again.body["total_minor"], 2150, "history is not rewritten");
    assert_eq!(again.body["lines"][0]["unit_price_minor"], 1000);
    assert_eq!(again.body["lines"][0]["tax_rate_bps"], 750);

    let new = s.sell("sale-key-0010", json!([{ "variant_id": s.rice, "quantity": 2 }]), cash(9000)).await;
    assert_eq!((new.body["subtotal_minor"].as_i64(), new.body["tax_minor"].as_i64()), (Some(4000), Some(400)));
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_not_enough_stock_stops_the_whole_sale_and_gives_the_number_back(pool: PgPool) {
    let s = shop(pool).await;
    let r = s.sell("sale-key-0011", json!([{ "variant_id": s.rice, "quantity": 5 }, { "variant_id": s.book, "quantity": 101 }]), cash(1_000_000)).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.body["error"]["message"].as_str().unwrap().contains("BOOK"));
    assert_eq!(s.on_hand(s.rice).await, 100, "the first line was not taken either");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sales").fetch_one(&s.api.pool).await.unwrap();
    assert_eq!(n, 0);
    let ok = s.sell("sale-key-0012", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await;
    assert_eq!(ok.body["receipt_number"], "RCT-000001", "no gap in the numbering");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr16_payments_must_cover_the_total_and_only_cash_gives_change(pool: PgPool) {
    let s = shop(pool).await;
    let line = json!([{ "variant_id": s.rice, "quantity": 1 }]); // total 1075
    assert_eq!(s.sell("pay-key-00001", line.clone(), cash(1000)).await.status, StatusCode::BAD_REQUEST, "short");
    assert_eq!(s.sell("pay-key-00002", line.clone(), json!([{ "method": "card", "amount_minor": 2000 }])).await.status, StatusCode::BAD_REQUEST, "no change from a card");
    assert_eq!(s.sell("pay-key-00003", line.clone(), json!([{ "method": "bitcoin", "amount_minor": 1075 }])).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.sell("pay-key-00004", line.clone(), json!([{ "method": "cash", "amount_minor": 0 }])).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.on_hand(s.rice).await, 100, "failed attempts take no stock");

    let split = s.sell("pay-key-00005", line.clone(), json!([{ "method": "card", "amount_minor": 600, "reference": "POS-77" }, { "method": "cash", "amount_minor": 500 }])).await;
    assert_eq!(split.status, StatusCode::CREATED, "{}", split.body);
    assert_eq!((split.body["paid_minor"].as_i64(), split.body["change_minor"].as_i64()), (Some(1100), Some(25)));
    assert_eq!(split.body["payments"].as_array().unwrap().len(), 2);
    let exact = s.sell("pay-key-00006", line.clone(), json!([{ "method": "transfer", "amount_minor": 1075, "reference": "NIP123" }])).await;
    assert_eq!(exact.body["change_minor"], 0);
    // Cash that is not needed for the change is fine, but card plus too much cash for change is not.
    let odd = s.sell("pay-key-00007", line, json!([{ "method": "card", "amount_minor": 1075 }, { "method": "cash", "amount_minor": 100 }])).await;
    assert_eq!(odd.status, StatusCode::BAD_REQUEST, "paying the whole total by card and then more in cash is refused");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_a_retried_sale_is_not_sold_twice(pool: PgPool) {
    let s = shop(pool).await;
    let body = json!({ "lines": [{ "variant_id": s.rice, "quantity": 4 }], "payments": cash(5000) });
    let first = s.api.post_idem(&s.sales(), &s.cashier, "retry-key-001", body.clone()).await;
    assert_eq!(first.status, StatusCode::CREATED);
    let again = s.api.post_idem(&s.sales(), &s.cashier, "retry-key-001", body.clone()).await;
    assert_eq!(again.status, StatusCode::OK, "the original is returned");
    assert_eq!(again.body["id"], first.body["id"]);
    assert_eq!(s.on_hand(s.rice).await, 96);

    let different = s.api.post_idem(&s.sales(), &s.cashier, "retry-key-001", json!({ "lines": [{ "variant_id": s.rice, "quantity": 1 }], "payments": cash(5000) })).await;
    assert_eq!(different.status, StatusCode::CONFLICT, "same key, different sale");
    let no_key = s.api.post(&s.sales(), &s.cashier, body.clone()).await;
    assert_eq!(no_key.status, StatusCode::BAD_REQUEST);

    // Five simultaneous copies of one request still make exactly one sale.
    let body2 = json!({ "lines": [{ "variant_id": s.rice, "quantity": 2 }], "payments": cash(5000) });
    let sales_url = s.sales();
    let results = futures::future::join_all((0..5).map(|_| s.api.post_idem(&sales_url, &s.cashier, "burst-key-001", body2.clone()))).await;
    let ids: std::collections::HashSet<String> = results.iter().map(|r| r.body["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(results.iter().filter(|r| r.status == StatusCode::CREATED).count(), 1);
    assert_eq!(s.on_hand(s.rice).await, 94);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sales").fetch_one(&s.api.pool).await.unwrap();
    assert_eq!(n, 2);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_two_tills_cannot_sell_the_last_unit_twice(pool: PgPool) {
    let s = shop(pool).await;
    let (_, c2) = s.api.employee(&s.owner, "cash2@amiable.test", "cashier", Some(s.lagos)).await;
    let inv = s.api.token("inv@amiable.test").await;
    let one = s.api.post("/api/v1/products", &s.owner, json!({ "name": "Rare", "variants": [{ "sku": "RARE", "price_minor": 100 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    s.api.post(&format!("/api/v1/stores/{}/stock/receipts", s.lagos), &inv, json!({ "lines": [{ "variant_id": one, "quantity": 1 }] })).await;
    let body = json!({ "lines": [{ "variant_id": one, "quantity": 1 }], "payments": cash(1000) });
    let (a, b) = futures::future::join(s.api.post_idem(&s.sales(), &s.cashier, "last-unit-a1", body.clone()), s.api.post_idem(&s.sales(), &c2, "last-unit-b1", body.clone())).await;
    let mut codes = [a.status, b.status];
    codes.sort();
    assert_eq!(codes, [StatusCode::CREATED, StatusCode::CONFLICT]);
    assert_eq!(s.on_hand(one).await, 0);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr17_the_receipt_has_everything_needed_to_print(pool: PgPool) {
    let s = shop(pool).await;
    s.api.put("/api/v1/business/settings", &s.owner, json!({ "receipt_header": "Welcome", "receipt_footer": "No refunds without receipt" })).await;
    s.api.patch("/api/v1/business", &s.owner, json!({ "trading_name": "Amiable Stores", "address": "1 Marina, Lagos" })).await;
    s.api.put(&format!("/api/v1/stores/{}/settings", s.lagos), &s.mgr, json!({ "receipt_prefix": "LAG" })).await;
    let sale = s.sell("rcpt-key-0001", json!([{ "variant_id": s.rice, "quantity": 2 }]), json!([{ "method": "card", "amount_minor": 2150, "reference": "POS-1" }])).await;
    assert_eq!(sale.body["receipt_number"], "LAG-000001");

    let id = sale.body["id"].as_str().unwrap();
    let r = s.api.get(&format!("/api/v1/sales/{id}/receipt"), &s.cashier).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["business"]["name"], "Amiable Stores");
    assert_eq!(r.body["store"]["name"], "Lagos");
    assert_eq!((r.body["header"].as_str(), r.body["footer"].as_str()), (Some("Welcome"), Some("No refunds without receipt")));
    assert_eq!(r.body["timezone"], "Africa/Lagos");
    assert_eq!(r.body["local_time"].as_str().unwrap().len(), 19);
    assert_eq!(r.body["lines"][0]["product_name"], "RICE");
    assert_eq!(r.body["payments"][0]["reference"], "POS-1");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr15_sales_are_visible_only_to_the_people_who_should_see_them(pool: PgPool) {
    let s = shop(pool).await;
    let (_, other_cashier) = s.api.employee(&s.owner, "cash2@amiable.test", "cashier", Some(s.lagos)).await;
    let (_, abuja_cashier) = s.api.employee(&s.owner, "ac@amiable.test", "cashier", Some(s.abuja)).await;
    let sale = s.sell("vis-key-00001", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await;
    let id = sale.body["id"].as_str().unwrap();

    assert_eq!(s.api.get(&format!("/api/v1/sales/{id}"), &s.cashier).await.status, StatusCode::OK);
    assert_eq!(s.api.get(&format!("/api/v1/sales/{id}"), &s.mgr).await.status, StatusCode::OK);
    assert_eq!(s.api.get(&format!("/api/v1/sales/{id}"), &other_cashier).await.status, StatusCode::FORBIDDEN, "a colleague's sale");
    assert_eq!(s.api.get(&format!("/api/v1/sales/{id}/receipt"), &abuja_cashier).await.status, StatusCode::FORBIDDEN);
    // A cashier cannot sell in another store by swapping the store id.
    let other = s.api.post_idem(&format!("/api/v1/stores/{}/sales", s.abuja), &s.cashier, "vis-key-00002", json!({ "lines": [{ "variant_id": s.rice, "quantity": 1 }], "payments": cash(2000) })).await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
    assert_eq!(s.api.get(&format!("/api/v1/stores/{}/sales", s.lagos), &other_cashier).await.body["items"].as_array().unwrap().len(), 0);
    assert_eq!(s.api.get(&format!("/api/v1/stores/{}/sales", s.lagos), &s.mgr).await.body["items"].as_array().unwrap().len(), 1);

    // Closed stores do not sell.
    s.api.patch(&format!("/api/v1/stores/{}", s.lagos), &s.owner, json!({ "status": "inactive" })).await;
    assert_eq!(s.sell("vis-key-00003", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await.status, StatusCode::CONFLICT);
    // And the record cannot be edited or deleted afterwards.
    assert!(sqlx::query("UPDATE sales SET total_minor = 1").execute(&s.api.pool).await.is_err());
    assert!(sqlx::query("DELETE FROM sales").execute(&s.api.pool).await.is_err());
    assert!(sqlx::query("DELETE FROM sale_lines").execute(&s.api.pool).await.is_err());
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr18_returns_refund_exactly_and_put_stock_back(pool: PgPool) {
    let s = shop(pool).await;
    let sale = s.sell("ret-sale-0001", json!([{ "variant_id": s.rice, "quantity": 3 }]), cash(4000)).await; // line total 3225
    let (sid, line) = (sale.body["id"].as_str().unwrap().to_string(), sale.body["lines"][0]["id"].as_str().unwrap().to_string());
    assert_eq!(s.on_hand(s.rice).await, 97);
    let url = format!("/api/v1/sales/{sid}/returns");

    let one = s.api.post_idem(&url, &s.cashier, "ret-key-00001", json!({ "reason": "wrong item", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] })).await;
    assert_eq!(one.status, StatusCode::CREATED, "{}", one.body);
    assert_eq!(one.body["refund_minor"], 1075);
    assert_eq!(one.body["return_number"], "RTN-000001");
    assert_eq!(s.on_hand(s.rice).await, 98);

    let rest = s.api.post_idem(&url, &s.cashier, "ret-key-00002", json!({ "reason": "broken", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 2, "disposition": "damaged" }] })).await;
    assert_eq!(rest.body["refund_minor"], 2150, "the parts add up to exactly what was paid: 1075 + 2150 = 3225");
    let b = s.stock(s.rice).await;
    assert_eq!((b["on_hand"].as_i64(), b["damaged"].as_i64()), (Some(98), Some(2)), "damaged returns do not become sellable");

    let more = s.api.post_idem(&url, &s.cashier, "ret-key-00003", json!({ "reason": "again", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] })).await;
    assert_eq!(more.status, StatusCode::CONFLICT, "nothing left to return");

    let shown = s.api.get(&format!("/api/v1/sales/{sid}"), &s.mgr).await;
    assert_eq!(shown.body["lines"][0]["returned_qty"], 3);
    assert_eq!(shown.body["returns"].as_array().unwrap().len(), 2);
    assert_eq!(s.api.audit_count("create_return", "success").await, 2);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr18_a_retried_return_refunds_once(pool: PgPool) {
    let s = shop(pool).await;
    let sale = s.sell("ret-sale-0002", json!([{ "variant_id": s.rice, "quantity": 2 }]), cash(3000)).await;
    let (sid, line) = (sale.body["id"].as_str().unwrap().to_string(), sale.body["lines"][0]["id"].as_str().unwrap().to_string());
    let url = format!("/api/v1/sales/{sid}/returns");
    let body = json!({ "reason": "changed mind", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] });
    let results = futures::future::join_all((0..4).map(|_| s.api.post_idem(&url, &s.cashier, "ret-retry-001", body.clone()))).await;
    assert_eq!(results.iter().filter(|r| r.status == StatusCode::CREATED).count(), 1);
    assert!(results.iter().all(|r| r.status == StatusCode::CREATED || r.status == StatusCode::OK));
    assert_eq!(s.on_hand(s.rice).await, 99);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM returns").fetch_one(&s.api.pool).await.unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr18_late_returns_and_big_refunds_need_a_manager(pool: PgPool) {
    let cfg = Config { return_manager_limit_minor: 2000, ..Config::default() };
    let s = shop_with(pool, cfg).await;
    let big = s.sell("ret-sale-0003", json!([{ "variant_id": s.rice, "quantity": 3 }]), cash(4000)).await; // refunds 3225 in full
    let (sid, line) = (big.body["id"].as_str().unwrap().to_string(), big.body["lines"][0]["id"].as_str().unwrap().to_string());
    let url = format!("/api/v1/sales/{sid}/returns");
    let all = |qty: i64| json!({ "reason": "return", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": qty, "disposition": "restock" }] });

    assert_eq!(s.api.post_idem(&url, &s.cashier, "mgr-key-00001", all(3)).await.status, StatusCode::FORBIDDEN, "refund above the cashier limit");
    assert_eq!(s.on_hand(s.rice).await, 97, "a refused return changes nothing");
    assert_eq!(s.api.post_idem(&url, &s.cashier, "mgr-key-00002", all(1)).await.status, StatusCode::CREATED, "1075 is within the limit");
    assert_eq!(s.api.post_idem(&url, &s.mgr, "mgr-key-00003", all(2)).await.status, StatusCode::CREATED, "a manager can approve the rest");
    assert!(s.api.audit_count("return_needs_manager", "denied").await >= 1);

    // A sale older than the return window also needs a manager.
    let old = s.sell("ret-sale-0004", json!([{ "variant_id": s.book, "quantity": 1 }]), cash(500)).await;
    let (osid, oline) = (old.body["id"].as_str().unwrap().to_string(), old.body["lines"][0]["id"].as_str().unwrap().to_string());
    sqlx::query("ALTER TABLE sales DISABLE TRIGGER sales_no_update").execute(&s.api.pool).await.unwrap();
    sqlx::query("UPDATE sales SET created_at = now() - interval '45 days' WHERE id = $1").bind(osid.parse::<Uuid>().unwrap()).execute(&s.api.pool).await.unwrap();
    sqlx::query("ALTER TABLE sales ENABLE TRIGGER sales_no_update").execute(&s.api.pool).await.unwrap();
    let body = json!({ "reason": "late", "refund_method": "cash", "lines": [{ "sale_line_id": oline, "quantity": 1, "disposition": "restock" }] });
    let ourl = format!("/api/v1/sales/{osid}/returns");
    assert_eq!(s.api.post_idem(&ourl, &s.cashier, "mgr-key-00004", body.clone()).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.api.post_idem(&ourl, &s.mgr, "mgr-key-00005", body).await.status, StatusCode::CREATED);

    // Input checks.
    let bad = s.api.post_idem(&url, &s.mgr, "mgr-key-00006", json!({ "reason": "", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 1, "disposition": "restock" }] })).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let foreign = s.api.post_idem(&url, &s.mgr, "mgr-key-00007", json!({ "reason": "x", "refund_method": "cash", "lines": [{ "sale_line_id": Uuid::new_v4(), "quantity": 1, "disposition": "restock" }] })).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr18_returned_perishables_go_back_to_the_batch_they_came_from(pool: PgPool) {
    let s = shop(pool).await;
    let inv = s.api.token("inv@amiable.test").await;
    let milk = s.api.post("/api/v1/products", &s.owner, json!({ "name": "Milk", "tracks_expiry": true, "variants": [{ "sku": "MILK", "price_minor": 400 }] })).await.body["variants"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let day = |n: i64| (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string();
    for (code, days) in [("SOON", 5), ("LATE", 20)] {
        let r = s.api.post(&format!("/api/v1/stores/{}/stock/receipts", s.lagos), &inv, json!({ "lines": [{ "variant_id": milk, "quantity": 5, "batch_code": code, "expiry_date": day(days) }] })).await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    let sale = s.sell("ret-sale-0005", json!([{ "variant_id": milk, "quantity": 7 }]), cash(10_000)).await; // 5 SOON + 2 LATE
    assert_eq!(sale.status, StatusCode::CREATED, "{}", sale.body);
    let (sid, line) = (sale.body["id"].as_str().unwrap().to_string(), sale.body["lines"][0]["id"].as_str().unwrap().to_string());
    let batches = |v: Value| -> Vec<(String, i64)> { v["batches"].as_array().unwrap().iter().map(|b| (b["batch_code"].as_str().unwrap().to_string(), b["quantity"].as_i64().unwrap())).collect() };
    assert_eq!(batches(s.stock(milk).await), vec![("LATE".to_string(), 3)]);

    let r = s.api.post_idem(&format!("/api/v1/sales/{sid}/returns"), &s.cashier, "batch-ret-001", json!({ "reason": "spoiled? no, changed mind", "refund_method": "cash", "lines": [{ "sale_line_id": line, "quantity": 6, "disposition": "restock" }] })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(batches(s.stock(milk).await), vec![("SOON".to_string(), 5), ("LATE".to_string(), 4)], "5 back to SOON, 1 to LATE");
    let sum: i64 = sqlx::query_scalar("SELECT sum(quantity)::bigint FROM stock_batches WHERE variant_id = $1").bind(milk).fetch_one(&s.api.pool).await.unwrap();
    assert_eq!(sum, s.on_hand(milk).await, "batches still add up to on_hand");
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr19_customers_are_recorded_found_and_linked_to_sales(pool: PgPool) {
    let s = shop(pool).await;
    let c = s.api.post("/api/v1/customers", &s.cashier, json!({ "name": "Bola Ade", "phone": "0803 123-4567", "email": "Bola@Example.com" })).await;
    assert_eq!(c.status, StatusCode::CREATED, "{}", c.body);
    assert_eq!(c.body["phone"], "08031234567");
    assert_eq!(c.body["email"], "bola@example.com");
    let cid = c.id("id");

    let dup = s.api.post("/api/v1/customers", &s.mgr, json!({ "name": "Someone Else", "phone": "08031234567" })).await;
    assert_eq!(dup.status, StatusCode::CONFLICT, "one customer per phone number");
    assert_eq!(s.api.post("/api/v1/customers", &s.cashier, json!({ "name": "X", "phone": "12" })).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.api.post("/api/v1/customers", &s.cashier, json!({ "name": " " })).await.status, StatusCode::BAD_REQUEST);

    assert_eq!(s.api.get("/api/v1/customers?q=bola", &s.cashier).await.body.as_array().unwrap().len(), 1);
    assert_eq!(s.api.get("/api/v1/customers?q=3123", &s.cashier).await.body.as_array().unwrap().len(), 1);
    assert_eq!(s.api.get("/api/v1/customers?q=nobody", &s.cashier).await.body.as_array().unwrap().len(), 0);

    assert_eq!(s.api.patch(&format!("/api/v1/customers/{cid}"), &s.cashier, json!({ "name": "Changed" })).await.status, StatusCode::FORBIDDEN);
    let upd = s.api.patch(&format!("/api/v1/customers/{cid}"), &s.mgr, json!({ "notes": "prefers card" })).await;
    assert_eq!(upd.body["notes"], "prefers card");

    for (key, qty) in [("cust-sale-001", 1), ("cust-sale-002", 2)] {
        let body = json!({ "lines": [{ "variant_id": s.rice, "quantity": qty }], "payments": cash(5000), "customer_id": cid });
        assert_eq!(s.api.post_idem(&s.sales(), &s.cashier, key, body).await.status, StatusCode::CREATED);
    }
    s.sell("cust-sale-003", json!([{ "variant_id": s.rice, "quantity": 1 }]), cash(2000)).await; // not theirs
    let history = s.api.get(&format!("/api/v1/customers/{cid}/sales"), &s.mgr).await;
    assert_eq!(history.body["sales"].as_array().unwrap().len(), 2);
    assert_eq!(history.body["total_spent_minor"], 1075 + 2150);

    let ghost = s.api.post_idem(&s.sales(), &s.cashier, "cust-sale-004", json!({ "lines": [{ "variant_id": s.rice, "quantity": 1 }], "payments": cash(2000), "customer_id": Uuid::new_v4() })).await;
    assert_eq!(ghost.status, StatusCode::NOT_FOUND);

    // Customer records are for people who serve customers, not for every role.
    let (_, fin) = s.api.employee(&s.owner, "fin@amiable.test", "finance_officer", Some(s.lagos)).await;
    assert_eq!(s.api.get("/api/v1/customers", &fin).await.status, StatusCode::FORBIDDEN);
}
