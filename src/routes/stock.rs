//! Store stock: balances, receipts, adjustments with approval, counts, expiry and low-stock alerts
//! (FR-10 to FR-14). Every change goes through `crate::inventory`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgConnection, Row};
use uuid::Uuid;

use crate::audit::{self, Entry};
use crate::error::{ApiError, ApiResult};
use crate::inventory::{self, apply, consume, Bucket, Move};
use crate::permissions::{Principal, Role};
use crate::state::AppState;

const READ: [Role; 4] = [Role::StoreManager, Role::Cashier, Role::InventoryOfficer, Role::FinanceOfficer];
const STAFF: [Role; 2] = [Role::StoreManager, Role::InventoryOfficer];
const LEDGER: [Role; 3] = [Role::StoreManager, Role::InventoryOfficer, Role::FinanceOfficer];
const APPROVE: [Role; 1] = [Role::StoreManager];

const TODAY: &str = "(now() AT TIME ZONE (SELECT timezone FROM business_settings WHERE business_id = $1))::date";

// ---------------------------------------------------------------- balances (FR-10)

#[derive(Deserialize)]
pub struct BalanceQuery {
    low: Option<bool>,
}

pub async fn balances(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<BalanceQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &READ, "view_stock").await?;
    let rows = sqlx::query(
        "SELECT b.variant_id, v.sku, pr.name AS product_name, b.on_hand, b.damaged, b.expired, b.reorder_level,
                (b.reorder_level IS NOT NULL AND b.on_hand <= b.reorder_level) AS low
         FROM stock_balances b JOIN product_variants v ON v.id = b.variant_id JOIN products pr ON pr.id = v.product_id
         WHERE b.store_id = $1 AND ($2::bool IS NOT TRUE OR (b.reorder_level IS NOT NULL AND b.on_hand <= b.reorder_level))
         ORDER BY lower(pr.name), v.sku",
    )
    .bind(store)
    .bind(q.low)
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| json!({
            "variant_id": r.get::<Uuid, _>("variant_id"), "sku": r.get::<String, _>("sku"), "product_name": r.get::<String, _>("product_name"),
            "on_hand": r.get::<i64, _>("on_hand"), "damaged": r.get::<i64, _>("damaged"), "expired": r.get::<i64, _>("expired"),
            "reorder_level": r.get::<Option<i64>, _>("reorder_level"), "low": r.get::<bool, _>("low"),
        }))
        .collect();
    Ok(Json(json!({ "items": items })))
}

pub async fn balance(State(state): State<AppState>, p: Principal, Path((store, variant)): Path<(Uuid, Uuid)>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &READ, "view_stock").await?;
    let mut conn = state.pool.acquire().await?;
    inventory::tracks_expiry(&mut conn, p.business_id, variant).await?;
    let b = sqlx::query("SELECT on_hand, damaged, expired, reorder_level FROM stock_balances WHERE store_id = $1 AND variant_id = $2").bind(store).bind(variant).fetch_optional(&mut *conn).await?;
    let batches = sqlx::query("SELECT id, batch_code, expiry_date, quantity FROM stock_batches WHERE store_id = $1 AND variant_id = $2 AND quantity > 0 ORDER BY expiry_date NULLS LAST, received_at")
        .bind(store)
        .bind(variant)
        .fetch_all(&mut *conn)
        .await?;
    let batches: Vec<Value> = batches
        .iter()
        .map(|r| json!({ "id": r.get::<Uuid, _>("id"), "batch_code": r.get::<String, _>("batch_code"), "expiry_date": r.get::<Option<NaiveDate>, _>("expiry_date"), "quantity": r.get::<i64, _>("quantity") }))
        .collect();
    let num = |c: &str| b.as_ref().map(|r| r.get::<i64, _>(c)).unwrap_or(0);
    Ok(Json(json!({
        "variant_id": variant, "on_hand": num("on_hand"), "damaged": num("damaged"), "expired": num("expired"),
        "reorder_level": b.as_ref().and_then(|r| r.get::<Option<i64>, _>("reorder_level")), "batches": batches,
    })))
}

// ---------------------------------------------------------------- receiving stock

