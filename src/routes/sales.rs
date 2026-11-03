//! Point-of-sale: sales, payments, receipts and returns (FR-15 to FR-18).
//!
//! A sale is one transaction: prices and tax are decided by the server (never the till), stock is taken
//! through the shared inventory service, and receipt numbers come from a per-store counter advanced in the
//! same transaction. Creating a sale or a return needs an `Idempotency-Key` header, so a retried request
//! (flaky network, double tap) returns the original instead of selling twice.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
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
const AUDITORS: [Role; 2] = [Role::StoreManager, Role::FinanceOfficer];
const METHODS: [&str; 4] = ["cash", "card", "transfer", "other"];

/// SQL for a sale's payment status, for a row aliased `sa`. A sale is `paid` only when successful payments
/// (less any change given) cover the total; `pending` while a payment is awaiting confirmation.
const PAYMENT_STATUS: &str = "CASE
    WHEN EXISTS (SELECT 1 FROM sale_lines l WHERE l.sale_id = sa.id)
         AND NOT EXISTS (SELECT 1 FROM sale_lines l WHERE l.sale_id = sa.id AND l.returned_qty < l.quantity) THEN 'refunded'
    WHEN COALESCE((SELECT sum(pp.amount_minor) FROM sale_payments pp WHERE pp.sale_id = sa.id AND pp.status IN ('successful', 'refunded')), 0) - sa.change_minor >= sa.total_minor THEN 'paid'
    WHEN EXISTS (SELECT 1 FROM sale_payments pp WHERE pp.sale_id = sa.id AND pp.status = 'pending') THEN 'pending'
    ELSE 'unpaid' END";

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

// ---------------------------------------------------------------- pricing a basket (FR-15)

/// A basket line names the product by `variant_id`, scanned `barcode`, or `sku` (exactly one).
#[derive(Deserialize, Serialize)]
pub struct SaleLineIn {
    variant_id: Option<Uuid>,
    barcode: Option<String>,
    sku: Option<String>,
    quantity: i64,
    /// A discount on this line, in minor units, taken off before tax.
    discount_minor: Option<i64>,
}

#[derive(Deserialize, Serialize)]
pub struct PaymentIn {
    method: String,
    amount_minor: i64,
    reference: Option<String>,
    /// `successful` (default) or, for card and transfer only, `pending` until confirmed.
    status: Option<String>,
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
    discount: i64,
    /// After the discount: the amount tax is charged on.
    subtotal: i64,
    tax: i64,
}

fn too_big() -> ApiError {
    ApiError::bad("that amount is too large")
}

fn check_lines(lines: &[SaleLineIn]) -> ApiResult<()> {
    if lines.is_empty() || lines.len() > 100 {
        return Err(ApiError::bad("a sale needs between 1 and 100 lines"));
    }
    if lines.iter().any(|l| l.quantity <= 0 || l.quantity > 100_000) {
        return Err(ApiError::bad("quantities must be between 1 and 100000"));
    }
    if lines.iter().any(|l| l.discount_minor.is_some_and(|d| d < 0)) {
        return Err(ApiError::bad("a discount cannot be negative"));
    }
    Ok(())
}

async fn resolve_variant(db: &mut PgConnection, business: Uuid, l: &SaleLineIn) -> ApiResult<Uuid> {
    let named = [l.variant_id.is_some(), l.barcode.is_some(), l.sku.is_some()].iter().filter(|x| **x).count();
    if named != 1 {
        return Err(ApiError::bad("name each line's product with exactly one of variant_id, barcode or sku"));
    }
    if let Some(v) = l.variant_id {
        return Ok(v);
    }
    let found: Option<Uuid> = if let Some(code) = &l.barcode {
        sqlx::query_scalar("SELECT id FROM product_variants WHERE business_id = $1 AND barcode = $2").bind(business).bind(code.trim()).fetch_optional(&mut *db).await?
    } else {
        sqlx::query_scalar("SELECT id FROM product_variants WHERE business_id = $1 AND lower(sku) = lower($2)").bind(business).bind(l.sku.as_deref().unwrap_or("").trim()).fetch_optional(&mut *db).await?
    };
    found.ok_or(ApiError::NotFound("product"))
}

