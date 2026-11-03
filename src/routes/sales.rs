//! Point-of-sale: sales, payments, receipts and returns (FR-15 to FR-18).
//!
//! A sale is one transaction: prices and tax are decided by the server (never the till), stock is taken
//! through the shared inventory service, and receipt numbers come from a per-store counter advanced in the
//! same transaction. Creating a sale or a return needs an `Idempotency-Key` header, so a retried request
//! (flaky network, double tap) returns the original instead of selling twice.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::inventory::{apply, consume, Bucket, Move};
use crate::money::{share, tax_minor};
use crate::permissions::{Principal, Role};
use crate::state::AppState;

const SELL: [Role; 2] = [Role::Cashier, Role::StoreManager];
const VIEW: [Role; 3] = [Role::Cashier, Role::StoreManager, Role::FinanceOfficer];
const METHODS: [&str; 3] = ["cash", "card", "transfer"];

fn idempotency_key(h: &HeaderMap) -> ApiResult<String> {
    let k = h.get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::trim).unwrap_or("");
    if (8..=128).contains(&k.len()) && k.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)) {
        Ok(k.to_string())
    } else {
        Err(ApiError::bad("send an Idempotency-Key header (8 to 128 letters, digits or - _ . :) that is unique per attempt"))
    }
}

fn fingerprint(parts: &[&str]) -> String {
    hex::encode(Sha256::digest(parts.join("|").as_bytes()))
}

/// Serialises requests that carry the same key, so a double-click cannot run twice in parallel.
async fn lock_key(db: &mut PgConnection, scope: &str, business: Uuid, key: &str) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(format!("{scope}:{business}:{key}")).execute(&mut *db).await?;
    Ok(())
}

// ---------------------------------------------------------------- creating a sale

#[derive(Deserialize, Serialize)]
pub struct SaleLineIn {
    variant_id: Uuid,
    quantity: i64,
}

#[derive(Deserialize, Serialize)]
pub struct PaymentIn {
    method: String,
    amount_minor: i64,
    reference: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct SaleIn {
    lines: Vec<SaleLineIn>,
    payments: Vec<PaymentIn>,
    customer_id: Option<Uuid>,
}

struct Priced {
    variant_id: Uuid,
    name: String,
    sku: String,
    qty: i64,
    unit: i64,
    exempt: bool,
    rate: i32,
    subtotal: i64,
    tax: i64,
}

fn too_big() -> ApiError {
    ApiError::bad("that amount is too large")
}

pub async fn create_sale(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, headers: HeaderMap, Json(b): Json<SaleIn>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.authorize_store(&state, store, &SELL, "create_sale").await?;
    let key = idempotency_key(&headers)?;
    if b.lines.is_empty() || b.lines.len() > 100 {
        return Err(ApiError::bad("a sale needs between 1 and 100 lines"));
    }
    if b.lines.iter().any(|l| l.quantity <= 0 || l.quantity > 100_000) {
        return Err(ApiError::bad("quantities must be between 1 and 100000"));
    }
    if b.payments.is_empty() || b.payments.len() > 10 {
        return Err(ApiError::bad("a sale needs between 1 and 10 payments"));
    }
    for pay in &b.payments {
        if !METHODS.contains(&pay.method.as_str()) || pay.amount_minor <= 0 {
            return Err(ApiError::bad("each payment needs a method (cash, card or transfer) and a positive amount"));
        }
    }
    let hash = fingerprint(&[&store.to_string(), &p.user_id.to_string(), &serde_json::to_string(&b).unwrap_or_default()]);

    let mut tx = state.pool.begin().await?;
    lock_key(&mut tx, "sale", p.business_id, &key).await?;
    if let Some(row) = sqlx::query("SELECT id, request_hash FROM sales WHERE business_id = $1 AND idempotency_key = $2").bind(p.business_id).bind(&key).fetch_optional(&mut *tx).await? {
        if row.get::<String, _>("request_hash") != hash {
            return Err(ApiError::conflict("this Idempotency-Key was already used for a different sale"));
        }
        let id: Uuid = row.get("id");
        return Ok((StatusCode::OK, Json(sale_json(&mut tx, id).await?)));
    }

    let open: bool = sqlx::query_scalar("SELECT status = 'active' FROM stores WHERE id = $1").bind(store).fetch_one(&mut *tx).await?;
    if !open {
        return Err(ApiError::conflict("this store is not active"));
    }
    if let Some(c) = b.customer_id {
        let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM customers WHERE id = $1 AND business_id = $2 AND is_active)").bind(c).bind(p.business_id).fetch_one(&mut *tx).await?;
        if !ok {
            return Err(ApiError::NotFound("customer"));
        }
    }