#[derive(Deserialize)]
pub struct ReceiptLine {
    variant_id: Uuid,
    quantity: i64,
    batch_code: Option<String>,
    expiry_date: Option<NaiveDate>,
    unit_cost_minor: Option<i64>,
}

#[derive(Deserialize)]
pub struct Receipt {
    lines: Vec<ReceiptLine>,
    reference: Option<String>,
}

pub async fn receive(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Json(b): Json<Receipt>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.authorize_store(&state, store, &STAFF, "receive_stock").await?;
    if b.lines.is_empty() || b.lines.len() > 200 {
        return Err(ApiError::bad("a receipt needs between 1 and 200 lines"));
    }
    let group = Uuid::new_v4();
    let reference = b.reference.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| group.to_string());
    let mut tx = state.pool.begin().await?;
    let mut out = Vec::new();
    for l in &b.lines {
        if l.quantity <= 0 || l.unit_cost_minor.is_some_and(|c| c < 0) {
            return Err(ApiError::bad("quantities must be positive and costs cannot be negative"));
        }
        let tracks = inventory::tracks_expiry(&mut tx, p.business_id, l.variant_id).await?;
        let batch = if tracks {
            let code = l.batch_code.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| ApiError::bad("this product needs a batch code"))?;
            let expiry = l.expiry_date.ok_or_else(|| ApiError::bad("this product needs an expiry date"))?;
            let fresh: bool = sqlx::query_scalar(&format!("SELECT $2::date >= {TODAY}")).bind(p.business_id).bind(expiry).fetch_one(&mut *tx).await?;
            if !fresh {
                return Err(ApiError::bad("cannot receive stock that has already expired"));
            }
            let existing = sqlx::query("SELECT id, expiry_date FROM stock_batches WHERE store_id = $1 AND variant_id = $2 AND batch_code = $3 FOR UPDATE").bind(store).bind(l.variant_id).bind(code).fetch_optional(&mut *tx).await?;
            let id: Uuid = match existing {
                Some(r) if r.get::<Option<NaiveDate>, _>("expiry_date") != Some(expiry) => return Err(ApiError::conflict("that batch code already exists with a different expiry date")),
                Some(r) => r.get("id"),
                None => sqlx::query_scalar("INSERT INTO stock_batches (business_id, store_id, variant_id, batch_code, expiry_date, unit_cost_minor) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id")
                    .bind(p.business_id).bind(store).bind(l.variant_id).bind(code).bind(expiry).bind(l.unit_cost_minor).fetch_one(&mut *tx).await?,
            };
            Some(id)
        } else {
            if l.batch_code.is_some() || l.expiry_date.is_some() {
                return Err(ApiError::bad("this product does not use batches or expiry dates"));
            }
            None
        };
        let on_hand = apply(&mut tx, &Move { business_id: p.business_id, store_id: store, variant_id: l.variant_id, batch_id: batch, bucket: Bucket::OnHand, delta: l.quantity, kind: "receive", group_id: group, reason: None, reference: Some(("receipt", reference.clone())), actor: p.user_id }).await?;
        out.push(json!({ "variant_id": l.variant_id, "batch_id": batch, "received": l.quantity, "on_hand": on_hand }));
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "receive_stock", entity_type: "stock_receipt", entity_id: Some(reference), after: Some(json!(out)), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "lines": out }))))
}

// ---------------------------------------------------------------- adjustments (FR-12)

#[derive(Serialize, FromRow)]
pub struct Adjustment {
    id: Uuid,
    store_id: Uuid,
    variant_id: Uuid,
    batch_id: Option<Uuid>,
    kind: String,
    quantity: i64,
    reason: String,
    source: String,
    status: String,
    requested_by: Uuid,
    decided_by: Option<Uuid>,
    decided_at: Option<DateTime<Utc>>,
    decision_note: Option<String>,
    auto_approved: bool,
    created_at: DateTime<Utc>,
}

const ADJ_COLS: &str = "id, store_id, variant_id, batch_id, kind, quantity, reason, source, status, requested_by, decided_by, decided_at, decision_note, auto_approved, created_at";

struct AdjInput {
    variant_id: Uuid,
    batch_id: Option<Uuid>,
    kind: String,
    quantity: i64,
    reason: String,
    source: &'static str,
}