/// Looks up each product, takes its current price for this store, applies the line discount, then tax on
/// what remains. Shared by the quote and the sale so the two always agree.
async fn price_lines(db: &mut PgConnection, state: &AppState, p: &Principal, store: Uuid, lines: &[SaleLineIn]) -> ApiResult<Vec<Priced>> {
    let rate: i32 = sqlx::query_scalar("SELECT COALESCE((SELECT rate_bps FROM tax_rates WHERE business_id = $1 AND effective_from <= now() ORDER BY effective_from DESC, created_at DESC LIMIT 1), 0)")
        .bind(p.business_id)
        .fetch_one(&mut *db)
        .await?;
    let is_manager = p.has_role_at(store, &[Role::StoreManager]);
    let mut priced = Vec::new();
    for l in lines {
        let variant = resolve_variant(db, p.business_id, l).await?;
        let r = sqlx::query(
            "SELECT v.sku, v.status AS vstatus, pr.name, pr.status AS pstatus, pr.tax_exempt,
               COALESCE(
                 (SELECT x.price_minor FROM variant_prices x WHERE x.variant_id = v.id AND x.store_id = $3 AND x.effective_from <= now() ORDER BY x.effective_from DESC, x.created_at DESC LIMIT 1),
                 (SELECT x.price_minor FROM variant_prices x WHERE x.variant_id = v.id AND x.store_id IS NULL AND x.effective_from <= now() ORDER BY x.effective_from DESC, x.created_at DESC LIMIT 1)) AS price
             FROM product_variants v JOIN products pr ON pr.id = v.product_id WHERE v.id = $1 AND v.business_id = $2",
        )
        .bind(variant)
        .bind(p.business_id)
        .bind(store)
        .fetch_optional(&mut *db)
        .await?
        .ok_or(ApiError::NotFound("product"))?;
        let name: String = r.get("name");
        if r.get::<String, _>("vstatus") != "active" || r.get::<String, _>("pstatus") != "active" {
            return Err(ApiError::conflict(format!("{name} is not for sale")));
        }
        let unit: i64 = r.get::<Option<i64>, _>("price").ok_or_else(|| ApiError::conflict(format!("{name} has no price")))?;
        let exempt: bool = r.get("tax_exempt");
        let line_rate = if exempt { 0 } else { rate };
        let gross = unit.checked_mul(l.quantity).ok_or_else(too_big)?;
        let discount = l.discount_minor.unwrap_or(0);
        if discount > gross {
            return Err(ApiError::bad(format!("the discount on {name} is more than the line is worth")));
        }
        // Discounts above the cashier limit need a manager (FR-29): the limit is a share of the line's value.
        if !is_manager && (discount as i128) * 10_000 > (gross as i128) * (state.config.cashier_max_discount_bps as i128) {
            audit::denied(&state.pool, p.business_id, p.user_id, Some(store), "discount_needs_manager").await;
            return Err(ApiError::Forbidden);
        }
        let subtotal = gross - discount;
        priced.push(Priced { variant_id: variant, name, sku: r.get("sku"), qty: l.quantity, unit, exempt, rate: line_rate, discount, subtotal, tax: tax_minor(subtotal, line_rate) });
    }
    Ok(priced)
}

fn totals(priced: &[Priced]) -> ApiResult<(i64, i64)> {
    let subtotal = priced.iter().try_fold(0i64, |a, l| a.checked_add(l.subtotal)).ok_or_else(too_big)?;
    let tax = priced.iter().try_fold(0i64, |a, l| a.checked_add(l.tax)).ok_or_else(too_big)?;
    Ok((subtotal, tax))
}

#[derive(Deserialize)]
pub struct QuoteIn {
    lines: Vec<SaleLineIn>,
}

/// Prices a basket without selling anything: what the cashier reviews before taking payment.
pub async fn quote(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Json(b): Json<QuoteIn>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &SELL, "quote_sale").await?;
    check_lines(&b.lines)?;
    let mut conn = state.pool.acquire().await?;
    let priced = price_lines(&mut conn, &state, &p, store, &b.lines).await?;
    let (subtotal, tax) = totals(&priced)?;
    let currency: String = sqlx::query_scalar("SELECT base_currency::text FROM business_settings WHERE business_id = $1").bind(p.business_id).fetch_one(&mut *conn).await?;
    let lines: Vec<Value> = priced.iter().map(|l| json!({
        "variant_id": l.variant_id, "product_name": l.name, "sku": l.sku, "quantity": l.qty, "unit_price_minor": l.unit, "discount_minor": l.discount,
        "tax_exempt": l.exempt, "tax_rate_bps": l.rate, "line_subtotal_minor": l.subtotal, "line_tax_minor": l.tax, "line_total_minor": l.subtotal + l.tax,
    })).collect();
    Ok(Json(json!({ "currency": currency, "lines": lines, "subtotal_minor": subtotal, "tax_minor": tax, "total_minor": subtotal + tax })))
}

// ---------------------------------------------------------------- creating a sale

