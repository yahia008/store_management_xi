-- Product management (SRS FR-07 to FR-09): categories, products, variants and price history.
-- The catalogue is shared by the whole business; what differs per store is the price override (below)
-- and, later, the stock held (FR-10).

CREATE TABLE categories (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses (id),
    parent_id   UUID REFERENCES categories (id),
    name        TEXT NOT NULL CHECK (length(btrim(name)) > 0),
    is_active   BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX categories_unique_name
    ON categories (business_id, COALESCE(parent_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(name));

-- Products are archived, never deleted, so old sales and stock history keep pointing at something real.
CREATE TABLE products (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id  UUID NOT NULL REFERENCES businesses (id),
    category_id  UUID REFERENCES categories (id),
    name         TEXT NOT NULL CHECK (length(btrim(name)) > 0),
    description  TEXT,
    unit         TEXT NOT NULL DEFAULT 'each',
    tax_exempt   BOOLEAN NOT NULL DEFAULT FALSE,
    -- Perishable goods are stocked in expiring batches (FR-13); the flag is stored now so the catalogue is complete.
    tracks_expiry BOOLEAN NOT NULL DEFAULT FALSE,
    status       TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived')),
    created_by   UUID,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX products_business ON products (business_id, status);
CREATE INDEX products_category ON products (category_id);

-- What is actually sold and counted. A product with no options has one variant.
CREATE TABLE product_variants (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id  UUID NOT NULL REFERENCES products (id),
    business_id UUID NOT NULL REFERENCES businesses (id),
    sku         TEXT NOT NULL CHECK (length(btrim(sku)) > 0),
    barcode     TEXT,
    name        TEXT,
    attributes  JSONB NOT NULL DEFAULT '{}'::jsonb,
    cost_minor  BIGINT CHECK (cost_minor >= 0),
    status      TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX variants_sku ON product_variants (business_id, lower(sku));
CREATE UNIQUE INDEX variants_barcode ON product_variants (business_id, barcode) WHERE barcode IS NOT NULL;
CREATE INDEX variants_product ON product_variants (product_id);

-- Price history. A row is added for every price change and never edited, so what a sale was charged can
-- always be traced. `store_id` NULL is the business-wide price; a row with a store is that store's override.
CREATE TABLE variant_prices (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id    UUID NOT NULL REFERENCES businesses (id),
    variant_id     UUID NOT NULL REFERENCES product_variants (id),
    store_id       UUID REFERENCES stores (id),
    price_minor    BIGINT NOT NULL CHECK (price_minor >= 0),
    effective_from TIMESTAMPTZ NOT NULL,
    created_by     UUID,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX variant_prices_lookup ON variant_prices (variant_id, store_id, effective_from DESC);

CREATE FUNCTION price_history_is_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'variant_prices is append-only';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER variant_prices_no_update BEFORE UPDATE ON variant_prices FOR EACH ROW EXECUTE FUNCTION price_history_is_append_only();
CREATE TRIGGER variant_prices_no_delete BEFORE DELETE ON variant_prices FOR EACH ROW EXECUTE FUNCTION price_history_is_append_only();
