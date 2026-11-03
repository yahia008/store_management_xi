-- Sales, returns and customers (SRS FR-15 to FR-19).
--
-- Money is whole minor units. A sale stores the price, tax rate and product name it was made with, so
-- later price or tax changes never alter an old receipt.

CREATE TABLE customers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses (id),
    name        TEXT NOT NULL CHECK (length(btrim(name)) > 0),
    phone       TEXT,
    email       TEXT,
    notes       TEXT,
    is_active   BOOLEAN NOT NULL DEFAULT TRUE,
    created_by  UUID,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX customers_phone ON customers (business_id, phone) WHERE phone IS NOT NULL;
CREATE INDEX customers_name ON customers (business_id, lower(name));

ALTER TABLE store_settings ADD COLUMN next_return_number BIGINT NOT NULL DEFAULT 1 CHECK (next_return_number >= 1);

CREATE TABLE sales (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id        UUID NOT NULL REFERENCES businesses (id),
    store_id           UUID NOT NULL REFERENCES stores (id),
    cashier_id         UUID NOT NULL REFERENCES users (id),
    customer_id        UUID REFERENCES customers (id),
    receipt_number     TEXT NOT NULL,
    transaction_number TEXT NOT NULL,
    currency           CHAR(3) NOT NULL,
    subtotal_minor     BIGINT NOT NULL CHECK (subtotal_minor >= 0),
    tax_minor          BIGINT NOT NULL CHECK (tax_minor >= 0),
    total_minor        BIGINT NOT NULL CHECK (total_minor >= 0),
    paid_minor         BIGINT NOT NULL,
    change_minor       BIGINT NOT NULL CHECK (change_minor >= 0),
    idempotency_key    TEXT NOT NULL,
    request_hash       TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (total_minor = subtotal_minor + tax_minor),
    CHECK (paid_minor >= total_minor AND change_minor = paid_minor - total_minor)
);
CREATE UNIQUE INDEX sales_idempotency ON sales (business_id, idempotency_key);
CREATE UNIQUE INDEX sales_receipt ON sales (store_id, receipt_number);
CREATE INDEX sales_store_time ON sales (store_id, created_at DESC);
CREATE INDEX sales_customer ON sales (customer_id, created_at DESC);

CREATE TABLE sale_lines (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    sale_id            UUID NOT NULL REFERENCES sales (id),
    variant_id         UUID NOT NULL REFERENCES product_variants (id),
    product_name       TEXT NOT NULL,
    sku                TEXT NOT NULL,
    quantity           BIGINT NOT NULL CHECK (quantity > 0),
    unit_price_minor   BIGINT NOT NULL CHECK (unit_price_minor >= 0),
    tax_exempt         BOOLEAN NOT NULL,
    tax_rate_bps       INTEGER NOT NULL CHECK (tax_rate_bps BETWEEN 0 AND 10000),
    line_subtotal_minor BIGINT NOT NULL CHECK (line_subtotal_minor >= 0),
    line_tax_minor     BIGINT NOT NULL CHECK (line_tax_minor >= 0),
    line_total_minor   BIGINT NOT NULL CHECK (line_total_minor >= 0),
    returned_qty       BIGINT NOT NULL DEFAULT 0,
    CHECK (returned_qty >= 0 AND returned_qty <= quantity)
);
CREATE INDEX sale_lines_sale ON sale_lines (sale_id);

-- Which batches a sale line took stock from, so a return can put units back where they came from.
CREATE TABLE sale_line_allocations (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    seq          BIGSERIAL NOT NULL, -- the order stock was taken in, so returns refill batches in that same order
    sale_line_id UUID NOT NULL REFERENCES sale_lines (id),
    batch_id     UUID REFERENCES stock_batches (id),
    quantity     BIGINT NOT NULL CHECK (quantity > 0),
    returned_qty BIGINT NOT NULL DEFAULT 0,
    CHECK (returned_qty >= 0 AND returned_qty <= quantity)
);

CREATE TABLE sale_payments (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    sale_id      UUID NOT NULL REFERENCES sales (id),
    method       TEXT NOT NULL CHECK (method IN ('cash', 'card', 'transfer')),
    amount_minor BIGINT NOT NULL CHECK (amount_minor > 0),
    reference    TEXT
);

CREATE TABLE returns (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id     UUID NOT NULL REFERENCES businesses (id),
    store_id        UUID NOT NULL REFERENCES stores (id),
    sale_id         UUID NOT NULL REFERENCES sales (id),
    return_number   TEXT NOT NULL,
    reason          TEXT NOT NULL CHECK (length(btrim(reason)) > 0),
    refund_method   TEXT NOT NULL CHECK (refund_method IN ('cash', 'card', 'transfer')),
    refund_minor    BIGINT NOT NULL CHECK (refund_minor >= 0),
    created_by      UUID NOT NULL REFERENCES users (id),
    idempotency_key TEXT NOT NULL,
    request_hash    TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX returns_idempotency ON returns (business_id, idempotency_key);
CREATE UNIQUE INDEX returns_number ON returns (store_id, return_number);
CREATE INDEX returns_sale ON returns (sale_id);

CREATE TABLE return_lines (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    return_id    UUID NOT NULL REFERENCES returns (id),
    sale_line_id UUID NOT NULL REFERENCES sale_lines (id),
    quantity     BIGINT NOT NULL CHECK (quantity > 0),
    refund_minor BIGINT NOT NULL CHECK (refund_minor >= 0),
    disposition  TEXT NOT NULL CHECK (disposition IN ('restock', 'damaged'))
);

-- Sales records are permanent: nothing is deleted, and the sale header and payments never change.
-- (Only `sale_lines.returned_qty` and the allocation counters move, when goods come back.)
CREATE FUNCTION sales_records_are_permanent() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% records cannot be changed or deleted', TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER sales_no_update BEFORE UPDATE ON sales FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER sales_no_delete BEFORE DELETE ON sales FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER sale_lines_no_delete BEFORE DELETE ON sale_lines FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER sale_payments_no_update BEFORE UPDATE ON sale_payments FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER sale_payments_no_delete BEFORE DELETE ON sale_payments FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER returns_no_update BEFORE UPDATE ON returns FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER returns_no_delete BEFORE DELETE ON returns FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER return_lines_no_update BEFORE UPDATE ON return_lines FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
CREATE TRIGGER return_lines_no_delete BEFORE DELETE ON return_lines FOR EACH ROW EXECUTE FUNCTION sales_records_are_permanent();