fn payment_status_for(pay: &PaymentIn) -> ApiResult<&'static str> {
    if !METHODS.contains(&pay.method.as_str()) || pay.amount_minor <= 0 {
        return Err(ApiError::bad("each payment needs a method (cash, card, transfer or other) and a positive amount"));
    }
    match (pay.status.as_deref().unwrap_or("successful"), pay.method.as_str()) {
        ("successful", _) => Ok("successful"),
        ("pending", "cash") => Err(ApiError::bad("a cash payment cannot be pending")),
        ("pending", _) => Ok("pending"),
        _ => Err(ApiError::bad("a new payment is either successful or pending")),
    }
}

pub async fn create_sale(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, headers: HeaderMap, Json(b): Json<SaleIn>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.authorize_store(&state, store, &SELL, "create_sale").await?;
    let key = idempotency_key(&headers)?;
    check_lines(&b.lines)?;
    if b.payments.is_empty() || b.payments.len() > 10 {
        return Err(ApiError::bad("a sale needs between 1 and 10 payments"));
    }
    let pay_status: Vec<&str> = b.payments.iter().map(payment_status_for).collect::<ApiResult<_>>()?;
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

    let currency: String = sqlx::query_scalar("SELECT base_currency::text FROM business_settings WHERE business_id = $1").bind(p.business_id).fetch_one(&mut *tx).await?;
    let priced = price_lines(&mut tx, &state, &p, store, &b.lines).await?;
    let (subtotal, tax) = totals(&priced)?;
    let total = subtotal + tax;

    // Payments (successful or awaiting confirmation) must cover the total; only cash can go over it, and
    // change must come out of cash that actually went towards the sale.
    let paid = b.payments.iter().try_fold(0i64, |a, x| a.checked_add(x.amount_minor)).ok_or_else(too_big)?;
    let cash: i64 = b.payments.iter().filter(|x| x.method == "cash").map(|x| x.amount_minor).sum();
    let change = paid - total;
    if change < 0 {
        return Err(ApiError::bad(format!("payments of {paid} do not cover the total of {total}")));
    }
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
            "INSERT INTO sale_lines (sale_id, variant_id, product_name, sku, quantity, unit_price_minor, tax_exempt, tax_rate_bps, line_subtotal_minor, line_tax_minor, line_total_minor, discount_minor)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING id",
        )
        .bind(sale_id).bind(l.variant_id).bind(&l.name).bind(&l.sku).bind(l.qty).bind(l.unit).bind(l.exempt).bind(l.rate).bind(l.subtotal).bind(l.tax).bind(l.subtotal + l.tax).bind(l.discount)
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
    for (pay, status) in b.payments.iter().zip(&pay_status) {
        sqlx::query("INSERT INTO sale_payments (sale_id, method, amount_minor, reference, status) VALUES ($1,$2,$3,$4,$5)")
            .bind(sale_id).bind(&pay.method).bind(pay.amount_minor).bind(pay.reference.as_deref().map(str::trim).filter(|s| !s.is_empty())).bind(status)
            .execute(&mut *tx)
            .await
            .map_err(|e| match ApiError::from(e) {
                ApiError::Conflict(_) => ApiError::bad("the same payment reference was used twice on this sale"),
                other => other,
            })?;
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "create_sale", entity_type: "sale", entity_id: Some(sale_id.to_string()), after: Some(json!({ "receipt_number": receipt_number, "total_minor": total })), ..Default::default() }).await?;
    let out = sale_json(&mut tx, sale_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// ---------------------------------------------------------------- reading sales and receipts

async fn sale_json(db: &mut PgConnection, id: Uuid) -> ApiResult<Value> {
    let s = sqlx::query(&format!(
        "SELECT sa.id, sa.store_id, sa.cashier_id, sa.customer_id, sa.receipt_number, sa.transaction_number, sa.currency::text AS currency, sa.subtotal_minor, sa.tax_minor, sa.total_minor,
                sa.paid_minor, sa.change_minor, sa.created_at, {PAYMENT_STATUS} AS payment_status
         FROM sales sa WHERE sa.id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    let lines = sqlx::query("SELECT id, variant_id, product_name, sku, quantity, unit_price_minor, discount_minor, tax_exempt, tax_rate_bps, line_subtotal_minor, line_tax_minor, line_total_minor, returned_qty FROM sale_lines WHERE sale_id = $1 ORDER BY product_name, id")
        .bind(id)
        .fetch_all(&mut *db)
        .await?;
    let lines: Vec<Value> = lines.iter().map(|r| json!({
        "id": r.get::<Uuid, _>("id"), "variant_id": r.get::<Uuid, _>("variant_id"), "product_name": r.get::<String, _>("product_name"), "sku": r.get::<String, _>("sku"),
        "quantity": r.get::<i64, _>("quantity"), "unit_price_minor": r.get::<i64, _>("unit_price_minor"), "discount_minor": r.get::<i64, _>("discount_minor"),
        "tax_exempt": r.get::<bool, _>("tax_exempt"), "tax_rate_bps": r.get::<i32, _>("tax_rate_bps"),
        "line_subtotal_minor": r.get::<i64, _>("line_subtotal_minor"), "line_tax_minor": r.get::<i64, _>("line_tax_minor"), "line_total_minor": r.get::<i64, _>("line_total_minor"),
        "returned_qty": r.get::<i64, _>("returned_qty"),
    })).collect();
    let pays = sqlx::query("SELECT id, method, amount_minor, reference, status FROM sale_payments WHERE sale_id = $1 ORDER BY id").bind(id).fetch_all(&mut *db).await?;
    let pays: Vec<Value> = pays.iter().map(|r| json!({ "id": r.get::<Uuid, _>("id"), "method": r.get::<String, _>("method"), "amount_minor": r.get::<i64, _>("amount_minor"), "reference": r.get::<Option<String>, _>("reference"), "status": r.get::<String, _>("status") })).collect();
    let rets = sqlx::query("SELECT id, return_number, refund_minor, refund_method, refund_status, created_at FROM returns WHERE sale_id = $1 ORDER BY created_at").bind(id).fetch_all(&mut *db).await?;
    let rets: Vec<Value> = rets.iter().map(|r| json!({ "id": r.get::<Uuid, _>("id"), "return_number": r.get::<String, _>("return_number"), "refund_minor": r.get::<i64, _>("refund_minor"), "refund_method": r.get::<String, _>("refund_method"), "refund_status": r.get::<String, _>("refund_status"), "created_at": r.get::<DateTime<Utc>, _>("created_at") })).collect();
    Ok(json!({
        "id": s.get::<Uuid, _>("id"), "store_id": s.get::<Uuid, _>("store_id"), "cashier_id": s.get::<Uuid, _>("cashier_id"), "customer_id": s.get::<Option<Uuid>, _>("customer_id"),
        "receipt_number": s.get::<String, _>("receipt_number"), "transaction_number": s.get::<String, _>("transaction_number"), "currency": s.get::<String, _>("currency"),
        "subtotal_minor": s.get::<i64, _>("subtotal_minor"), "tax_minor": s.get::<i64, _>("tax_minor"), "total_minor": s.get::<i64, _>("total_minor"),
        "paid_minor": s.get::<i64, _>("paid_minor"), "change_minor": s.get::<i64, _>("change_minor"), "payment_status": s.get::<String, _>("payment_status"),
        "created_at": s.get::<DateTime<Utc>, _>("created_at"), "lines": lines, "payments": pays, "returns": rets,
    }))
}

/// Finds a sale in the caller's business and checks they may see it: managers and finance see the
/// whole store; a cashier sees only the sales they rang up themselves.
async fn visible_sale(state: &AppState, p: &Principal, id: Uuid, action: &str) -> ApiResult<Uuid> {
    let row = sqlx::query("SELECT store_id, cashier_id FROM sales WHERE id = $1 AND business_id = $2").bind(id).bind(p.business_id).fetch_optional(&state.pool).await?.ok_or(ApiError::NotFound("sale"))?;
    let (store, cashier): (Uuid, Uuid) = (row.get("store_id"), row.get("cashier_id"));
    p.authorize_store(state, store, &VIEW, action).await?;
    if cashier != p.user_id && !p.has_role_at(store, &AUDITORS) {
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

/// The sale plus the business and store details, receipt header and footer, and local time.
async fn receipt_json(db: &mut PgConnection, id: Uuid, store: Uuid) -> ApiResult<Value> {
    let mut sale = sale_json(db, id).await?;
    let b = sqlx::query(
        "SELECT b.legal_name, b.trading_name, b.address, b.phone, s.receipt_header, s.receipt_footer, s.timezone,
                to_char(sa.created_at AT TIME ZONE s.timezone, 'YYYY-MM-DD HH24:MI:SS') AS local_time, u.full_name AS cashier_name
         FROM businesses b JOIN business_settings s ON s.business_id = b.id JOIN sales sa ON sa.business_id = b.id JOIN users u ON u.id = sa.cashier_id WHERE sa.id = $1",
    )
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    let st = sqlx::query("SELECT name, address, phone FROM stores WHERE id = $1").bind(store).fetch_one(&mut *db).await?;
    sale["business"] = json!({ "name": b.get::<Option<String>, _>("trading_name").unwrap_or_else(|| b.get::<String, _>("legal_name")), "legal_name": b.get::<String, _>("legal_name"), "address": b.get::<Option<String>, _>("address"), "phone": b.get::<Option<String>, _>("phone") });
    sale["store"] = json!({ "name": st.get::<String, _>("name"), "address": st.get::<Option<String>, _>("address"), "phone": st.get::<Option<String>, _>("phone") });
    sale["cashier_name"] = json!(b.get::<String, _>("cashier_name"));
    sale["header"] = json!(b.get::<Option<String>, _>("receipt_header"));
    sale["footer"] = json!(b.get::<Option<String>, _>("receipt_footer"));
    sale["timezone"] = json!(b.get::<String, _>("timezone"));
    sale["local_time"] = json!(b.get::<String, _>("local_time"));
    let prints: i64 = sqlx::query_scalar("SELECT count(*) FROM receipt_prints WHERE sale_id = $1").bind(id).fetch_one(&mut *db).await?;
    sale["print_count"] = json!(prints);
    Ok(sale)
}

/// Read-only: the receipt as it would print. Use `POST .../receipt/print` to record a print.
pub async fn receipt(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let store = visible_sale(&state, &p, id, "view_receipt").await?;
    let mut conn = state.pool.acquire().await?;
    Ok(Json(receipt_json(&mut conn, id, store).await?))
}

/// Records that the receipt was printed. The first print is the original; every later one is marked as a
/// reprint (`reprint: true`, `copy_label`) and is limited to managers and finance staff (FR-17).
pub async fn print_receipt(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let store = visible_sale(&state, &p, id, "print_receipt").await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(format!("print:{id}")).execute(&mut *tx).await?;
    let prior: i64 = sqlx::query_scalar("SELECT count(*) FROM receipt_prints WHERE sale_id = $1").bind(id).fetch_one(&mut *tx).await?;
    if prior > 0 && !p.has_role_at(store, &AUDITORS) {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), "reprint_receipt").await;
        return Err(ApiError::Forbidden);
    }
    sqlx::query("INSERT INTO receipt_prints (sale_id, printed_by) VALUES ($1,$2)").bind(id).bind(p.user_id).execute(&mut *tx).await?;
    if prior > 0 {
        audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "reprint_receipt", entity_type: "sale", entity_id: Some(id.to_string()), after: Some(json!({ "print_number": prior + 1 })), ..Default::default() }).await?;
    }
    let mut r = receipt_json(&mut tx, id, store).await?;
    r["reprint"] = json!(prior > 0);
    r["copy_label"] = json!(if prior > 0 { format!("REPRINT #{prior}") } else { "ORIGINAL".to_string() });
    tx.commit().await?;
    Ok(Json(r))
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
    /// Dates are in the business's timezone and inclusive.
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    cashier_id: Option<Uuid>,
    /// A receipt number, transaction number or sale id.
    transaction: Option<String>,
    payment_status: Option<String>,
}

pub async fn list_sales(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &VIEW, "list_sales").await?;
    if q.payment_status.as_deref().is_some_and(|s| !["paid", "pending", "unpaid", "refunded"].contains(&s)) {
        return Err(ApiError::bad("payment_status must be paid, pending, unpaid or refunded"));
    }
    let all = p.has_role_at(store, &AUDITORS);
    let tz = "(SELECT timezone FROM business_settings WHERE business_id = sa.business_id)";
    let rows = sqlx::query(&format!(
        "SELECT * FROM (
            SELECT sa.id, sa.receipt_number, sa.transaction_number, sa.total_minor, sa.cashier_id, sa.created_at, {PAYMENT_STATUS} AS payment_status
            FROM sales sa
            WHERE sa.store_id = $1 AND ($2 OR sa.cashier_id = $3)
              AND ($4::date IS NULL OR sa.created_at >= ($4::date)::timestamp AT TIME ZONE {tz})
              AND ($5::date IS NULL OR sa.created_at < (($5::date) + 1)::timestamp AT TIME ZONE {tz})
              AND ($6::uuid IS NULL OR sa.cashier_id = $6)
              AND ($7::text IS NULL OR sa.receipt_number = $7 OR sa.transaction_number = $7 OR sa.id::text = $7)
         ) x WHERE ($8::text IS NULL OR x.payment_status = $8) ORDER BY x.created_at DESC, x.id LIMIT $9"
    ))
    .bind(store)
    .bind(all)
    .bind(p.user_id)
    .bind(q.from)
    .bind(q.to)
    .bind(q.cashier_id)
    .bind(q.transaction.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(q.payment_status)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid, _>("id"), "receipt_number": r.get::<String, _>("receipt_number"), "transaction_number": r.get::<String, _>("transaction_number"),
        "total_minor": r.get::<i64, _>("total_minor"), "cashier_id": r.get::<Uuid, _>("cashier_id"), "payment_status": r.get::<String, _>("payment_status"),
        "created_at": r.get::<DateTime<Utc>, _>("created_at"),
    })).collect();
    Ok(Json(json!({ "items": items })))
}