/// Carry out an approved adjustment through the movement service.
async fn apply_adjustment(db: &mut PgConnection, business_id: Uuid, actor: Uuid, a: &Adjustment) -> ApiResult<()> {
    let group = Uuid::new_v4();
    let mv = |bucket: Bucket, batch: Option<Uuid>, delta: i64, kind: &'static str| Move {
        business_id, store_id: a.store_id, variant_id: a.variant_id, batch_id: batch, bucket, delta, kind, group_id: group,
        reason: Some(a.reason.as_str()), reference: Some(("adjustment", a.id.to_string())), actor,
    };
    match a.kind.as_str() {
        "increase" => {
            apply(db, &mv(Bucket::OnHand, a.batch_id, a.quantity, "adjust_in")).await?;
        }
        "decrease" | "damage" => {
            let kind = if a.kind == "damage" { "damage" } else { "adjust_out" };
            match a.batch_id {
                Some(batch) => {
                    apply(db, &mv(Bucket::OnHand, Some(batch), -a.quantity, kind)).await?;
                }
                None => {
                    consume(db, &mv(Bucket::OnHand, None, 0, kind), a.quantity).await?;
                }
            }
            if a.kind == "damage" {
                apply(db, &mv(Bucket::Damaged, a.batch_id, a.quantity, "damage")).await?;
            }
        }
        _ => return Err(ApiError::bad("unknown adjustment kind")),
    }
    Ok(())
}

/// Record an adjustment. Up to the store's threshold it is applied straight away; anything bigger waits
/// as `pending` until a different person approves it (separation of duties).
async fn submit_adjustment(db: &mut PgConnection, p: &Principal, store: Uuid, a: AdjInput) -> ApiResult<Adjustment> {
    if !["increase", "decrease", "damage"].contains(&a.kind.as_str()) {
        return Err(ApiError::bad("kind must be increase, decrease or damage"));
    }
    if a.quantity <= 0 || a.reason.trim().is_empty() {
        return Err(ApiError::bad("a positive quantity and a reason are required"));
    }
    let tracks = inventory::tracks_expiry(db, p.business_id, a.variant_id).await?;
    match (tracks, a.batch_id, a.kind.as_str()) {
        (false, Some(_), _) => return Err(ApiError::bad("this product has no batches")),
        (true, None, "increase") => return Err(ApiError::bad("say which batch to add to")),
        _ => {}
    }
    if let Some(batch) = a.batch_id {
        let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM stock_batches WHERE id = $1 AND store_id = $2 AND variant_id = $3)").bind(batch).bind(store).bind(a.variant_id).fetch_one(&mut *db).await?;
        if !ok {
            return Err(ApiError::NotFound("batch"));
        }
    }
    let threshold: i64 = sqlx::query_scalar("SELECT COALESCE((SELECT adjustment_approval_threshold FROM store_stock_settings WHERE store_id = $1), 50)").bind(store).fetch_one(&mut *db).await?;
    let needs_approval = a.quantity > threshold;
    let adj = sqlx::query_as::<_, Adjustment>(&format!(
        "INSERT INTO stock_adjustments (business_id, store_id, variant_id, batch_id, kind, quantity, reason, source, status, requested_by, auto_approved)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING {ADJ_COLS}"
    ))
    .bind(p.business_id)
    .bind(store)
    .bind(a.variant_id)
    .bind(a.batch_id)
    .bind(&a.kind)
    .bind(a.quantity)
    .bind(a.reason.trim())
    .bind(a.source)
    .bind(if needs_approval { "pending" } else { "approved" })
    .bind(p.user_id)
    .bind(!needs_approval)
    .fetch_one(&mut *db)
    .await?;
    if !needs_approval {
        apply_adjustment(db, p.business_id, p.user_id, &adj).await?;
    }
    let action = if needs_approval { "request_stock_adjustment" } else { "stock_adjustment" };
    audit::record(&mut *db, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action, entity_type: "stock_adjustment", entity_id: Some(adj.id.to_string()), after: audit::json(&adj), ..Default::default() }).await?;
    Ok(adj)
}

