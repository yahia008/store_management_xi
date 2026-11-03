pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod inventory;
pub mod permissions;
pub mod routes;
pub mod security;
pub mod state;

use std::sync::Arc;

use axum::Router;
use sqlx::PgPool;

pub use config::Config;
pub use state::{AppState, CapturingNotifier, LogNotifier, ResetNotifier};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub fn app(pool: PgPool, config: Config, notifier: Arc<dyn ResetNotifier>) -> Router {
    let state = AppState { pool, config: Arc::new(config), notifier };
    routes::router().with_state(state)
}
