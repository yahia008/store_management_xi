//! Categories, products, variants and price history (FR-07, FR-08, FR-09).
//!
//! Anyone signed in can read the catalogue (a cashier needs it to sell); only owners and administrators
//! change it. Money is whole minor units. Cost is hidden from roles that have no need to see it.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::permissions::{Principal, Role};
use crate::state::AppState;

fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn conflict_as(e: sqlx::Error, msg: &str) -> ApiError {
    match ApiError::from(e) {
        ApiError::Conflict(_) => ApiError::conflict(msg),
        other => other,
    }
}

impl Principal {
    /// Cost prices are for people who run or stock a store, not for the till.
    fn can_see_cost(&self) -> bool {
        self.is_business_wide() || self.grants.iter().any(|g| matches!(g.role, Role::StoreManager | Role::InventoryOfficer | Role::FinanceOfficer))
    }
}

// ---------------------------------------------------------------- categories

#[derive(Serialize, FromRow)]
pub struct Category {
    id: Uuid,
    parent_id: Option<Uuid>,
    name: String,
    is_active: bool,
}

pub async fn list_categories(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Category>>> {
    Ok(Json(
        sqlx::query_as::<_, Category>("SELECT id, parent_id, name, is_active FROM categories WHERE business_id = $1 ORDER BY lower(name)").bind(p.business_id).fetch_all(&state.pool).await?,
    ))
}

#[derive(Deserialize)]
pub struct CreateCategory {
    name: String,
    parent_id: Option<Uuid>,
}

async fn category_in_business(db: &mut PgConnection, business_id: Uuid, id: Uuid) -> ApiResult<()> {
    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM categories WHERE id = $1 AND business_id = $2)").bind(id).bind(business_id).fetch_one(&mut *db).await?;
    if ok { Ok(()) } else { Err(ApiError::NotFound("category")) }
}

pub async fn create_category(State(state): State<AppState>, p: Principal, Json(b): Json<CreateCategory>) -> ApiResult<(StatusCode, Json<Category>)> {
    p.require_business_admin(&state, "create_category").await?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad("category name is required"));
    }
    let mut tx = state.pool.begin().await?;
    if let Some(parent) = b.parent_id {
        category_in_business(&mut tx, p.business_id, parent).await?;
    }
    let c = sqlx::query_as::<_, Category>("INSERT INTO categories (business_id, parent_id, name) VALUES ($1,$2,$3) RETURNING id, parent_id, name, is_active")
        .bind(p.business_id)
        .bind(b.parent_id)
        .bind(b.name.trim())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| conflict_as(e, "a category with that name already exists here"))?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "create_category", entity_type: "category", entity_id: Some(c.id.to_string()), after: audit::json(&c), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(c)))
}

#[derive(Deserialize)]
pub struct UpdateCategory {
    name: Option<String>,
    parent_id: Option<Uuid>,
    is_active: Option<bool>,
}

pub async fn update_category(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<UpdateCategory>) -> ApiResult<Json<Category>> {
    p.require_business_admin(&state, "update_category").await?;
    if b.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad("category name cannot be empty"));
    }
    let mut tx = state.pool.begin().await?;
    category_in_business(&mut tx, p.business_id, id).await?;
    if let Some(parent) = b.parent_id {
        category_in_business(&mut tx, p.business_id, parent).await?;
        // A category cannot be moved under itself or one of its own descendants.
        let loops: bool = sqlx::query_scalar(
            "WITH RECURSIVE up AS (SELECT id, parent_id FROM categories WHERE id = $1
                                   UNION ALL SELECT c.id, c.parent_id FROM categories c JOIN up ON c.id = up.parent_id)
             SELECT EXISTS (SELECT 1 FROM up WHERE id = $2)",
        )
        .bind(parent)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if loops {
            return Err(ApiError::bad("a category cannot be placed inside itself"));
        }
    }
    let before = sqlx::query_as::<_, Category>("SELECT id, parent_id, name, is_active FROM categories WHERE id = $1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
    let after = sqlx::query_as::<_, Category>(
        "UPDATE categories SET name = COALESCE($2, name), parent_id = COALESCE($3, parent_id), is_active = COALESCE($4, is_active), updated_at = now()
         WHERE id = $1 RETURNING id, parent_id, name, is_active",
    )
    .bind(id)
    .bind(clean(b.name))
    .bind(b.parent_id)
    .bind(b.is_active)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| conflict_as(e, "a category with that name already exists here"))?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "update_category", entity_type: "category", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}

