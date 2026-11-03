//! The one place stock changes (FR-11). Handlers and, later, sales and returns all call [`apply`] or
//! [`consume`]; nothing else writes `stock_balances`, `stock_batches` or `stock_movements`.
//!
//! Each call runs inside the caller's transaction. The balance update is conditional
//! (`... AND bucket + delta >= 0`), so two requests racing for the last unit cannot both win and stock
//! cannot go negative.

use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bucket {
    OnHand,
    Damaged,
    Expired,
}

impl Bucket {
    pub fn name(self) -> &'static str {
        match self {
            Bucket::OnHand => "on_hand",
            Bucket::Damaged => "damaged",
            Bucket::Expired => "expired",
        }
    }
}

pub struct Move<'a> {
    pub business_id: Uuid,
    pub store_id: Uuid,
    pub variant_id: Uuid,
    pub batch_id: Option<Uuid>,
    pub bucket: Bucket,
    pub delta: i64,
    pub kind: &'a str,
    pub group_id: Uuid,
    pub reason: Option<&'a str>,
    pub reference: Option<(&'a str, String)>,
    pub actor: Uuid,
}

/// Does this variant's product keep expiring batches? Also proves the variant belongs to the business.
pub async fn tracks_expiry(db: &mut PgConnection, business_id: Uuid, variant_id: Uuid) -> ApiResult<bool> {
    sqlx::query_scalar("SELECT p.tracks_expiry FROM product_variants v JOIN products p ON p.id = v.product_id WHERE v.id = $1 AND v.business_id = $2")
        .bind(variant_id)
        .bind(business_id)
        .fetch_optional(&mut *db)
        .await?
        .ok_or(ApiError::NotFound("variant"))
}