// ---------------------------------------------------------------- payments after the sale (FR-16)

/// A sale the caller may take payments on: its own cashier, or a manager.
async fn payable_sale(state: &AppState, p: &Principal, id: Uuid, action: &str) -> ApiResult<Uuid> {
    let row = sqlx::query("SELECT store_id, cashier_id FROM sales WHERE id = $1 AND business_id = $2").bind(id).bind(p.business_id).fetch_optional(&state.pool).await?.ok_or(ApiError::NotFound("sale"))?;
    let (store, cashier): (Uuid, Uuid) = (row.get("store_id"), row.get("cashier_id"));
    p.authorize_store(state, store, &SELL, action).await?;
    if cashier != p.user_id && !p.has_role_at(store, &[Role::StoreManager]) {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), action).await;
        return Err(ApiError::Forbidden);
    }
    Ok(store)
}

/// Adds a payment to a sale that is not fully paid. A payment that repeats an earlier external reference
/// returns the original instead of being counted twice (NFR-10).
pub async fn add_payment(State(state): State<AppState>, p: Principal, Path(id): Path<Uuid>, Json(b): Json<PaymentIn>) -> ApiResult<(StatusCode, Json<Value>)> {
    let store = payable_sale(&state, &p, id, "add_payment").await?;
    let status = payment_status_for(&b)?;
    let reference = b.reference.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(format!("pay:{id}")).execute(&mut *tx).await?;
    if let Some(r) = reference {
        if let Some(row) = sqlx::query("SELECT id, amount_minor, status FROM sale_payments WHERE sale_id = $1 AND method = $2 AND reference = $3").bind(id).bind(&b.method).bind(r).fetch_optional(&mut *tx).await? {
            if row.get::<i64, _>("amount_minor") != b.amount_minor {
                return Err(ApiError::conflict("that payment reference was already used with a different amount"));
            }
            return Ok((StatusCode::OK, Json(json!({ "payment_id": row.get::<Uuid, _>("id"), "status": row.get::<String, _>("status"), "duplicate": true }))));
        }
    }
    let due = sqlx::query(
        "SELECT (sa.total_minor - (COALESCE((SELECT sum(amount_minor) FROM sale_payments WHERE sale_id = sa.id AND status = 'successful'), 0) - sa.change_minor)
                - COALESCE((SELECT sum(amount_minor) FROM sale_payments WHERE sale_id = sa.id AND status = 'pending'), 0))::bigint AS due
         FROM sales sa WHERE sa.id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    .get::<i64, _>("due");
    if due <= 0 {
        return Err(ApiError::conflict("this sale has nothing left to pay"));
    }
    if b.amount_minor > due {
        return Err(ApiError::bad(format!("the balance still to pay is {due}")));
    }
    let pid: Uuid = sqlx::query_scalar("INSERT INTO sale_payments (sale_id, method, amount_minor, reference, status) VALUES ($1,$2,$3,$4,$5) RETURNING id")
        .bind(id).bind(&b.method).bind(b.amount_minor).bind(reference).bind(status)
        .fetch_one(&mut *tx)
        .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "add_payment", entity_type: "sale", entity_id: Some(id.to_string()), after: Some(json!({ "payment_id": pid, "amount_minor": b.amount_minor, "status": status })), ..Default::default() }).await?;
    let sale = sale_json(&mut tx, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "payment_id": pid, "status": status, "payment_status": sale["payment_status"] }))))
}