#[derive(Deserialize)]
pub struct AdjustBody {
    variant_id: Uuid,
    batch_id: Option<Uuid>,
    kind: String,
    quantity: i64,
    reason: String,
}

pub async fn adjust(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Json(b): Json<AdjustBody>) -> ApiResult<(StatusCode, Json<Adjustment>)> {
    p.authorize_store(&state, store, &STAFF, "adjust_stock").await?;
    let mut tx = state.pool.begin().await?;
    let adj = submit_adjustment(&mut tx, &p, store, AdjInput { variant_id: b.variant_id, batch_id: b.batch_id, kind: b.kind, quantity: b.quantity, reason: b.reason, source: "manual" }).await?;
    tx.commit().await?;
    // 201 when applied now, 202 when it is waiting for approval.
    let status = if adj.status == "pending" { StatusCode::ACCEPTED } else { StatusCode::CREATED };
    Ok((status, Json(adj)))
}

#[derive(Deserialize)]
pub struct AdjustQuery {
    status: Option<String>,
}

pub async fn list_adjustments(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<AdjustQuery>) -> ApiResult<Json<Vec<Adjustment>>> {
    p.authorize_store(&state, store, &LEDGER, "view_adjustments").await?;
    Ok(Json(
        sqlx::query_as::<_, Adjustment>(&format!("SELECT {ADJ_COLS} FROM stock_adjustments WHERE store_id = $1 AND ($2::text IS NULL OR status = $2) ORDER BY created_at DESC LIMIT 200"))
            .bind(store)
            .bind(q.status)
            .fetch_all(&state.pool)
            .await?,
    ))
}

#[derive(Deserialize, Default)]
pub struct Decision {
    note: Option<String>,
}

async fn decide(state: &AppState, p: &Principal, store: Uuid, id: Uuid, approve: bool, note: Option<String>) -> ApiResult<Adjustment> {
    p.authorize_store(state, store, &APPROVE, if approve { "approve_adjustment" } else { "reject_adjustment" }).await?;
    let mut tx = state.pool.begin().await?;
    let adj = sqlx::query_as::<_, Adjustment>(&format!("SELECT {ADJ_COLS} FROM stock_adjustments WHERE id = $1 AND store_id = $2 FOR UPDATE"))
        .bind(id)
        .bind(store)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound("adjustment"))?;
    if adj.status != "pending" {
        return Err(ApiError::conflict("this adjustment has already been decided"));
    }
    if adj.requested_by == p.user_id {
        audit::denied(&state.pool, p.business_id, p.user_id, Some(store), "decide_own_adjustment").await;
        return Err(ApiError::Forbidden);
    }
    if approve {
        apply_adjustment(&mut tx, p.business_id, p.user_id, &adj).await?;
    }
    let done = sqlx::query_as::<_, Adjustment>(&format!(
        "UPDATE stock_adjustments SET status = $2, decided_by = $3, decided_at = now(), decision_note = $4 WHERE id = $1 RETURNING {ADJ_COLS}"
    ))
    .bind(id)
    .bind(if approve { "approved" } else { "rejected" })
    .bind(p.user_id)
    .bind(note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()))
    .fetch_one(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: if approve { "approve_adjustment" } else { "reject_adjustment" }, entity_type: "stock_adjustment", entity_id: Some(id.to_string()), before: audit::json(&adj), after: audit::json(&done), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(done)
}

pub async fn approve(State(state): State<AppState>, p: Principal, Path((store, id)): Path<(Uuid, Uuid)>, body: Option<Json<Decision>>) -> ApiResult<Json<Adjustment>> {
    Ok(Json(decide(&state, &p, store, id, true, body.and_then(|b| b.0.note)).await?))
}

pub async fn reject(State(state): State<AppState>, p: Principal, Path((store, id)): Path<(Uuid, Uuid)>, body: Option<Json<Decision>>) -> ApiResult<Json<Adjustment>> {
    Ok(Json(decide(&state, &p, store, id, false, body.and_then(|b| b.0.note)).await?))
}

// ---------------------------------------------------------------- stock counts (FR-12)

#[derive(Deserialize)]
pub struct CountLine {
    variant_id: Uuid,
    batch_id: Option<Uuid>,
    counted: i64,
}

