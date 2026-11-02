-- Business, stores, accounts and access control (SRS FR-01 to FR-06).
--
-- Every row that belongs to a business carries `business_id`, and every store-level row carries
-- `store_id`, so adding businesses and stores never needs a new schema (SRS NFR-07). Identifiers are
-- system-generated UUIDs. Times are `timestamptz` (UTC).

CREATE TABLE businesses (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    legal_name          TEXT NOT NULL CHECK (length(btrim(legal_name)) > 0),
    trading_name        TEXT,
    registration_number TEXT,
    address             TEXT,
    phone               TEXT,
    email               TEXT,
    logo_url            TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by          UUID
);

-- Business-wide settings (FR-03). The base currency is an ISO 4217 code; monetary amounts elsewhere are
-- whole minor units (kobo, cents) so no floating point is ever used for money (NFR-30).
CREATE TABLE business_settings (
    business_id    UUID PRIMARY KEY REFERENCES businesses (id),
    base_currency  CHAR(3) NOT NULL CHECK (base_currency ~ '^[A-Z]{3}$'),
    timezone       TEXT NOT NULL,
    receipt_header TEXT,
    receipt_footer TEXT,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by     UUID
);

-- Tax rates are effective-dated and only ever added: a new rate applies from its `effective_from`, and a
-- sale keeps the rate it was made with, so history is never silently rewritten (FR-03).
CREATE TABLE tax_rates (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id    UUID NOT NULL REFERENCES businesses (id),
    name           TEXT NOT NULL CHECK (length(btrim(name)) > 0),
    rate_bps       INTEGER NOT NULL CHECK (rate_bps BETWEEN 0 AND 10000), -- basis points: 750 = 7.5%
    effective_from TIMESTAMPTZ NOT NULL,
    created_by     UUID,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX tax_rates_effective ON tax_rates (business_id, effective_from DESC);

CREATE TABLE stores (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses (id),
    name        TEXT NOT NULL CHECK (length(btrim(name)) > 0),
    address     TEXT,
    phone       TEXT,
    email       TEXT,
    status      TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'inactive')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  UUID
);
CREATE UNIQUE INDEX stores_business_name ON stores (business_id, lower(name));
CREATE INDEX stores_business ON stores (business_id);

-- Store-level settings (FR-03): receipt and transaction numbering. The counters are advanced inside the
-- transaction that uses them, so two sales can never be given the same number.
CREATE TABLE store_settings (
    store_id                UUID PRIMARY KEY REFERENCES stores (id),
    receipt_prefix          TEXT NOT NULL DEFAULT 'RCT' CHECK (receipt_prefix ~ '^[A-Z0-9-]{1,10}$'),
    transaction_prefix      TEXT NOT NULL DEFAULT 'TXN' CHECK (transaction_prefix ~ '^[A-Z0-9-]{1,10}$'),
    next_receipt_number     BIGINT NOT NULL DEFAULT 1 CHECK (next_receipt_number >= 1),
    next_transaction_number BIGINT NOT NULL DEFAULT 1 CHECK (next_transaction_number >= 1),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by              UUID
);

-- Staff accounts (FR-04, FR-05). Passwords are stored only as Argon2id hashes. An account belongs to one
-- business; its access to stores comes from `memberships`.
CREATE TABLE users (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id         UUID NOT NULL REFERENCES businesses (id),
    email               TEXT NOT NULL,
    full_name           TEXT NOT NULL CHECK (length(btrim(full_name)) > 0),
    phone               TEXT,
    password_hash       TEXT NOT NULL,
    is_active           BOOLEAN NOT NULL DEFAULT TRUE,
    failed_login_count  INTEGER NOT NULL DEFAULT 0,
    locked_until        TIMESTAMPTZ,
    password_changed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by          UUID,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email ON users (lower(email));
CREATE INDEX users_business ON users (business_id);

-- One row per role an employee holds. `store_id` is NULL for the two business-wide roles and set for the
-- store-level roles, so the same person can be a manager in Lagos and a cashier in Abuja (FR-05).
CREATE TABLE memberships (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    business_id UUID NOT NULL REFERENCES businesses (id),
    store_id    UUID REFERENCES stores (id),
    role        TEXT NOT NULL CHECK (role IN
                    ('business_owner', 'administrator', 'store_manager', 'cashier', 'inventory_officer', 'finance_officer')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((role IN ('business_owner', 'administrator')) = (store_id IS NULL))
);
CREATE UNIQUE INDEX memberships_unique
    ON memberships (user_id, COALESCE(store_id, '00000000-0000-0000-0000-000000000000'::uuid), role);
CREATE INDEX memberships_user ON memberships (user_id);
CREATE INDEX memberships_store ON memberships (store_id);

-- Sessions (FR-04). Only a SHA-256 of the token is kept, so a copy of this table cannot be used to sign in.
CREATE TABLE sessions (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id              UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash           TEXT NOT NULL UNIQUE,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at           TIMESTAMPTZ NOT NULL,
    last_used_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    idle_timeout_minutes INTEGER NOT NULL CHECK (idle_timeout_minutes > 0),
    revoked_at           TIMESTAMPTZ,
    revoked_reason       TEXT
);
CREATE INDEX sessions_user ON sessions (user_id) WHERE revoked_at IS NULL;

-- Single-use password-reset tokens (FR-04, NFR-16): hashed, short-lived, and marked used when spent.
CREATE TABLE password_reset_tokens (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);

-- The audit trail (FR-27 builds on this). Entries are only ever added: the triggers below refuse any
-- UPDATE or DELETE, so ordinary application code cannot rewrite history.
CREATE TABLE audit_log (
    id             BIGSERIAL PRIMARY KEY,
    business_id    UUID,
    store_id       UUID,
    actor_user_id  UUID,
    action         TEXT NOT NULL,
    entity_type    TEXT NOT NULL,
    entity_id      TEXT,
    before_values  JSONB,
    after_values   JSONB,
    outcome        TEXT NOT NULL DEFAULT 'success',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_business_time ON audit_log (business_id, created_at DESC);
CREATE INDEX audit_entity ON audit_log (entity_type, entity_id);

CREATE FUNCTION audit_log_is_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_is_append_only();
CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_is_append_only();
CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION audit_log_is_append_only();