// ---------------------------------------------------------------- products

#[derive(Serialize, FromRow)]
pub struct Product {
    id: Uuid,
    category_id: Option<Uuid>,
    name: String,
    description: Option<String>,
    unit: String,
    tax_exempt: bool,
    tracks_expiry: bool,
    status: String,
    brand: Option<String>,
    image_url: Option<String>,
    attributes: Value,
    returnable: bool,
}

#[derive(Serialize, FromRow)]
pub struct Variant {
    id: Uuid,
    product_id: Uuid,
    sku: String,
    barcode: Option<String>,
    name: Option<String>,
    attributes: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_minor: Option<i64>,
    status: String,
    /// The price a sale would use right now: the store's own price if it has one, else the business price.
    price_minor: Option<i64>,
}

const PRODUCT_COLS: &str = "id, category_id, name, description, unit, tax_exempt, tracks_expiry, status, brand, image_url, attributes, returnable";

/// Variant columns with the effective price resolved for an optional store (`$2`).
const VARIANT_SELECT: &str = "SELECT v.id, v.product_id, v.sku, v.barcode, v.name, v.attributes, v.cost_minor, v.status,
    COALESCE(
      (SELECT p.price_minor FROM variant_prices p WHERE p.variant_id = v.id AND p.store_id = $2 AND p.effective_from <= now()
        ORDER BY p.effective_from DESC, p.created_at DESC LIMIT 1),
      (SELECT p.price_minor FROM variant_prices p WHERE p.variant_id = v.id AND p.store_id IS NULL AND p.effective_from <= now()
        ORDER BY p.effective_from DESC, p.created_at DESC LIMIT 1)) AS price_minor
    FROM product_variants v";

async fn variants_of(db: &mut PgConnection, p: &Principal, product_id: Uuid, store: Option<Uuid>) -> ApiResult<Vec<Variant>> {
    let mut vs = sqlx::query_as::<_, Variant>(&format!("{VARIANT_SELECT} WHERE v.product_id = $1 AND v.business_id = $3 ORDER BY v.created_at, v.sku"))
        .bind(product_id)
        .bind(store)
        .bind(p.business_id)
        .fetch_all(&mut *db)
        .await?;
    if !p.can_see_cost() {
        vs.iter_mut().for_each(|v| v.cost_minor = None);
    }
    Ok(vs)
}

async fn product_json(db: &mut PgConnection, p: &Principal, product: Product, store: Option<Uuid>) -> ApiResult<Value> {
    let variants = variants_of(db, p, product.id, store).await?;
    let mut v = serde_json::to_value(&product).map_err(|e| ApiError::Internal(e.to_string()))?;
    v["variants"] = json!(variants);
    Ok(v)
}

/// Prices for a store view: the store must be one the caller may see.
async fn price_store(state: &AppState, p: &Principal, store: Option<Uuid>) -> ApiResult<Option<Uuid>> {
    if let Some(s) = store {
        p.authorize_store(state, s, &[Role::StoreManager, Role::Cashier, Role::InventoryOfficer, Role::FinanceOfficer], "view_store_prices").await?;
    }
    Ok(store)
}