async fn move_payment(state: &AppState, p: &Principal, sale: Uuid, payment: Uuid, from: &str, to: &str, action: &str, manager_only: bool) -> ApiResult<Json<Value>> {
    let store = payable_sale(state, p, sale, action).await?;
    if manager_only && !p.has_role_at(store, &[Role::StoreManager]) {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), action).await;
        return Err(ApiError::Forbidden);
    }
    let mut tx = state.pool.begin().await?;
    let n = sqlx::query("UPDATE sale_payments SET status = $4 WHERE id = $1 AND sale_id = $2 AND status = $3").bind(payment).bind(sale).bind(from).bind(to).execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::conflict(format!("only a {from} payment can be moved to {to}")));
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action, entity_type: "sale", entity_id: Some(sale.to_string()), after: Some(json!({ "payment_id": payment, "status": to })), ..Default::default() }).await?;
    let out = sale_json(&mut tx, sale).await?;
    tx.commit().await?;
    Ok(Json(json!({ "payment_id": payment, "status": to, "payment_status": out["payment_status"] })))
}

pub async fn confirm_payment(State(state): State<AppState>, p: Principal, Path((id, pid)): Path<(Uuid, Uuid)>) -> ApiResult<Json<Value>> {
    move_payment(&state, &p, id, pid, "pending", "successful", "confirm_payment", false).await
}

