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
        }
    }
}