#[derive(Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    category_id: Option<Uuid>,
    barcode: Option<String>,
    store_id: Option<Uuid>,
    include_archived: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub async fn list_products(State(state): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    let store = price_store(&state, &p, q.store_id).await?;
    let like = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")));
    let status = if q.include_archived == Some(true) && p.is_business_wide() { None } else { Some("active") };
    let filter = "pr.business_id = $1 AND ($2::text IS NULL OR pr.status = $2)
        AND ($3::uuid IS NULL OR pr.category_id = $3)
        AND ($4::text IS NULL OR pr.name ILIKE $4 OR pr.brand ILIKE $4 OR EXISTS (SELECT 1 FROM product_variants v WHERE v.product_id = pr.id AND (v.sku ILIKE $4 OR v.barcode ILIKE $4)))
        AND ($5::text IS NULL OR EXISTS (SELECT 1 FROM product_variants v WHERE v.product_id = pr.id AND v.barcode = $5))";
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM products pr WHERE {filter}"))
        .bind(p.business_id)
        .bind(status)
        .bind(q.category_id)
        .bind(&like)
        .bind(&q.barcode)
        .fetch_one(&state.pool)
        .await?;
    let products = sqlx::query_as::<_, Product>(&format!(
        "SELECT {} FROM products pr WHERE {filter} ORDER BY lower(pr.name), pr.id LIMIT $6 OFFSET $7",
        PRODUCT_COLS.split(", ").map(|c| format!("pr.{c}")).collect::<Vec<_>>().join(", ")
    ))
    .bind(p.business_id)
    .bind(status)
    .bind(q.category_id)
    .bind(&like)
    .bind(&q.barcode)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .bind(q.offset.unwrap_or(0).max(0))
    .fetch_all(&state.pool)
    .await?;
    let mut conn = state.pool.acquire().await?;
    let mut items = Vec::new();
    for pr in products {
        items.push(product_json(&mut conn, &p, pr, store).await?);
    }
    Ok(Json(json!({ "total": total, "items": items })))
}

pub async fn get_product(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    let store = price_store(&state, &p, q.store_id).await?;
    let mut conn = state.pool.acquire().await?;
    let product = sqlx::query_as::<_, Product>(&format!("SELECT {PRODUCT_COLS} FROM products WHERE id = $1 AND business_id = $2"))
        .bind(id)
        .bind(p.business_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(ApiError::NotFound("product"))?;
    Ok(Json(product_json(&mut conn, &p, product, store).await?))
}

#[derive(Deserialize)]
pub struct NewVariant {
    sku: String,
    barcode: Option<String>,
    name: Option<String>,
    attributes: Option<Value>,
    cost_minor: Option<i64>,
    price_minor: i64,
}

#[derive(Deserialize)]
pub struct CreateProduct {
    name: String,
    description: Option<String>,
    category_id: Option<Uuid>,
    unit: Option<String>,
    tax_exempt: Option<bool>,
    tracks_expiry: Option<bool>,
    brand: Option<String>,
    image_url: Option<String>,
    attributes: Option<Value>,
    returnable: Option<bool>,
    variants: Vec<NewVariant>,
}

fn check_variant(v: &NewVariant) -> ApiResult<()> {
    if v.sku.trim().is_empty() {
        return Err(ApiError::bad("every variant needs a SKU"));
    }
    if v.price_minor < 0 || v.cost_minor.is_some_and(|c| c < 0) {
        return Err(ApiError::bad("prices and costs cannot be negative"));
    }
    if v.attributes.as_ref().is_some_and(|a| !a.is_object()) {
        return Err(ApiError::bad("attributes must be an object such as {\"size\": \"L\"}"));
    }
    Ok(())
}

async fn insert_variant(db: &mut PgConnection, p: &Principal, product_id: Uuid, v: &NewVariant) -> ApiResult<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO product_variants (product_id, business_id, sku, barcode, name, attributes, cost_minor) VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(product_id)
    .bind(p.business_id)
    .bind(v.sku.trim())
    .bind(clean(v.barcode.clone()))
    .bind(clean(v.name.clone()))
    .bind(v.attributes.clone().unwrap_or_else(|| json!({})))
    .bind(v.cost_minor)
    .fetch_one(&mut *db)
    .await
    .map_err(|e| conflict_as(e, "that SKU or barcode is already used by another variant"))?;
    sqlx::query("INSERT INTO variant_prices (business_id, variant_id, store_id, price_minor, effective_from, created_by) VALUES ($1,$2,NULL,$3,now(),$4)")
        .bind(p.business_id)
        .bind(id)
        .bind(v.price_minor)
        .bind(p.user_id)
        .execute(&mut *db)
        .await?;
    Ok(id)
}

