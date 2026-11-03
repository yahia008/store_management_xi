-- Inventory and stock control (SRS FR-10 to FR-14).
--
-- Stock is never edited directly. Every change is a row in `stock_movements`, written by one shared
-- service in the same transaction that updates the balance, so the balance and the ledger cannot drift.

-- FR-10: stock per store and variant, split into three buckets so damaged and expired goods are never
-- counted as sellable. The CHECKs make a negative bucket impossible even if application code is wrong.
CREATE TABLE stock_balances (
    store_id      UUID NOT NULL REFERENCES stores (id),
    variant_id    UUID NOT NULL REFERENCES product_variants (id),
    business_id   UUID NOT NULL REFERENCES businesses (id),
    on_hand       BIGINT NOT NULL DEFAULT 0 CHECK (on_hand >= 0),
    damaged       BIGINT NOT NULL DEFAULT 0 CHECK (damaged >= 0),
    expired       BIGINT NOT NULL DEFAULT 0 CHECK (expired >= 0),
    reorder_level BIGINT CHECK (reorder_level >= 0),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store_id, variant_id)
);

-- FR-13: batches for goods that expire. For such variants the batch quantities add up to `on_hand`.
CREATE TABLE stock_batches (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id     UUID NOT NULL REFERENCES businesses (id),
    store_id        UUID NOT NULL REFERENCES stores (id),
    variant_id      UUID NOT NULL REFERENCES product_variants (id),
    batch_code      TEXT NOT NULL CHECK (length(btrim(batch_code)) > 0),
    expiry_date     DATE,
    quantity        BIGINT NOT NULL DEFAULT 0 CHECK (quantity >= 0),
    unit_cost_minor BIGINT CHECK (unit_cost_minor >= 0),
    received_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (store_id, variant_id, batch_code)
);
CREATE INDEX stock_batches_fefo ON stock_batches (store_id, variant_id, expiry_date NULLS LAST) WHERE quantity > 0;

-- FR-11: the ledger. `quantity_delta` is signed and applies to `bucket`; a transfer between buckets
-- (damage, expiry) is two rows sharing a `group_id`.
CREATE TABLE stock_movements (
    id             BIGSERIAL PRIMARY KEY,
    business_id    UUID NOT NULL REFERENCES businesses (id),
    store_id       UUID NOT NULL REFERENCES stores (id),
    variant_id     UUID NOT NULL REFERENCES product_variants (id),
    batch_id       UUID REFERENCES stock_batches (id),
    group_id       UUID NOT NULL,
    kind           TEXT NOT NULL,
    bucket         TEXT NOT NULL CHECK (bucket IN ('on_hand', 'damaged', 'expired')),
    quantity_delta BIGINT NOT NULL CHECK (quantity_delta <> 0),
    balance_after  BIGINT NOT NULL CHECK (balance_after >= 0),
    reason         TEXT,
    reference_type TEXT,
    reference_id   TEXT,
    actor_user_id  UUID,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX stock_movements_lookup ON stock_movements (store_id, variant_id, id DESC);

CREATE FUNCTION stock_movements_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'stock_movements is append-only';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER stock_movements_no_update BEFORE UPDATE ON stock_movements FOR EACH ROW EXECUTE FUNCTION stock_movements_are_append_only();
CREATE TRIGGER stock_movements_no_delete BEFORE DELETE ON stock_movements FOR EACH ROW EXECUTE FUNCTION stock_movements_are_append_only();
CREATE TRIGGER stock_movements_no_truncate BEFORE TRUNCATE ON stock_movements FOR EACH STATEMENT EXECUTE FUNCTION stock_movements_are_append_only();

-- FR-12: adjustments. Big ones wait here as `pending` until someone else approves them.
CREATE TABLE stock_adjustments (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id   UUID NOT NULL REFERENCES businesses (id),
    store_id      UUID NOT NULL REFERENCES stores (id),
    variant_id    UUID NOT NULL REFERENCES product_variants (id),
    batch_id      UUID REFERENCES stock_batches (id),
    kind          TEXT NOT NULL CHECK (kind IN ('increase', 'decrease', 'damage')),
    quantity      BIGINT NOT NULL CHECK (quantity > 0),
    reason        TEXT NOT NULL CHECK (length(btrim(reason)) > 0),
    source        TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('manual', 'count')),
    status        TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'rejected')),
    requested_by  UUID NOT NULL,
    decided_by    UUID,
    decided_at    TIMESTAMPTZ,
    decision_note TEXT,
    auto_approved BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX stock_adjustments_store ON stock_adjustments (store_id, status, created_at DESC);

CREATE TABLE store_stock_settings (
    store_id                      UUID PRIMARY KEY REFERENCES stores (id),
    adjustment_approval_threshold BIGINT NOT NULL DEFAULT 50 CHECK (adjustment_approval_threshold >= 0),
    updated_at                    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by                    UUID
);

-- FR-12: stock counts, kept with what the system expected and what was found.
CREATE TABLE stock_counts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses (id),
    store_id    UUID NOT NULL REFERENCES stores (id),
    created_by  UUID NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE stock_count_lines (
    id            BIGSERIAL PRIMARY KEY,
    count_id      UUID NOT NULL REFERENCES stock_counts (id),
    variant_id    UUID NOT NULL REFERENCES product_variants (id),
    batch_id      UUID REFERENCES stock_batches (id),
    system_qty    BIGINT NOT NULL,
    counted_qty   BIGINT NOT NULL CHECK (counted_qty >= 0),
    variance      BIGINT NOT NULL,
    adjustment_id UUID REFERENCES stock_adjustments (id)
);

-- FR-14: one open low-stock alert per store and variant, however many sales happen while it is low.
CREATE TABLE stock_alerts (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id         UUID NOT NULL REFERENCES businesses (id),
    store_id            UUID NOT NULL REFERENCES stores (id),
    variant_id          UUID NOT NULL REFERENCES product_variants (id),
    status              TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved')),
    on_hand_at_trigger  BIGINT NOT NULL,
    reorder_level       BIGINT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at         TIMESTAMPTZ
);
CREATE UNIQUE INDEX stock_alerts_one_open ON stock_alerts (store_id, variant_id) WHERE status = 'open';