/// Apply one signed movement to one bucket. Returns the bucket's balance afterwards.
pub async fn apply(db: &mut PgConnection, m: &Move<'_>) -> ApiResult<i64> {
    if m.delta == 0 {
        return Err(ApiError::bad("a stock movement cannot be zero"));
    }
    let tracks = tracks_expiry(db, m.business_id, m.variant_id).await?;
    sqlx::query("INSERT INTO stock_balances (store_id, variant_id, business_id) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(m.store_id)
        .bind(m.variant_id)
        .bind(m.business_id)
        .execute(&mut *db)
        .await?;

    if m.bucket == Bucket::OnHand {
        match (tracks, m.batch_id) {
            (true, None) => return Err(ApiError::bad("this product is stocked in batches; say which batch")),
            (false, Some(_)) => return Err(ApiError::bad("this product has no batches")),
            _ => {}
        }
        if let Some(batch) = m.batch_id {
            let ok = sqlx::query("UPDATE stock_batches SET quantity = quantity + $4 WHERE id = $1 AND store_id = $2 AND variant_id = $3 AND quantity + $4 >= 0")
                .bind(batch)
                .bind(m.store_id)
                .bind(m.variant_id)
                .bind(m.delta)
                .execute(&mut *db)
                .await?
                .rows_affected();
            if ok == 0 {
                return Err(ApiError::conflict("that batch does not exist here or does not have enough stock"));
            }
        }
    }

    let col = m.bucket.name();
    let balance: Option<i64> = sqlx::query_scalar(&format!(
        "UPDATE stock_balances SET {col} = {col} + $3, updated_at = now() WHERE store_id = $1 AND variant_id = $2 AND {col} + $3 >= 0 RETURNING {col}"
    ))
    .bind(m.store_id)
    .bind(m.variant_id)
    .bind(m.delta)
    .fetch_optional(&mut *db)
    .await?;
    let balance = balance.ok_or_else(|| ApiError::conflict("not enough stock"))?;

    sqlx::query(
        "INSERT INTO stock_movements (business_id, store_id, variant_id, batch_id, group_id, kind, bucket, quantity_delta, balance_after, reason, reference_type, reference_id, actor_user_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(m.business_id)
    .bind(m.store_id)
    .bind(m.variant_id)
    .bind(m.batch_id)
    .bind(m.group_id)
    .bind(m.kind)
    .bind(col)
    .bind(m.delta)
    .bind(balance)
    .bind(m.reason)
    .bind(m.reference.as_ref().map(|r| r.0))
    .bind(m.reference.as_ref().map(|r| r.1.clone()))
    .bind(m.actor)
    .execute(&mut *db)
    .await?;

    if m.bucket == Bucket::OnHand {
        refresh_alert(db, m.business_id, m.store_id, m.variant_id).await?;
    }
    Ok(balance)
}

/// Take `qty` sellable units out, oldest expiry first (FEFO) for batched goods; expired batches are
/// skipped. Fails without changing anything if there is not enough. Returns (batch, quantity) taken.
pub async fn consume(db: &mut PgConnection, base: &Move<'_>, qty: i64) -> ApiResult<Vec<(Option<Uuid>, i64)>> {
    if qty <= 0 {
        return Err(ApiError::bad("quantity must be positive"));
    }
    let tracks = tracks_expiry(db, base.business_id, base.variant_id).await?;
    let mk = |batch: Option<Uuid>, delta: i64| Move {
        business_id: base.business_id, store_id: base.store_id, variant_id: base.variant_id, batch_id: batch, bucket: Bucket::OnHand,
        delta, kind: base.kind, group_id: base.group_id, reason: base.reason, reference: base.reference.clone(), actor: base.actor,
    };
    if !tracks {
        apply(db, &mk(None, -qty)).await?;
        return Ok(vec![(None, qty)]);
    }
    let rows = sqlx::query(
        "SELECT id, quantity FROM stock_batches
         WHERE store_id = $1 AND variant_id = $2 AND quantity > 0
           AND (expiry_date IS NULL OR expiry_date >= (now() AT TIME ZONE (SELECT timezone FROM business_settings WHERE business_id = $3))::date)
         ORDER BY expiry_date NULLS LAST, received_at, id FOR UPDATE",
    )
    .bind(base.store_id)
    .bind(base.variant_id)
    .bind(base.business_id)
    .fetch_all(&mut *db)
    .await?;
    let mut left = qty;
    let mut taken = Vec::new();
    for r in rows {
        if left == 0 {
            break;
        }
        let (id, have): (Uuid, i64) = (r.get("id"), r.get("quantity"));
        let take = have.min(left);
        taken.push((Some(id), take));
        left -= take;
    }
    if left > 0 {
        return Err(ApiError::conflict("not enough unexpired stock"));
    }
    for (batch, take) in &taken {
        apply(db, &mk(*batch, -take)).await?;
    }
    Ok(taken)
}

/// Keep the low-stock alert in step with the balance (FR-14). At most one alert is open per store and
/// variant, so a busy day does not produce a flood; it closes by itself once stock is back above the level.
pub async fn refresh_alert(db: &mut PgConnection, business_id: Uuid, store_id: Uuid, variant_id: Uuid) -> ApiResult<()> {
    let row = sqlx::query("SELECT on_hand, reorder_level FROM stock_balances WHERE store_id = $1 AND variant_id = $2").bind(store_id).bind(variant_id).fetch_optional(&mut *db).await?;
    let Some(row) = row else { return Ok(()) };
    let on_hand: i64 = row.get("on_hand");
    let level: Option<i64> = row.get("reorder_level");
    match level {
        Some(level) if on_hand <= level => {
            sqlx::query(
                "INSERT INTO stock_alerts (business_id, store_id, variant_id, on_hand_at_trigger, reorder_level) VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (store_id, variant_id) WHERE status = 'open' DO NOTHING",
            )
            .bind(business_id)
            .bind(store_id)
            .bind(variant_id)
            .bind(on_hand)
            .bind(level)
            .execute(&mut *db)
            .await?;
        }
        _ => {
            sqlx::query("UPDATE stock_alerts SET status = 'resolved', resolved_at = now() WHERE store_id = $1 AND variant_id = $2 AND status = 'open'")
                .bind(store_id)
                .bind(variant_id)
                .execute(&mut *db)
                .await?;
        }
    }
    Ok(())
}