pub async fn create_product(State(state): State<AppState>, p: Principal, Json(b): Json<CreateProduct>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.require_business_admin(&state, "create_product").await?;
    if b.name.trim().is_empty() {
        return Err(ApiError::bad("product name is required"));
    }
    if b.variants.is_empty() {
        return Err(ApiError::bad("a product needs at least one variant (give a SKU and a price)"));
    }
    b.variants.iter().try_for_each(check_variant)?;
    if b.attributes.as_ref().is_some_and(|a| !a.is_object()) {
        return Err(ApiError::bad("attributes must be an object such as {\"origin\": \"Kebbi\"}"));
    }
    let mut tx = state.pool.begin().await?;
    if let Some(c) = b.category_id {
        category_in_business(&mut tx, p.business_id, c).await?;
    }
    let product = sqlx::query_as::<_, Product>(&format!(
        "INSERT INTO products (business_id, category_id, name, description, unit, tax_exempt, tracks_expiry, created_by, brand, image_url, attributes, returnable)
         VALUES ($1,$2,$3,$4,COALESCE($5,'each'),$6,$7,$8,$9,$10,$11,$12) RETURNING {PRODUCT_COLS}"
    ))
    .bind(p.business_id)
    .bind(b.category_id)
    .bind(b.name.trim())
    .bind(clean(b.description))
    .bind(clean(b.unit))
    .bind(b.tax_exempt.unwrap_or(false))
    .bind(b.tracks_expiry.unwrap_or(false))
    .bind(p.user_id)
    .bind(clean(b.brand))
    .bind(clean(b.image_url))
    .bind(b.attributes.unwrap_or_else(|| json!({})))
    .bind(b.returnable.unwrap_or(true))
    .fetch_one(&mut *tx)
    .await?;
    for v in &b.variants {
        insert_variant(&mut tx, &p, product.id, v).await?;
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "create_product", entity_type: "product", entity_id: Some(product.id.to_string()), after: audit::json(&product), ..Default::default() }).await?;
    let out = product_json(&mut tx, &p, product, None).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Deserialize)]
pub struct UpdateProduct {
    name: Option<String>,
    description: Option<String>,
    category_id: Option<Uuid>,
    unit: Option<String>,
    tax_exempt: Option<bool>,
    tracks_expiry: Option<bool>,
    status: Option<String>,
    brand: Option<String>,
    image_url: Option<String>,
    attributes: Option<Value>,
    returnable: Option<bool>,
}

pub async fn update_product(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<UpdateProduct>) -> ApiResult<Json<Value>> {
    p.require_business_admin(&state, "update_product").await?;
    if b.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ApiError::bad("product name cannot be empty"));
    }
    if b.status.as_deref().is_some_and(|s| s != "active" && s != "archived") {
        return Err(ApiError::bad("status must be active or archived"));
    }
    if b.attributes.as_ref().is_some_and(|a| !a.is_object()) {
        return Err(ApiError::bad("attributes must be an object"));
    }
    let mut tx = state.pool.begin().await?;
    if let Some(c) = b.category_id {
        category_in_business(&mut tx, p.business_id, c).await?;
    }
    let before = sqlx::query_as::<_, Product>(&format!("SELECT {PRODUCT_COLS} FROM products WHERE id = $1 AND business_id = $2 FOR UPDATE"))
        .bind(id)
        .bind(p.business_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound("product"))?;
    let after = sqlx::query_as::<_, Product>(&format!(
        "UPDATE products SET name = COALESCE($2,name), description = COALESCE($3,description), category_id = COALESCE($4,category_id),
             unit = COALESCE($5,unit), tax_exempt = COALESCE($6,tax_exempt), tracks_expiry = COALESCE($7,tracks_expiry),
             status = COALESCE($8,status), brand = COALESCE($9,brand), image_url = COALESCE($10,image_url), attributes = COALESCE($11,attributes),
             returnable = COALESCE($12,returnable), updated_at = now() WHERE id = $1 RETURNING {PRODUCT_COLS}"
    ))
    .bind(id)
    .bind(clean(b.name))
    .bind(clean(b.description))
    .bind(b.category_id)
    .bind(clean(b.unit))
    .bind(b.tax_exempt)
    .bind(b.tracks_expiry)
    .bind(b.status)
    .bind(clean(b.brand))
    .bind(clean(b.image_url))
    .bind(b.attributes)
    .bind(b.returnable)
    .fetch_one(&mut *tx)
    .await?;
    let action = if before.status != after.status { if after.status == "archived" { "archive_product" } else { "restore_product" } } else { "update_product" };
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action, entity_type: "product", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    let out = product_json(&mut tx, &p, after, None).await?;
    tx.commit().await?;
    Ok(Json(out))
}

