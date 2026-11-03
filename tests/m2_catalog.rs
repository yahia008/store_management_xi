//! FR-07 categories and products, FR-08 variants, FR-09 price history.
mod common;
use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;

fn rice() -> Value {
    json!({ "name": "Rice 50kg", "unit": "bag", "variants": [{ "sku": "RICE-50", "barcode": "6001", "price_minor": 4_500_000, "cost_minor": 3_800_000 }] })
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr07_categories_form_a_tree_without_loops(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let food = api.post("/api/v1/categories", &owner, json!({ "name": "Food" })).await;
    assert_eq!(food.status, StatusCode::CREATED);
    let grains = api.post("/api/v1/categories", &owner, json!({ "name": "Grains", "parent_id": food.id("id") })).await;
    assert_eq!(grains.status, StatusCode::CREATED);

    let dup = api.post("/api/v1/categories", &owner, json!({ "name": "food" })).await;
    assert_eq!(dup.status, StatusCode::CONFLICT);
    // The same name is fine under a different parent.
    assert_eq!(api.post("/api/v1/categories", &owner, json!({ "name": "Food", "parent_id": food.id("id") })).await.status, StatusCode::CREATED);

    let cycle = api.patch(&format!("/api/v1/categories/{}", food.id("id")), &owner, json!({ "parent_id": grains.id("id") })).await;
    assert_eq!(cycle.status, StatusCode::BAD_REQUEST);
    let missing = api.post("/api/v1/categories", &owner, json!({ "name": "X", "parent_id": uuid::Uuid::new_v4() })).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr07_products_are_created_found_and_archived_not_deleted(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let cat = api.post("/api/v1/categories", &owner, json!({ "name": "Grains" })).await.id("id");
    let mut body = rice();
    body["category_id"] = json!(cat);
    let created = api.post("/api/v1/products", &owner, body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["variants"][0]["price_minor"], 4_500_000);
    let id = created.id("id");
    api.post("/api/v1/products", &owner, json!({ "name": "Beans", "variants": [{ "sku": "BEAN-1", "price_minor": 100 }] })).await;

    let by_name = api.get("/api/v1/products?q=rice", &owner).await;
    assert_eq!(by_name.body["total"], 1);
    let by_sku = api.get("/api/v1/products?q=bean-1", &owner).await;
    assert_eq!(by_sku.body["items"][0]["name"], "Beans");
    let by_barcode = api.get("/api/v1/products?barcode=6001", &owner).await;
    assert_eq!(by_barcode.body["items"][0]["id"], json!(id));
    let by_cat = api.get(&format!("/api/v1/products?category_id={cat}"), &owner).await;
    assert_eq!(by_cat.body["total"], 1);
    let page = api.get("/api/v1/products?limit=1&offset=1", &owner).await;
    assert_eq!(page.body["total"], 2);
    assert_eq!(page.body["items"].as_array().unwrap().len(), 1);

    // Archiving hides it from the default list but keeps it.
    let arch = api.patch(&format!("/api/v1/products/{id}"), &owner, json!({ "status": "archived" })).await;
    assert_eq!(arch.body["status"], "archived");
    assert_eq!(api.get("/api/v1/products?q=rice", &owner).await.body["total"], 0);
    assert_eq!(api.get("/api/v1/products?q=rice&include_archived=true", &owner).await.body["total"], 1);
    assert_eq!(api.get(&format!("/api/v1/products/{id}"), &owner).await.status, StatusCode::OK);
    assert_eq!(api.audit_count("archive_product", "success").await, 1);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr07_product_input_is_validated(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let no_variant = api.post("/api/v1/products", &owner, json!({ "name": "Thing", "variants": [] })).await;
    assert_eq!(no_variant.status, StatusCode::BAD_REQUEST);
    let negative = api.post("/api/v1/products", &owner, json!({ "name": "Thing", "variants": [{ "sku": "T", "price_minor": -1 }] })).await;
    assert_eq!(negative.status, StatusCode::BAD_REQUEST);
    let no_name = api.post("/api/v1/products", &owner, json!({ "name": "  ", "variants": [{ "sku": "T", "price_minor": 1 }] })).await;
    assert_eq!(no_name.status, StatusCode::BAD_REQUEST);
    // A failed create leaves nothing behind (the product and its variants go in together).
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM products").fetch_one(&api.pool).await.unwrap();
    assert_eq!(n, 0);
    let half = api.post("/api/v1/products", &owner, json!({ "name": "Thing", "variants": [{ "sku": "A", "price_minor": 1 }, { "sku": "a", "price_minor": 2 }] })).await;
    assert_eq!(half.status, StatusCode::CONFLICT, "SKUs are unique ignoring case");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM products").fetch_one(&api.pool).await.unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr08_variants_have_unique_sku_and_barcode(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let product = api.post("/api/v1/products", &owner, json!({ "name": "T-shirt", "variants": [{ "sku": "TS-S", "name": "Small", "attributes": { "size": "S" }, "price_minor": 500000 }] })).await;
    let pid = product.id("id");
    let add = api.post(&format!("/api/v1/products/{pid}/variants"), &owner, json!({ "sku": "TS-L", "barcode": "777", "attributes": { "size": "L" }, "price_minor": 550000 })).await;
    assert_eq!(add.status, StatusCode::CREATED, "{}", add.body);
    assert_eq!(add.body["attributes"]["size"], "L");

    let dup_sku = api.post(&format!("/api/v1/products/{pid}/variants"), &owner, json!({ "sku": "ts-l", "price_minor": 1 })).await;
    assert_eq!(dup_sku.status, StatusCode::CONFLICT);
    let other = api.post("/api/v1/products", &owner, json!({ "name": "Cap", "variants": [{ "sku": "CAP", "barcode": "777", "price_minor": 1 }] })).await;
    assert_eq!(other.status, StatusCode::CONFLICT, "a barcode identifies one variant in the whole business");

    let vid = add.id("id");
    let upd = api.patch(&format!("/api/v1/variants/{vid}"), &owner, json!({ "status": "archived", "name": "Large" })).await;
    assert_eq!(upd.body["status"], "archived");
    assert_eq!(upd.body["name"], "Large");
    let two = api.get(&format!("/api/v1/products/{pid}"), &owner).await;
    assert_eq!(two.body["variants"].as_array().unwrap().len(), 2);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr09_prices_are_a_history_and_future_prices_wait(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let p = api.post("/api/v1/products", &owner, rice()).await;
    let (pid, vid) = (p.id("id"), p.body["variants"][0]["id"].as_str().unwrap().to_string());

    let later = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &owner, json!({ "price_minor": 5_000_000, "effective_from": later })).await.status, StatusCode::CREATED);
    let now = api.get(&format!("/api/v1/products/{pid}"), &owner).await;
    assert_eq!(now.body["variants"][0]["price_minor"], 4_500_000, "the scheduled price is not live yet");

    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &owner, json!({ "price_minor": 4_700_000 })).await.status, StatusCode::CREATED);
    let now = api.get(&format!("/api/v1/products/{pid}"), &owner).await;
    assert_eq!(now.body["variants"][0]["price_minor"], 4_700_000);

    let hist = api.get(&format!("/api/v1/variants/{vid}/prices"), &owner).await;
    assert_eq!(hist.body["history"].as_array().unwrap().len(), 3, "nothing was overwritten");
    assert_eq!(hist.body["current_price_minor"], 4_700_000);

    let past = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &owner, json!({ "price_minor": 1, "effective_from": past })).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &owner, json!({ "price_minor": -5 })).await.status, StatusCode::BAD_REQUEST);

    // The database itself refuses to rewrite the history.
    assert!(sqlx::query("UPDATE variant_prices SET price_minor = 1").execute(&api.pool).await.is_err());
    assert!(sqlx::query("DELETE FROM variant_prices").execute(&api.pool).await.is_err());
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr09_a_store_can_have_its_own_price(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let abuja = api.store(&owner, "Abuja").await;
    let (_, lagos_mgr) = api.employee(&owner, "m@amiable.test", "store_manager", Some(lagos)).await;
    let (_, abuja_cashier) = api.employee(&owner, "c@amiable.test", "cashier", Some(abuja)).await;
    let p = api.post("/api/v1/products", &owner, rice()).await;
    let (pid, vid) = (p.id("id"), p.body["variants"][0]["id"].as_str().unwrap().to_string());

    // Lagos's manager prices Lagos only.
    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &lagos_mgr, json!({ "price_minor": 4_800_000, "store_id": lagos })).await.status, StatusCode::CREATED);
    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &lagos_mgr, json!({ "price_minor": 1, "store_id": abuja })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.post(&format!("/api/v1/variants/{vid}/prices"), &lagos_mgr, json!({ "price_minor": 1 })).await.status, StatusCode::FORBIDDEN, "business-wide prices are for owners and administrators");

    let at_lagos = api.get(&format!("/api/v1/products/{pid}?store_id={lagos}"), &owner).await;
    assert_eq!(at_lagos.body["variants"][0]["price_minor"], 4_800_000);
    let at_abuja = api.get(&format!("/api/v1/products/{pid}?store_id={abuja}"), &abuja_cashier).await;
    assert_eq!(at_abuja.body["variants"][0]["price_minor"], 4_500_000, "Abuja still uses the business price");
    assert_eq!(api.get(&format!("/api/v1/products/{pid}?store_id={lagos}"), &abuja_cashier).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "store_api::MIGRATOR")]