    // Number the sale first: this locks the store's counter row until we commit, so numbers are issued
    // in order with no gaps, and a failed sale gives its number back.
    let nums = sqlx::query(
        "UPDATE store_settings SET next_receipt_number = next_receipt_number + 1, next_transaction_number = next_transaction_number + 1
         WHERE store_id = $1 RETURNING receipt_prefix, transaction_prefix, next_receipt_number - 1 AS r, next_transaction_number - 1 AS t",
    )
    .bind(store)
    .fetch_one(&mut *tx)
    .await?;
    let receipt_number = format!("{}-{:06}", nums.get::<String, _>("receipt_prefix"), nums.get::<i64, _>("r"));
    let transaction_number = format!("{}-{:06}", nums.get::<String, _>("transaction_prefix"), nums.get::<i64, _>("t"));

    let rate: i32 = sqlx::query_scalar("SELECT COALESCE((SELECT rate_bps FROM tax_rates WHERE business_id = $1 AND effective_from <= now() ORDER BY effective_from DESC, created_at DESC LIMIT 1), 0)")
        .bind(p.business_id)
        .fetch_one(&mut *tx)
        .await?;
    let currency: String = sqlx::query_scalar("SELECT base_currency::text FROM business_settings WHERE business_id = $1").bind(p.business_id).fetch_one(&mut *tx).await?;