// ---------------------------------------------------------------- variants

pub async fn add_variant(State(state): State<AppState>, p: Principal, Path(product_id): Path<Uuid>, Json(b): Json<NewVariant>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.require_business_admin(&state, "add_variant").await?;
    check_variant(&b)?;
    let mut tx = state.pool.begin().await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM products WHERE id = $1 AND business_id = $2)").bind(product_id).bind(p.business_id).fetch_one(&mut *tx).await?;
    if !exists {
        return Err(ApiError::NotFound("product"));
    }
    let id = insert_variant(&mut tx, &p, product_id, &b).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "add_variant", entity_type: "variant", entity_id: Some(id.to_string()), after: Some(json!({ "sku": b.sku.trim(), "price_minor": b.price_minor })), ..Default::default() }).await?;
    let v = sqlx::query_as::<_, Variant>(&format!("{VARIANT_SELECT} WHERE v.id = $1")).bind(id).bind(None::<Uuid>).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!(v))))
}

#[derive(Deserialize)]
pub struct UpdateVariant {
    sku: Option<String>,
    barcode: Option<String>,
    name: Option<String>,
    attributes: Option<Value>,
    cost_minor: Option<i64>,
    status: Option<String>,
}

pub async fn update_variant(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<UpdateVariant>) -> ApiResult<Json<Variant>> {
    p.require_business_admin(&state, "update_variant").await?;
    if b.sku.as_deref().is_some_and(|s| s.trim().is_empty()) || b.cost_minor.is_some_and(|c| c < 0) {
        return Err(ApiError::bad("SKU cannot be empty and cost cannot be negative"));
    }
    if b.status.as_deref().is_some_and(|s| s != "active" && s != "archived") {
        return Err(ApiError::bad("status must be active or archived"));
    }
    if b.attributes.as_ref().is_some_and(|a| !a.is_object()) {
        return Err(ApiError::bad("attributes must be an object"));
    }
    let mut tx = state.pool.begin().await?;
    let before = sqlx::query_as::<_, Variant>(&format!("{VARIANT_SELECT} WHERE v.id = $1 AND v.business_id = $3 FOR UPDATE OF v"))
        .bind(id)
        .bind(None::<Uuid>)
        .bind(p.business_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound("variant"))?;
    sqlx::query(
        "UPDATE product_variants SET sku = COALESCE($2,sku), barcode = COALESCE($3,barcode), name = COALESCE($4,name),
             attributes = COALESCE($5,attributes), cost_minor = COALESCE($6,cost_minor), status = COALESCE($7,status), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(clean(b.sku))
    .bind(clean(b.barcode))
    .bind(clean(b.name))
    .bind(b.attributes)
    .bind(b.cost_minor)
    .bind(b.status)
    .execute(&mut *tx)
    .await
    .map_err(|e| conflict_as(e, "that SKU or barcode is already used by another variant"))?;
    let after = sqlx::query_as::<_, Variant>(&format!("{VARIANT_SELECT} WHERE v.id = $1")).bind(id).bind(None::<Uuid>).fetch_one(&mut *tx).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), actor: Some(p.user_id), action: "update_variant", entity_type: "variant", entity_id: Some(id.to_string()), before: audit::json(&before), after: audit::json(&after), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(after))
}

// ---------------------------------------------------------------- price history

#[derive(Serialize, FromRow)]
pub struct PriceRow {
    id: Uuid,
    store_id: Option<Uuid>,
    price_minor: i64,
    previous_price_minor: Option<i64>,
    effective_from: DateTime<Utc>,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
}

async fn variant_exists(db: &mut PgConnection, p: &Principal, id: Uuid) -> ApiResult<()> {
    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM product_variants WHERE id = $1 AND business_id = $2)").bind(id).bind(p.business_id).fetch_one(&mut *db).await?;
    if ok { Ok(()) } else { Err(ApiError::NotFound("variant")) }
}