async fn fr07_staff_can_read_the_catalogue_but_not_change_it_or_see_cost(pool: PgPool) {
    let api = Api::new(pool);
    let owner = api.setup().await;
    let lagos = api.store(&owner, "Lagos").await;
    let (_, cashier) = api.employee(&owner, "c@amiable.test", "cashier", Some(lagos)).await;
    let (_, inv) = api.employee(&owner, "i@amiable.test", "inventory_officer", Some(lagos)).await;
    let p = api.post("/api/v1/products", &owner, rice()).await;
    let (pid, vid) = (p.id("id"), p.body["variants"][0]["id"].as_str().unwrap().to_string());

    let seen = api.get(&format!("/api/v1/products/{pid}"), &cashier).await;
    assert_eq!(seen.status, StatusCode::OK);
    assert!(seen.body["variants"][0].get("cost_minor").is_none(), "the till does not see cost");
    assert_eq!(seen.body["variants"][0]["price_minor"], 4_500_000);
    let seen = api.get(&format!("/api/v1/products/{pid}"), &inv).await;
    assert_eq!(seen.body["variants"][0]["cost_minor"], 3_800_000);

    for who in [&cashier, &inv] {
        assert_eq!(api.post("/api/v1/products", who, rice()).await.status, StatusCode::FORBIDDEN);
        assert_eq!(api.patch(&format!("/api/v1/products/{pid}"), who, json!({ "name": "Free rice" })).await.status, StatusCode::FORBIDDEN);
        assert_eq!(api.post("/api/v1/categories", who, json!({ "name": "X" })).await.status, StatusCode::FORBIDDEN);
        assert_eq!(api.patch(&format!("/api/v1/variants/{vid}"), who, json!({ "sku": "HACK" })).await.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(api.get(&format!("/api/v1/variants/{vid}/prices"), &cashier).await.status, StatusCode::FORBIDDEN);
    assert!(api.audit_count("create_product", "denied").await >= 2);
}