    let mut priced = Vec::new();
    for l in &b.lines {
        let r = sqlx::query(
            "SELECT v.sku, v.status AS vstatus, pr.name, pr.status AS pstatus, pr.tax_exempt,
               COALESCE(
                 (SELECT x.price_minor FROM variant_prices x WHERE x.variant_id = v.id AND x.store_id = $3 AND x.effective_from <= now() ORDER BY x.effective_from DESC, x.created_at DESC LIMIT 1),
                 (SELECT x.price_minor FROM variant_prices x WHERE x.variant_id = v.id AND x.store_id IS NULL AND x.effective_from <= now() ORDER BY x.effective_from DESC, x.created_at DESC LIMIT 1)) AS price
             FROM product_variants v JOIN products pr ON pr.id = v.product_id WHERE v.id = $1 AND v.business_id = $2",
        )
        .bind(l.variant_id)
        .bind(p.business_id)
        .bind(store)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound("product"))?;
        let name: String = r.get("name");
        if r.get::<String, _>("vstatus") != "active" || r.get::<String, _>("pstatus") != "active" {
            return Err(ApiError::conflict(format!("{name} is not for sale")));
        }
        let unit: i64 = r.get::<Option<i64>, _>("price").ok_or_else(|| ApiError::conflict(format!("{name} has no price")))?;
        let exempt: bool = r.get("tax_exempt");
        let line_rate = if exempt { 0 } else { rate };
        let subtotal = unit.checked_mul(l.quantity).ok_or_else(too_big)?;
        priced.push(Priced { variant_id: l.variant_id, name, sku: r.get("sku"), qty: l.quantity, unit, exempt, rate: line_rate, subtotal, tax: tax_minor(subtotal, line_rate) });
    }
    let subtotal = priced.iter().try_fold(0i64, |a, l| a.checked_add(l.subtotal)).ok_or_else(too_big)?;
    let tax = priced.iter().try_fold(0i64, |a, l| a.checked_add(l.tax)).ok_or_else(too_big)?;
    let total = subtotal + tax;

    let paid = b.payments.iter().try_fold(0i64, |a, x| a.checked_add(x.amount_minor)).ok_or_else(too_big)?;
    let cash: i64 = b.payments.iter().filter(|x| x.method == "cash").map(|x| x.amount_minor).sum();
    let change = paid - total;
    if change < 0 {
        return Err(ApiError::bad(format!("payments of {paid} do not cover the total of {total}")));
    }
    // Only cash can exceed the total, and change must come out of cash that actually went towards the sale.
    if paid - cash > total || (change > 0 && change >= cash) {
        return Err(ApiError::bad("only cash can be paid over the total (to give change)"));
    }

    let sale_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sales (id, business_id, store_id, cashier_id, customer_id, receipt_number, transaction_number, currency, subtotal_minor, tax_minor, total_minor, paid_minor, change_minor, idempotency_key, request_hash)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
    )
    .bind(sale_id).bind(p.business_id).bind(store).bind(p.user_id).bind(b.customer_id).bind(&receipt_number).bind(&transaction_number).bind(&currency)
    .bind(subtotal).bind(tax).bind(total).bind(paid).bind(change).bind(&key).bind(&hash)
    .execute(&mut *tx)
    .await?;

    for l in &priced {
        let line_id: Uuid = sqlx::query_scalar(
            "INSERT INTO sale_lines (sale_id, variant_id, product_name, sku, quantity, unit_price_minor, tax_exempt, tax_rate_bps, line_subtotal_minor, line_tax_minor, line_total_minor)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id",
        )
        .bind(sale_id).bind(l.variant_id).bind(&l.name).bind(&l.sku).bind(l.qty).bind(l.unit).bind(l.exempt).bind(l.rate).bind(l.subtotal).bind(l.tax).bind(l.subtotal + l.tax)
        .fetch_one(&mut *tx)
        .await?;
        let base = Move { business_id: p.business_id, store_id: store, variant_id: l.variant_id, batch_id: None, bucket: Bucket::OnHand, delta: 0, kind: "sale", group_id: sale_id, reason: None, reference: Some(("sale", sale_id.to_string())), actor: p.user_id };
        let taken = consume(&mut tx, &base, l.qty).await.map_err(|e| match e {
            ApiError::Conflict(_) => ApiError::conflict(format!("not enough stock for {}", l.name)),
            other => other,
        })?;
        for (batch, q) in taken {
            sqlx::query("INSERT INTO sale_line_allocations (sale_line_id, batch_id, quantity) VALUES ($1,$2,$3)").bind(line_id).bind(batch).bind(q).execute(&mut *tx).await?;
        }
    }
    for pay in &b.payments {
        sqlx::query("INSERT INTO sale_payments (sale_id, method, amount_minor, reference) VALUES ($1,$2,$3,$4)")
            .bind(sale_id).bind(&pay.method).bind(pay.amount_minor).bind(pay.reference.as_deref().map(str::trim).filter(|s| !s.is_empty()))
            .execute(&mut *tx)
            .await?;
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "create_sale", entity_type: "sale", entity_id: Some(sale_id.to_string()), after: Some(json!({ "receipt_number": receipt_number, "total_minor": total })), ..Default::default() }).await?;
    let out = sale_json(&mut tx, sale_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// ---------------------------------------------------------------- reading sales and receipts

async fn sale_json(db: &mut PgConnection, id: Uuid) -> ApiResult<Value> {
    let s = sqlx::query(
        "SELECT id, store_id, cashier_id, customer_id, receipt_number, transaction_number, currency::text AS currency, subtotal_minor, tax_minor, total_minor, paid_minor, change_minor, created_at
         FROM sales WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    let lines = sqlx::query("SELECT id, variant_id, product_name, sku, quantity, unit_price_minor, tax_exempt, tax_rate_bps, line_subtotal_minor, line_tax_minor, line_total_minor, returned_qty FROM sale_lines WHERE sale_id = $1 ORDER BY product_name, id")
        .bind(id)
        .fetch_all(&mut *db)
        .await?;
    let lines: Vec<Value> = lines.iter().map(|r| json!({
        "id": r.get::<Uuid, _>("id"), "variant_id": r.get::<Uuid, _>("variant_id"), "product_name": r.get::<String, _>("product_name"), "sku": r.get::<String, _>("sku"),
        "quantity": r.get::<i64, _>("quantity"), "unit_price_minor": r.get::<i64, _>("unit_price_minor"), "tax_exempt": r.get::<bool, _>("tax_exempt"), "tax_rate_bps": r.get::<i32, _>("tax_rate_bps"),
        "line_subtotal_minor": r.get::<i64, _>("line_subtotal_minor"), "line_tax_minor": r.get::<i64, _>("line_tax_minor"), "line_total_minor": r.get::<i64, _>("line_total_minor"),
        "returned_qty": r.get::<i64, _>("returned_qty"),
    })).collect();
    let pays = sqlx::query("SELECT method, amount_minor, reference FROM sale_payments WHERE sale_id = $1 ORDER BY id").bind(id).fetch_all(&mut *db).await?;
    let pays: Vec<Value> = pays.iter().map(|r| json!({ "method": r.get::<String, _>("method"), "amount_minor": r.get::<i64, _>("amount_minor"), "reference": r.get::<Option<String>, _>("reference") })).collect();
    let rets = sqlx::query("SELECT id, return_number, refund_minor, refund_method, created_at FROM returns WHERE sale_id = $1 ORDER BY created_at").bind(id).fetch_all(&mut *db).await?;
    let rets: Vec<Value> = rets.iter().map(|r| json!({ "id": r.get::<Uuid, _>("id"), "return_number": r.get::<String, _>("return_number"), "refund_minor": r.get::<i64, _>("refund_minor"), "refund_method": r.get::<String, _>("refund_method"), "created_at": r.get::<DateTime<Utc>, _>("created_at") })).collect();
    Ok(json!({
        "id": s.get::<Uuid, _>("id"), "store_id": s.get::<Uuid, _>("store_id"), "cashier_id": s.get::<Uuid, _>("cashier_id"), "customer_id": s.get::<Option<Uuid>, _>("customer_id"),
        "receipt_number": s.get::<String, _>("receipt_number"), "transaction_number": s.get::<String, _>("transaction_number"), "currency": s.get::<String, _>("currency"),
        "subtotal_minor": s.get::<i64, _>("subtotal_minor"), "tax_minor": s.get::<i64, _>("tax_minor"), "total_minor": s.get::<i64, _>("total_minor"),
        "paid_minor": s.get::<i64, _>("paid_minor"), "change_minor": s.get::<i64, _>("change_minor"), "created_at": s.get::<DateTime<Utc>, _>("created_at"),
        "lines": lines, "payments": pays, "returns": rets,
    }))
}

/// Finds a sale in the caller's business and checks they may see it: managers and finance see the
/// whole store; a cashier sees only the sales they rang up themselves.
async fn visible_sale(state: &AppState, p: &Principal, id: Uuid, action: &str) -> ApiResult<Uuid> {
    let row = sqlx::query("SELECT store_id, cashier_id FROM sales WHERE id = $1 AND business_id = $2").bind(id).bind(p.business_id).fetch_optional(&state.pool).await?.ok_or(ApiError::NotFound("sale"))?;
    let (store, cashier): (Uuid, Uuid) = (row.get("store_id"), row.get("cashier_id"));
    p.authorize_store(state, store, &VIEW, action).await?;
    if cashier != p.user_id && !p.has_role_at(store, &[Role::StoreManager, Role::FinanceOfficer]) {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), action).await;
        return Err(ApiError::Forbidden);
    }
    Ok(store)
}