#[derive(Deserialize)]
pub struct PriceQuery {
    store_id: Option<Uuid>,
}

/// Every price this variant has had, newest first. Only people who manage prices see the full history.
pub async fn price_history(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Query(q): Query<PriceQuery>) -> ApiResult<Json<Value>> {
    let mut conn = state.pool.acquire().await?;
    variant_exists(&mut conn, &p, id).await?;
    match q.store_id {
        Some(s) => p.authorize_store(&state, s, &[Role::StoreManager, Role::FinanceOfficer], "view_price_history").await?,
        None => {
            if !p.can_see_cost() {
                audit::denied(&state.pool, p.business_id, p.user_id, None, "view_price_history").await;
                return Err(ApiError::Forbidden);
            }
        }
    }
    let rows = sqlx::query_as::<_, PriceRow>(
        "SELECT id, store_id, price_minor, previous_price_minor, effective_from, created_by, created_at FROM variant_prices
         WHERE variant_id = $1 AND ($2::uuid IS NULL OR store_id = $2 OR store_id IS NULL) ORDER BY effective_from DESC, created_at DESC",
    )
    .bind(id)
    .bind(q.store_id)
    .fetch_all(&mut *conn)
    .await?;
    let effective = sqlx::query_as::<_, Variant>(&format!("{VARIANT_SELECT} WHERE v.id = $1")).bind(id).bind(q.store_id).fetch_one(&mut *conn).await?;
    Ok(Json(json!({ "current_price_minor": effective.price_minor, "history": rows })))
}

#[derive(Deserialize)]
pub struct SetPrice {
    price_minor: i64,
    store_id: Option<Uuid>,
    effective_from: Option<DateTime<Utc>>,
}

/// Adds a new price. The business-wide price is set by owners and administrators; a store's own price by
/// that store's manager (or an owner/administrator). A price cannot start in the past, so what earlier
/// sales were charged is never contradicted by the history.
pub async fn set_price(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<SetPrice>) -> ApiResult<(StatusCode, Json<PriceRow>)> {
    match b.store_id {
        None => p.require_business_admin(&state, "set_business_price").await?,
        Some(s) => p.authorize_store(&state, s, &[Role::StoreManager], "set_store_price").await?,
    }
    if b.price_minor < 0 {
        return Err(ApiError::bad("a price cannot be negative"));
    }
    let from = b.effective_from.unwrap_or_else(Utc::now);
    if from < Utc::now() - chrono::Duration::seconds(60) {
        return Err(ApiError::bad("a price cannot start in the past"));
    }
    let mut tx = state.pool.begin().await?;
    variant_exists(&mut tx, &p, id).await?;
    // The price this one replaces: the store's own current price, else the business price in force.
    let current = "SELECT price_minor FROM variant_prices WHERE variant_id = $1 AND store_id IS NOT DISTINCT FROM $2 AND effective_from <= now() ORDER BY effective_from DESC, created_at DESC LIMIT 1";
    let mut previous: Option<i64> = sqlx::query_scalar(current).bind(id).bind(b.store_id).fetch_optional(&mut *tx).await?;
    if previous.is_none() && b.store_id.is_some() {
        previous = sqlx::query_scalar(current).bind(id).bind(None::<Uuid>).fetch_optional(&mut *tx).await?;
    }
    let row = sqlx::query_as::<_, PriceRow>(
        "INSERT INTO variant_prices (business_id, variant_id, store_id, price_minor, effective_from, created_by, previous_price_minor) VALUES ($1,$2,$3,$4,$5,$6,$7)
         RETURNING id, store_id, price_minor, previous_price_minor, effective_from, created_by, created_at",
    )
    .bind(p.business_id)
    .bind(id)
    .bind(b.store_id)
    .bind(b.price_minor)
    .bind(from)
    .bind(p.user_id)
    .bind(previous)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: b.store_id, actor: Some(p.user_id), action: "set_price", entity_type: "variant", entity_id: Some(id.to_string()), after: audit::json(&row), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}
