use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlx::PgPool;

use crate::config::Config;

/// How password-reset tokens reach people. Production plugs in email/SMS; nothing here sends mail.
#[async_trait]
pub trait ResetNotifier: Send + Sync {
    async fn send_reset(&self, email: &str, token: &str);
}

/// Logs that a reset was requested; prints the token only when `DEV_PRINT_RESET_TOKENS` is on.
pub struct LogNotifier {
    pub print_token: bool,
}

#[async_trait]
impl ResetNotifier for LogNotifier {
    async fn send_reset(&self, email: &str, token: &str) {
        if self.print_token {
            tracing::warn!("DEV reset token for {email}: {token}");
        } else {
            tracing::info!("password reset requested for {email} (no mail transport configured)");
        }
    }
}

/// Keeps tokens in memory so tests can finish a reset.
#[derive(Default)]
pub struct CapturingNotifier(pub Mutex<Vec<(String, String)>>);

#[async_trait]
impl ResetNotifier for CapturingNotifier {
    async fn send_reset(&self, email: &str, token: &str) {
        self.0.lock().unwrap().push((email.to_string(), token.to_string()));
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub notifier: Arc<dyn ResetNotifier>,
}