pub async fn get_sale(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    visible_sale(&state, &p, id, "view_sale").await?;
    let mut conn = state.pool.acquire().await?;
    Ok(Json(sale_json(&mut conn, id).await?))
}

/// The printable receipt: the sale plus the business and store details and the receipt header and
/// footer, with the time shown in the business's own timezone.
pub async fn receipt(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let store = visible_sale(&state, &p, id, "view_receipt").await?;
    let mut conn = state.pool.acquire().await?;
    let mut sale = sale_json(&mut conn, id).await?;
    let b = sqlx::query(
        "SELECT b.legal_name, b.trading_name, b.address, b.phone, s.receipt_header, s.receipt_footer, s.timezone,
                to_char(sa.created_at AT TIME ZONE s.timezone, 'YYYY-MM-DD HH24:MI:SS') AS local_time
         FROM businesses b JOIN business_settings s ON s.business_id = b.id JOIN sales sa ON sa.business_id = b.id WHERE sa.id = $1",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    let st = sqlx::query("SELECT name, address, phone FROM stores WHERE id = $1").bind(store).fetch_one(&mut *conn).await?;
    sale["business"] = json!({ "name": b.get::<Option<String>, _>("trading_name").unwrap_or_else(|| b.get::<String, _>("legal_name")), "legal_name": b.get::<String, _>("legal_name"), "address": b.get::<Option<String>, _>("address"), "phone": b.get::<Option<String>, _>("phone") });
    sale["store"] = json!({ "name": st.get::<String, _>("name"), "address": st.get::<Option<String>, _>("address"), "phone": st.get::<Option<String>, _>("phone") });
    sale["header"] = json!(b.get::<Option<String>, _>("receipt_header"));
    sale["footer"] = json!(b.get::<Option<String>, _>("receipt_footer"));
    sale["timezone"] = json!(b.get::<String, _>("timezone"));
    sale["local_time"] = json!(b.get::<String, _>("local_time"));
    Ok(Json(sale))
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
}

pub async fn list_sales(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &VIEW, "list_sales").await?;
    let all = p.has_role_at(store, &[Role::StoreManager, Role::FinanceOfficer]);
    let rows = sqlx::query(
        "SELECT id, receipt_number, total_minor, cashier_id, created_at FROM sales WHERE store_id = $1 AND ($2 OR cashier_id = $3) ORDER BY created_at DESC, id LIMIT $4",
    )
    .bind(store)
    .bind(all)
    .bind(p.user_id)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows.iter().map(|r| json!({ "id": r.get::<Uuid, _>("id"), "receipt_number": r.get::<String, _>("receipt_number"), "total_minor": r.get::<i64, _>("total_minor"), "cashier_id": r.get::<Uuid, _>("cashier_id"), "created_at": r.get::<DateTime<Utc>, _>("created_at") })).collect();
    Ok(Json(json!({ "items": items })))
}