#[derive(Deserialize)]
pub struct CountBody {
    lines: Vec<CountLine>,
}

/// A count compares what was found with what the system expected. Small differences are corrected at
/// once; large ones become pending adjustments for approval. The system figure is read under a row lock,
/// so a sale during the count cannot make the correction wrong.
pub async fn count(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Json(b): Json<CountBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    p.authorize_store(&state, store, &STAFF, "count_stock").await?;
    if b.lines.is_empty() || b.lines.len() > 500 || b.lines.iter().any(|l| l.counted < 0) {
        return Err(ApiError::bad("a count needs 1 to 500 lines with non-negative quantities"));
    }
    let mut seen = std::collections::HashSet::new();
    if !b.lines.iter().all(|l| seen.insert((l.variant_id, l.batch_id))) {
        return Err(ApiError::bad("each variant (and batch) can appear once in a count"));
    }
    let mut tx = state.pool.begin().await?;
    let count_id: Uuid = sqlx::query_scalar("INSERT INTO stock_counts (business_id, store_id, created_by) VALUES ($1,$2,$3) RETURNING id").bind(p.business_id).bind(store).bind(p.user_id).fetch_one(&mut *tx).await?;
    let mut out = Vec::new();
    for l in &b.lines {
        let tracks = inventory::tracks_expiry(&mut tx, p.business_id, l.variant_id).await?;
        let system: i64 = if tracks {
            let batch = l.batch_id.ok_or_else(|| ApiError::bad("count batched products batch by batch"))?;
            sqlx::query_scalar("SELECT quantity FROM stock_batches WHERE id = $1 AND store_id = $2 AND variant_id = $3 FOR UPDATE").bind(batch).bind(store).bind(l.variant_id).fetch_optional(&mut *tx).await?.ok_or(ApiError::NotFound("batch"))?
        } else {
            if l.batch_id.is_some() {
                return Err(ApiError::bad("this product has no batches"));
            }
            sqlx::query("INSERT INTO stock_balances (store_id, variant_id, business_id) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING").bind(store).bind(l.variant_id).bind(p.business_id).execute(&mut *tx).await?;
            sqlx::query_scalar("SELECT on_hand FROM stock_balances WHERE store_id = $1 AND variant_id = $2 FOR UPDATE").bind(store).bind(l.variant_id).fetch_one(&mut *tx).await?
        };
        let variance = l.counted - system;
        let adj = if variance == 0 {
            None
        } else {
            let kind = if variance > 0 { "increase" } else { "decrease" };
            Some(submit_adjustment(&mut tx, &p, store, AdjInput { variant_id: l.variant_id, batch_id: l.batch_id, kind: kind.into(), quantity: variance.abs(), reason: format!("stock count {count_id}"), source: "count" }).await?)
        };
        sqlx::query("INSERT INTO stock_count_lines (count_id, variant_id, batch_id, system_qty, counted_qty, variance, adjustment_id) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(count_id).bind(l.variant_id).bind(l.batch_id).bind(system).bind(l.counted).bind(variance).bind(adj.as_ref().map(|a| a.id))
            .execute(&mut *tx).await?;
        out.push(json!({ "variant_id": l.variant_id, "batch_id": l.batch_id, "system": system, "counted": l.counted, "variance": variance, "adjustment_status": adj.as_ref().map(|a| a.status.clone()), "adjustment_id": adj.as_ref().map(|a| a.id) }));
    }
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "stock_count", entity_type: "stock_count", entity_id: Some(count_id.to_string()), after: Some(json!(out)), ..Default::default() }).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": count_id, "lines": out }))))
}

// ---------------------------------------------------------------- expiry (FR-13)

#[derive(Deserialize)]
pub struct ExpiringQuery {
    days: Option<i32>,
}