pub async fn fail_payment(State(state): State<AppState>, p: Principal, Path((id, pid)): Path<(Uuid, Uuid)>) -> ApiResult<Json<Value>> {
    move_payment(&state, &p, id, pid, "pending", "failed", "fail_payment", false).await
}

/// Undoes a payment that had succeeded (a chargeback, a bank reversal). Managers only.
pub async fn reverse_payment(State(state): State<AppState>, p: Principal, Path((id, pid)): Path<(Uuid, Uuid)>) -> ApiResult<Json<Value>> {
    move_payment(&state, &p, id, pid, "successful", "reversed", "reverse_payment", true).await
}

// ---------------------------------------------------------------- reconciliation (FR-16)

#[derive(Deserialize)]
pub struct ReconQuery {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

/// Compares what was sold with what was collected, by payment method, for a store and period. The report
/// is `reconciled` only when every sale is fully paid and no payment is still pending.
pub async fn reconciliation(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<ReconQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &AUDITORS, "view_reconciliation").await?;
    let tz = "(SELECT timezone FROM business_settings WHERE business_id = sa.business_id)";
    let scope = format!(
        "sa.store_id = $1 AND ($2::date IS NULL OR sa.created_at >= ($2::date)::timestamp AT TIME ZONE {tz}) AND ($3::date IS NULL OR sa.created_at < (($3::date) + 1)::timestamp AT TIME ZONE {tz})"
    );
    let head = sqlx::query(&format!("SELECT count(*) AS n, COALESCE(sum(sa.total_minor), 0)::bigint AS total, COALESCE(sum(sa.change_minor), 0)::bigint AS change FROM sales sa WHERE {scope}"))
        .bind(store).bind(q.from).bind(q.to).fetch_one(&state.pool).await?;
    let rows = sqlx::query(&format!("SELECT pp.method, pp.status, sum(pp.amount_minor)::bigint AS amount FROM sale_payments pp JOIN sales sa ON sa.id = pp.sale_id WHERE {scope} GROUP BY 1, 2 ORDER BY 1, 2"))
        .bind(store).bind(q.from).bind(q.to).fetch_all(&state.pool).await?;
    let refunds: i64 = sqlx::query_scalar(&format!("SELECT COALESCE(sum(r.refund_minor), 0)::bigint FROM returns r JOIN sales sa ON sa.id = r.sale_id WHERE {scope}"))
        .bind(store).bind(q.from).bind(q.to).fetch_one(&state.pool).await?;
    let (sales_total, change): (i64, i64) = (head.get("total"), head.get("change"));
    let mut by_method = serde_json::Map::new();
    let (mut collected, mut pending) = (0i64, 0i64);
    for r in &rows {
        let (method, status, amount): (String, String, i64) = (r.get("method"), r.get("status"), r.get("amount"));
        if status == "successful" || status == "refunded" {
            collected += amount;
        }
        if status == "pending" {
            pending += amount;
        }
        by_method.entry(method).or_insert_with(|| json!({})).as_object_mut().unwrap().insert(status, json!(amount));
    }
    collected -= change; // change handed back never counted as takings
    let unpaid = sales_total - collected - pending;
    Ok(Json(json!({
        "sales_count": head.get::<i64, _>("n"), "sales_total_minor": sales_total, "change_given_minor": change,
        "collected_minor": collected, "pending_minor": pending, "unpaid_minor": unpaid, "refunds_issued_minor": refunds,
        "net_collected_minor": collected - refunds, "by_method": by_method, "reconciled": unpaid == 0 && pending == 0,
    })))
}