// ---------------------------------------------------------------- returns and refunds

#[derive(Deserialize, Serialize)]
pub struct ReturnLineIn {
    sale_line_id: Uuid,
    quantity: i64,
    /// `restock` puts the goods back on the shelf; `damaged` records them as damaged stock.
    disposition: String,
}

#[derive(Deserialize, Serialize)]
pub struct ReturnIn {
    lines: Vec<ReturnLineIn>,
    reason: String,
    refund_method: String,
}

async fn return_json(db: &mut PgConnection, id: Uuid) -> ApiResult<Value> {
    let r = sqlx::query("SELECT id, sale_id, store_id, return_number, reason, refund_method, refund_minor, created_by, created_at FROM returns WHERE id = $1").bind(id).fetch_one(&mut *db).await?;
    let lines = sqlx::query("SELECT sale_line_id, quantity, refund_minor, disposition FROM return_lines WHERE return_id = $1 ORDER BY id").bind(id).fetch_all(&mut *db).await?;
    let lines: Vec<Value> = lines.iter().map(|l| json!({ "sale_line_id": l.get::<Uuid, _>("sale_line_id"), "quantity": l.get::<i64, _>("quantity"), "refund_minor": l.get::<i64, _>("refund_minor"), "disposition": l.get::<String, _>("disposition") })).collect();
    Ok(json!({
        "id": r.get::<Uuid, _>("id"), "sale_id": r.get::<Uuid, _>("sale_id"), "store_id": r.get::<Uuid, _>("store_id"), "return_number": r.get::<String, _>("return_number"),
        "reason": r.get::<String, _>("reason"), "refund_method": r.get::<String, _>("refund_method"), "refund_minor": r.get::<i64, _>("refund_minor"),
        "created_by": r.get::<Uuid, _>("created_by"), "created_at": r.get::<DateTime<Utc>, _>("created_at"), "lines": lines,
    }))
}