pub async fn expiring(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<ExpiringQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &STAFF, "view_expiring_stock").await?;
    let rows = sqlx::query(&format!(
        "SELECT b.id, b.variant_id, v.sku, b.batch_code, b.expiry_date, b.quantity, (b.expiry_date < {TODAY}) AS expired
         FROM stock_batches b JOIN product_variants v ON v.id = b.variant_id
         WHERE b.store_id = $2 AND b.quantity > 0 AND b.expiry_date IS NOT NULL AND b.expiry_date <= {TODAY} + $3::int
         ORDER BY b.expiry_date, b.batch_code"
    ))
    .bind(p.business_id)
    .bind(store)
    .bind(q.days.unwrap_or(30).clamp(0, 3650))
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows.iter().map(|r| json!({
        "batch_id": r.get::<Uuid, _>("id"), "variant_id": r.get::<Uuid, _>("variant_id"), "sku": r.get::<String, _>("sku"), "batch_code": r.get::<String, _>("batch_code"),
        "expiry_date": r.get::<Option<NaiveDate>, _>("expiry_date"), "quantity": r.get::<i64, _>("quantity"), "expired": r.get::<bool, _>("expired"),
    })).collect();
    Ok(Json(json!({ "items": items })))
}

/// Moves whatever is left of expired batches out of sellable stock into the expired bucket. Safe to run
/// repeatedly (for instance from a nightly job): a batch already moved has nothing left to move.
pub async fn expire_sweep(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &STAFF, "expire_stock").await?;
    let mut tx = state.pool.begin().await?;
    let rows = sqlx::query(&format!(
        "SELECT id, variant_id, quantity FROM stock_batches WHERE store_id = $2 AND quantity > 0 AND expiry_date < {TODAY} ORDER BY expiry_date, id FOR UPDATE"
    ))
    .bind(p.business_id)
    .bind(store)
    .fetch_all(&mut *tx)
    .await?;
    let mut moved = Vec::new();
    for r in rows {
        let (batch, variant, qty): (Uuid, Uuid, i64) = (r.get("id"), r.get("variant_id"), r.get("quantity"));
        let group = Uuid::new_v4();
        let mv = |bucket: Bucket, delta: i64| Move { business_id: p.business_id, store_id: store, variant_id: variant, batch_id: Some(batch), bucket, delta, kind: "expire", group_id: group, reason: Some("batch expired"), reference: Some(("batch", batch.to_string())), actor: p.user_id };
        apply(&mut tx, &mv(Bucket::OnHand, -qty)).await?;
        apply(&mut tx, &mv(Bucket::Expired, qty)).await?;
        moved.push(json!({ "batch_id": batch, "variant_id": variant, "quantity": qty }));
    }
    if !moved.is_empty() {
        audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "expire_stock", entity_type: "stock_batch", after: Some(json!(moved)), ..Default::default() }).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({ "moved": moved })))
}

// ---------------------------------------------------------------- ledger, alerts, settings

#[derive(Deserialize)]
pub struct MovementQuery {
    variant_id: Option<Uuid>,
    limit: Option<i64>,
}

pub async fn movements(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<MovementQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &LEDGER, "view_stock_movements").await?;
    let rows = sqlx::query(
        "SELECT id, variant_id, batch_id, group_id, kind, bucket, quantity_delta, balance_after, reason, reference_type, reference_id, actor_user_id, created_at
         FROM stock_movements WHERE store_id = $1 AND ($2::uuid IS NULL OR variant_id = $2) ORDER BY id DESC LIMIT $3",
    )
    .bind(store)
    .bind(q.variant_id)
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<i64, _>("id"), "variant_id": r.get::<Uuid, _>("variant_id"), "batch_id": r.get::<Option<Uuid>, _>("batch_id"), "group_id": r.get::<Uuid, _>("group_id"),
        "kind": r.get::<String, _>("kind"), "bucket": r.get::<String, _>("bucket"), "quantity_delta": r.get::<i64, _>("quantity_delta"), "balance_after": r.get::<i64, _>("balance_after"),
        "reason": r.get::<Option<String>, _>("reason"), "reference_type": r.get::<Option<String>, _>("reference_type"), "reference_id": r.get::<Option<String>, _>("reference_id"),
        "actor_user_id": r.get::<Option<Uuid>, _>("actor_user_id"), "created_at": r.get::<DateTime<Utc>, _>("created_at"),
    })).collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct AlertQuery {
    status: Option<String>,
}

