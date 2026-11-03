-- Closes gaps found when the built features were checked against the SRS text (sections 1 to 5).

-- FR-07: brand, image and additional attributes; FR-18: products can be marked non-returnable.
ALTER TABLE products
    ADD COLUMN brand      TEXT,
    ADD COLUMN image_url  TEXT,
    ADD COLUMN attributes JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN returnable BOOLEAN NOT NULL DEFAULT TRUE;

-- FR-09: every price row also records the price it replaced.
ALTER TABLE variant_prices ADD COLUMN previous_price_minor BIGINT;

-- FR-12: an adjustment can carry a supporting reference (delivery note, incident number ...).
ALTER TABLE stock_adjustments ADD COLUMN reference TEXT;

-- FR-14: the near-expiry warning period is a store setting.
ALTER TABLE store_stock_settings
    ADD COLUMN expiry_warning_days INTEGER NOT NULL DEFAULT 30 CHECK (expiry_warning_days BETWEEN 0 AND 3650);

-- FR-15: discounts. `line_subtotal_minor` stays the amount tax is charged on (after the discount).
ALTER TABLE sale_lines ADD COLUMN discount_minor BIGINT NOT NULL DEFAULT 0 CHECK (discount_minor >= 0);

-- FR-16: payment status, an "other" method, and one payment per external reference.
ALTER TABLE sale_payments
    ADD COLUMN status TEXT NOT NULL DEFAULT 'successful' CHECK (status IN ('pending', 'successful', 'failed', 'reversed', 'refunded'));
ALTER TABLE sale_payments DROP CONSTRAINT sale_payments_method_check;
ALTER TABLE sale_payments ADD CONSTRAINT sale_payments_method_check CHECK (method IN ('cash', 'card', 'transfer', 'other'));
CREATE UNIQUE INDEX sale_payments_reference ON sale_payments (sale_id, method, reference) WHERE reference IS NOT NULL;

-- A payment's amount, method and reference never change; only its status moves (pending -> successful, ...).
DROP TRIGGER sale_payments_no_update ON sale_payments;
CREATE FUNCTION sale_payments_only_status_changes() RETURNS trigger AS $$
BEGIN
    IF (NEW.id, NEW.sale_id, NEW.method, NEW.amount_minor, NEW.reference)
       IS DISTINCT FROM (OLD.id, OLD.sale_id, OLD.method, OLD.amount_minor, OLD.reference) THEN
        RAISE EXCEPTION 'only the status of a payment can change';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER sale_payments_status_only BEFORE UPDATE ON sale_payments FOR EACH ROW EXECUTE FUNCTION sale_payments_only_status_changes();

-- FR-18: refund status, approval details, and the same method list as payments.
ALTER TABLE returns DROP CONSTRAINT returns_refund_method_check;
ALTER TABLE returns ADD CONSTRAINT returns_refund_method_check CHECK (refund_method IN ('cash', 'card', 'transfer', 'other'));
ALTER TABLE returns
    ADD COLUMN refund_status TEXT NOT NULL DEFAULT 'refunded' CHECK (refund_status IN ('pending', 'refunded', 'failed')),
    ADD COLUMN approved_by   UUID REFERENCES users (id);

-- FR-17: every time a receipt is printed, so a reprint can be identified as one.
CREATE TABLE receipt_prints (
    id         BIGSERIAL PRIMARY KEY,
    sale_id    UUID NOT NULL REFERENCES sales (id),
    printed_by UUID NOT NULL REFERENCES users (id),
    printed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX receipt_prints_sale ON receipt_prints (sale_id);