pub async fn create_return(State(state): State<AppState>, p: Principal, Path(sale_id): Path<Uuid>, headers: HeaderMap, Json(b): Json<ReturnIn>) -> ApiResult<(StatusCode, Json<Value>)> {
    let sale = sqlx::query("SELECT store_id FROM sales WHERE id = $1 AND business_id = $2").bind(sale_id).bind(p.business_id).fetch_optional(&state.pool).await?.ok_or(ApiError::NotFound("sale"))?;
    let store: Uuid = sale.get("store_id");
    p.authorize_store(&state, store, &SELL, "create_return").await?;
    let key = idempotency_key(&headers)?;
    if b.lines.is_empty() || b.lines.len() > 100 || b.lines.iter().any(|l| l.quantity <= 0) {
        return Err(ApiError::bad("a return needs 1 to 100 lines with positive quantities"));
    }
    if b.lines.iter().any(|l| l.disposition != "restock" && l.disposition != "damaged") {
        return Err(ApiError::bad("disposition must be restock or damaged"));
    }
    if b.reason.trim().is_empty() || !METHODS.contains(&b.refund_method.as_str()) {
        return Err(ApiError::bad("a reason and a refund method (cash, card or transfer) are required"));
    }
    let hash = fingerprint(&[&sale_id.to_string(), &p.user_id.to_string(), &serde_json::to_string(&b).unwrap_or_default()]);

    let mut tx = state.pool.begin().await?;
    lock_key(&mut tx, "return", p.business_id, &key).await?;
    if let Some(row) = sqlx::query("SELECT id, request_hash FROM returns WHERE business_id = $1 AND idempotency_key = $2").bind(p.business_id).bind(&key).fetch_optional(&mut *tx).await? {
        if row.get::<String, _>("request_hash") != hash {
            return Err(ApiError::conflict("this Idempotency-Key was already used for a different return"));
        }
        let id: Uuid = row.get("id");
        return Ok((StatusCode::OK, Json(return_json(&mut tx, id).await?)));
    }

    let return_id = Uuid::new_v4();
    let mut done: Vec<(Uuid, i64, i64, String)> = Vec::new(); // (sale line, qty, refund, disposition)
    for l in &b.lines {
        let line = sqlx::query("SELECT variant_id, quantity, returned_qty, line_total_minor FROM sale_lines WHERE id = $1 AND sale_id = $2 FOR UPDATE")
            .bind(l.sale_line_id)
            .bind(sale_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::NotFound("sale line"))?;
        let (variant, qty, before, total): (Uuid, i64, i64, i64) = (line.get("variant_id"), line.get("quantity"), line.get("returned_qty"), line.get("line_total_minor"));
        if before + l.quantity > qty {
            return Err(ApiError::conflict(format!("only {} of this line can still be returned", qty - before)));
        }
        sqlx::query("UPDATE sale_lines SET returned_qty = returned_qty + $2 WHERE id = $1").bind(l.sale_line_id).bind(l.quantity).execute(&mut *tx).await?;
        let refund = share(total, before + l.quantity, qty) - share(total, before, qty);

        // Put the goods back where they came from (or into damaged stock), via the shared service.
        let mut left = l.quantity;
        let allocs = sqlx::query("SELECT id, batch_id, quantity - returned_qty AS avail FROM sale_line_allocations WHERE sale_line_id = $1 AND quantity > returned_qty ORDER BY seq FOR UPDATE").bind(l.sale_line_id).fetch_all(&mut *tx).await?;
        for a in allocs {
            if left == 0 {
                break;
            }
            let (alloc_id, batch, avail): (Uuid, Option<Uuid>, i64) = (a.get("id"), a.get("batch_id"), a.get("avail"));
            let take = avail.min(left);
            left -= take;
            sqlx::query("UPDATE sale_line_allocations SET returned_qty = returned_qty + $2 WHERE id = $1").bind(alloc_id).bind(take).execute(&mut *tx).await?;
            let bucket = if l.disposition == "restock" { Bucket::OnHand } else { Bucket::Damaged };
            apply(&mut tx, &Move { business_id: p.business_id, store_id: store, variant_id: variant, batch_id: batch, bucket, delta: take, kind: "return", group_id: return_id, reason: Some(b.reason.trim()), reference: Some(("return", return_id.to_string())), actor: p.user_id }).await?;
        }
        if left != 0 {
            return Err(ApiError::Internal("sale line allocations do not match its quantity".into()));
        }
        done.push((l.sale_line_id, l.quantity, refund, l.disposition.clone()));
    }
    let refund_total: i64 = done.iter().map(|d| d.2).sum();

    // Late returns and big refunds need a store manager (or a business-wide role).
    let late: bool = sqlx::query_scalar("SELECT created_at < now() - make_interval(days => $2::int) FROM sales WHERE id = $1").bind(sale_id).bind(state.config.return_window_days as i32).fetch_one(&mut *tx).await?;
    if (late || refund_total > state.config.return_manager_limit_minor) && !p.has_role_at(store, &[Role::StoreManager]) {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), "return_needs_manager").await;
        return Err(ApiError::Forbidden);
    }

    let n: i64 = sqlx::query_scalar("UPDATE store_settings SET next_return_number = next_return_number + 1 WHERE store_id = $1 RETURNING next_return_number - 1").bind(store).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO returns (id, business_id, store_id, sale_id, return_number, reason, refund_method, refund_minor, created_by, idempotency_key, request_hash) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
        .bind(return_id).bind(p.business_id).bind(store).bind(sale_id).bind(format!("RTN-{n:06}")).bind(b.reason.trim()).bind(&b.refund_method).bind(refund_total).bind(p.user_id).bind(&key).bind(&hash)
        .execute(&mut *tx)
        .await?;
    for (line, qty, refund, disposition) in &done {
        sqlx::query("INSERT INTO return_lines (return_id, sale_line_id, quantity, refund_minor, disposition) VALUES ($1,$2,$3,$4,$5)").bind(return_id).bind(line).bind(qty).bind(refund).bind(disposition).execute(&mut *tx).await?;
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "create_return", entity_type: "return", entity_id: Some(return_id.to_string()), after: Some(json!({ "sale_id": sale_id, "refund_minor": refund_total, "reason": b.reason.trim() })), ..Default::default() }).await?;
    let out = return_json(&mut tx, return_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}