// ---------------------------------------------------------------- returns and refunds (FR-18)

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
    let r = sqlx::query("SELECT id, sale_id, store_id, return_number, reason, refund_method, refund_minor, refund_status, approved_by, created_by, created_at FROM returns WHERE id = $1").bind(id).fetch_one(&mut *db).await?;
    let lines = sqlx::query("SELECT sale_line_id, quantity, refund_minor, disposition FROM return_lines WHERE return_id = $1 ORDER BY id").bind(id).fetch_all(&mut *db).await?;
    let lines: Vec<Value> = lines.iter().map(|l| json!({ "sale_line_id": l.get::<Uuid, _>("sale_line_id"), "quantity": l.get::<i64, _>("quantity"), "refund_minor": l.get::<i64, _>("refund_minor"), "disposition": l.get::<String, _>("disposition") })).collect();
    Ok(json!({
        "id": r.get::<Uuid, _>("id"), "sale_id": r.get::<Uuid, _>("sale_id"), "store_id": r.get::<Uuid, _>("store_id"), "return_number": r.get::<String, _>("return_number"),
        "reason": r.get::<String, _>("reason"), "refund_method": r.get::<String, _>("refund_method"), "refund_minor": r.get::<i64, _>("refund_minor"), "refund_status": r.get::<String, _>("refund_status"),
        "approved_by": r.get::<Option<Uuid>, _>("approved_by"), "created_by": r.get::<Uuid, _>("created_by"), "created_at": r.get::<DateTime<Utc>, _>("created_at"), "lines": lines,
    }))
}

