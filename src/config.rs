use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub session_ttl_hours: i64,
    pub session_idle_minutes: i32,
    pub admin_session_idle_minutes: i32,
    pub max_failed_logins: i32,
    pub lockout_minutes: i64,
    pub reset_token_ttl_minutes: i64,
    pub dev_print_reset_tokens: bool,
    /// Returns after this many days, or refunds above the limit, need a store manager (FR-17).
    pub return_window_days: i64,
    pub return_manager_limit_minor: i64,
    /// A cashier may discount a line by up to this many basis points (1000 = 10%); more needs a manager (FR-15, FR-29).
    pub cashier_max_discount_bps: i64,
    /// Owners and administrators must complete MFA before any admin function (NFR-17).
    pub require_admin_mfa: bool,
    /// Shown in authenticator apps next to the account name.
    pub mfa_issuer: String,
    /// Per-client request limits (NFR-20). `auth` covers sign-in, password reset and MFA, per IP address;
    /// `write` and `read` are per signed-in user; `ip` is a generous ceiling per IP address for everything.
    pub rate_limit_enabled: bool,
    pub rate_window_seconds: i64,
    pub rate_auth_per_window: i32,
    pub rate_write_per_window: i32,
    pub rate_read_per_window: i32,
    pub rate_ip_per_window: i32,
    /// Behind a trusted reverse proxy, take the client address from the last `X-Forwarded-For` entry.
    pub trust_proxy_headers: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            session_ttl_hours: 12,
            session_idle_minutes: 60,
            admin_session_idle_minutes: 15,
            max_failed_logins: 5,
            lockout_minutes: 15,
            reset_token_ttl_minutes: 30,
            dev_print_reset_tokens: false,
            return_window_days: 30,
            return_manager_limit_minor: 5_000_000,
            cashier_max_discount_bps: 1_000,
            // `Default` is the relaxed setting used by tests and local tinkering. `from_env` turns the
            // protections on unless told otherwise, which is what a real deployment gets.
            require_admin_mfa: false,
            mfa_issuer: "Store Management".into(),
            rate_limit_enabled: false,
            rate_window_seconds: 60,
            rate_auth_per_window: 10,
            rate_write_per_window: 120,
            rate_read_per_window: 600,
            rate_ip_per_window: 1_200,
            trust_proxy_headers: false,
        }
    }
}

fn var<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let d = Config::default();
        Config {
            session_ttl_hours: var("SESSION_TTL_HOURS", d.session_ttl_hours),
            session_idle_minutes: var("SESSION_IDLE_MINUTES", d.session_idle_minutes),
            admin_session_idle_minutes: var("ADMIN_SESSION_IDLE_MINUTES", d.admin_session_idle_minutes),
            max_failed_logins: var("MAX_FAILED_LOGINS", d.max_failed_logins),
            lockout_minutes: var("LOCKOUT_MINUTES", d.lockout_minutes),
            reset_token_ttl_minutes: var("RESET_TOKEN_TTL_MINUTES", d.reset_token_ttl_minutes),
            dev_print_reset_tokens: var("DEV_PRINT_RESET_TOKENS", d.dev_print_reset_tokens),
            return_window_days: var("RETURN_WINDOW_DAYS", d.return_window_days),
            return_manager_limit_minor: var("RETURN_MANAGER_LIMIT_MINOR", d.return_manager_limit_minor),
            cashier_max_discount_bps: var("CASHIER_MAX_DISCOUNT_BPS", d.cashier_max_discount_bps),
            require_admin_mfa: var("REQUIRE_ADMIN_MFA", true),
            mfa_issuer: env::var("MFA_ISSUER").unwrap_or(d.mfa_issuer),
            rate_limit_enabled: var("RATE_LIMIT_ENABLED", true),
            rate_window_seconds: var("RATE_WINDOW_SECONDS", d.rate_window_seconds),
            rate_auth_per_window: var("RATE_AUTH_PER_WINDOW", d.rate_auth_per_window),
            rate_write_per_window: var("RATE_WRITE_PER_WINDOW", d.rate_write_per_window),
            rate_read_per_window: var("RATE_READ_PER_WINDOW", d.rate_read_per_window),
            rate_ip_per_window: var("RATE_IP_PER_WINDOW", d.rate_ip_per_window),
            trust_proxy_headers: var("TRUST_PROXY_HEADERS", d.trust_proxy_headers),
        }
    }
}