pub async fn alerts(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Query(q): Query<AlertQuery>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &STAFF, "view_stock_alerts").await?;
    let status = q.status.unwrap_or_else(|| "open".into());
    let rows = sqlx::query(
        "SELECT a.id, a.variant_id, v.sku, a.status, a.on_hand_at_trigger, a.reorder_level, a.created_at, a.resolved_at
         FROM stock_alerts a JOIN product_variants v ON v.id = a.variant_id WHERE a.store_id = $1 AND a.status = $2 ORDER BY a.created_at DESC LIMIT 200",
    )
    .bind(store)
    .bind(status)
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid, _>("id"), "variant_id": r.get::<Uuid, _>("variant_id"), "sku": r.get::<String, _>("sku"), "status": r.get::<String, _>("status"),
        "on_hand_at_trigger": r.get::<i64, _>("on_hand_at_trigger"), "reorder_level": r.get::<i64, _>("reorder_level"),
        "created_at": r.get::<DateTime<Utc>, _>("created_at"), "resolved_at": r.get::<Option<DateTime<Utc>>, _>("resolved_at"),
    })).collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct ReorderBody {
    reorder_level: Option<i64>,
}

/// Sets (or, with null, clears) the level at which a variant counts as low stock in this store.
pub async fn set_reorder_level(State(state): State<AppState>, p: Principal, Path((store, variant)): Path<(Uuid, Uuid)>, Json(b): Json<ReorderBody>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &STAFF, "set_reorder_level").await?;
    if b.reorder_level.is_some_and(|l| l < 0) {
        return Err(ApiError::bad("the reorder level cannot be negative"));
    }
    let mut tx = state.pool.begin().await?;
    inventory::tracks_expiry(&mut tx, p.business_id, variant).await?;
    sqlx::query(
        "INSERT INTO stock_balances (store_id, variant_id, business_id, reorder_level) VALUES ($1,$2,$3,$4)
         ON CONFLICT (store_id, variant_id) DO UPDATE SET reorder_level = EXCLUDED.reorder_level, updated_at = now()",
    )
    .bind(store)
    .bind(variant)
    .bind(p.business_id)
    .bind(b.reorder_level)
    .execute(&mut *tx)
    .await?;
    inventory::refresh_alert(&mut tx, p.business_id, store, variant).await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "set_reorder_level", entity_type: "variant", entity_id: Some(variant.to_string()), after: Some(json!({ "reorder_level": b.reorder_level })), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "variant_id": variant, "reorder_level": b.reorder_level })))
}

pub async fn get_settings(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &READ, "view_stock_settings").await?;
    let t: i64 = sqlx::query_scalar("SELECT COALESCE((SELECT adjustment_approval_threshold FROM store_stock_settings WHERE store_id = $1), 50)").bind(store).fetch_one(&state.pool).await?;
    Ok(Json(json!({ "adjustment_approval_threshold": t })))
}

#[derive(Deserialize)]
pub struct StockSettings {
    adjustment_approval_threshold: i64,
}

pub async fn put_settings(State(state): State<AppState>, p: Principal, Path(store): Path<Uuid>, Json(b): Json<StockSettings>) -> ApiResult<Json<Value>> {
    p.authorize_store(&state, store, &APPROVE, "update_stock_settings").await?;
    if b.adjustment_approval_threshold < 0 {
        return Err(ApiError::bad("the threshold cannot be negative"));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO store_stock_settings (store_id, adjustment_approval_threshold, updated_by) VALUES ($1,$2,$3)
         ON CONFLICT (store_id) DO UPDATE SET adjustment_approval_threshold = EXCLUDED.adjustment_approval_threshold, updated_at = now(), updated_by = EXCLUDED.updated_by",
    )
    .bind(store)
    .bind(b.adjustment_approval_threshold)
    .bind(p.user_id)
    .execute(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry { business_id: Some(p.business_id), store_id: Some(store), actor: Some(p.user_id), action: "update_stock_settings", entity_type: "store", entity_id: Some(store.to_string()), after: Some(json!({ "adjustment_approval_threshold": b.adjustment_approval_threshold })), ..Default::default() }).await?;
    tx.commit().await?;
    Ok(Json(json!({ "adjustment_approval_threshold": b.adjustment_approval_threshold })))
}
