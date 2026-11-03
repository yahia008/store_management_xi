-- NFR-17 multi-factor authentication for staff, NFR-20 API abuse protection.

-- A TOTP secret is "pending" until the user proves they can produce a code (mfa_enabled_at is NULL),
-- then enabled. `mfa_last_step` is the last 30-second step accepted, so a code cannot be used twice.
ALTER TABLE users
    ADD COLUMN mfa_secret     TEXT,
    ADD COLUMN mfa_enabled_at TIMESTAMPTZ,
    ADD COLUMN mfa_last_step  BIGINT;

-- Whether this session completed MFA. Owners and administrators cannot use admin functions without it.
ALTER TABLE sessions ADD COLUMN mfa_verified BOOLEAN NOT NULL DEFAULT FALSE;

-- The short step between a correct password and a session: valid for minutes, a few tries, single use.
CREATE TABLE mfa_challenges (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    used_at    TIMESTAMPTZ
);

-- Single-use recovery codes for a lost phone. Only hashes are stored.
CREATE TABLE mfa_recovery_codes (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    used_at    TIMESTAMPTZ,
    UNIQUE (user_id, code_hash)
);

-- Rate-limit counters live in the database, not in one server's memory, so several application instances
-- share one count (NFR-06). One row per client bucket and time window.
CREATE TABLE rate_limits (
    bucket       TEXT NOT NULL,
    window_start TIMESTAMPTZ NOT NULL,
    hits         INTEGER NOT NULL,
    PRIMARY KEY (bucket, window_start)
);
