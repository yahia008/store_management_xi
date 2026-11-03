use std::sync::Arc;

use store_api::{app, Config, LogNotifier, MIGRATOR};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let config = Config::from_env();

    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(&database_url).await.expect("cannot connect to the database");
    MIGRATOR.run(&pool).await.expect("migrations failed");

    let notifier = Arc::new(LogNotifier { print_token: config.dev_print_reset_tokens });
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("cannot bind");
    tracing::info!("listening on {bind}");
    axum::serve(listener, app(pool, config, notifier).into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(shutdown()).await.expect("server error");
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