pub async fn create_return(State(state): State<AppState>, p: Principal, Path(sale_id): Path<Uuid>, headers: HeaderMap, Json(b): Json<ReturnIn>) -> ApiResult<(StatusCode, Json<Value>)> {
    let sale = sqlx::query("SELECT store_id FROM sales WHERE id = $1 AND business_id = $2").bind(sale_id).bind(p.business_id).fetch_optional(&state.pool).await?.ok_or(ApiError::NotFound("sale"))?;
    let store: Uuid = sale.get("store_id");
    p.authorize_store(&state, store, &SELL, "create_return").await?;
    p.require_active_store(&state, store).await?;
    let key = idempotency_key(&headers)?;
    if b.lines.is_empty() || b.lines.len() > 100 || b.lines.iter().any(|l| l.quantity <= 0) {
        return Err(ApiError::bad("a return needs 1 to 100 lines with positive quantities"));
    }
    if b.lines.iter().any(|l| l.disposition != "restock" && l.disposition != "damaged") {
        return Err(ApiError::bad("disposition must be restock or damaged"));
    }
    if b.reason.trim().is_empty() || !METHODS.contains(&b.refund_method.as_str()) {
        return Err(ApiError::bad("a reason and a refund method (cash, card, transfer or other) are required"));
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
    let mut non_returnable = false;
    let mut done: Vec<(Uuid, i64, i64, String)> = Vec::new(); // (sale line, qty, refund, disposition)
    for l in &b.lines {
        let line = sqlx::query(
            "SELECT sl.variant_id, sl.quantity, sl.returned_qty, sl.line_total_minor, pr.returnable FROM sale_lines sl
             JOIN product_variants v ON v.id = sl.variant_id JOIN products pr ON pr.id = v.product_id WHERE sl.id = $1 AND sl.sale_id = $2 FOR UPDATE OF sl",
        )
        .bind(l.sale_line_id)
        .bind(sale_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound("sale line"))?;
        let (variant, qty, before, total): (Uuid, i64, i64, i64) = (line.get("variant_id"), line.get("quantity"), line.get("returned_qty"), line.get("line_total_minor"));
        non_returnable |= !line.get::<bool, _>("returnable");
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

    // Late returns, big refunds and non-returnable goods need a store manager (or a business-wide role),
    // and the approver is recorded on the refund.
    let late: bool = sqlx::query_scalar("SELECT created_at < now() - make_interval(days => $2::int) FROM sales WHERE id = $1").bind(sale_id).bind(state.config.return_window_days as i32).fetch_one(&mut *tx).await?;
    let needs_manager = late || non_returnable || refund_total > state.config.return_manager_limit_minor;
    if needs_manager && !p.has_role_at(store, &[Role::StoreManager]) {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), "return_needs_manager").await;
        return Err(ApiError::Forbidden);
    }

    let n: i64 = sqlx::query_scalar("UPDATE store_settings SET next_return_number = next_return_number + 1 WHERE store_id = $1 RETURNING next_return_number - 1").bind(store).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO returns (id, business_id, store_id, sale_id, return_number, reason, refund_method, refund_minor, created_by, idempotency_key, request_hash, approved_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
        .bind(return_id).bind(p.business_id).bind(store).bind(sale_id).bind(format!("RTN-{n:06}")).bind(b.reason.trim()).bind(&b.refund_method).bind(refund_total).bind(p.user_id).bind(&key).bind(&hash)
        .bind(if needs_manager { Some(p.user_id) } else { None })
        .execute(&mut *tx)
        .await?;
    for (line, qty, refund, disposition) in &done {
        sqlx::query("INSERT INTO return_lines (return_id, sale_line_id, quantity, refund_minor, disposition) VALUES ($1,$2,$3,$4,$5)").bind(return_id).bind(line).bind(qty).bind(refund).bind(disposition).execute(&mut *tx).await?;
    }
    // Once everything on the sale has come back, its successful payments are marked refunded.
    sqlx::query("UPDATE sale_payments SET status = 'refunded' WHERE sale_id = $1 AND status = 'successful' AND NOT EXISTS (SELECT 1 FROM sale_lines l WHERE l.sale_id = $1 AND l.returned_qty < l.quantity)")
        .bind(sale_id)
        .execute(&mut *tx)
        .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "create_return", entity_type: "return", entity_id: Some(return_id.to_string()), after: Some(json!({ "sale_id": sale_id, "refund_minor": refund_total, "reason": b.reason.trim(), "approved_by_manager": needs_manager })), ..Default::default() }).await?;
    let out = return_json(&mut tx, return_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}
