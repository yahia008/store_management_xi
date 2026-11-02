use axum::routing::{delete, get, post};
use axum::Router;

use crate::state::AppState;

mod accounts;
mod business;
mod employees;
mod session;
mod stores;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        // FR-01: set up the business and its first owner.
        .route("/api/v1/setup", post(business::setup))
        // FR-04: sign-in and passwords.
        .route("/api/v1/auth/login", post(session::login))
        .route("/api/v1/auth/logout", post(session::logout))
        .route("/api/v1/auth/password/forgot", post(accounts::forgot))
        .route("/api/v1/auth/password/reset", post(accounts::reset))
        .route("/api/v1/auth/password/change", post(accounts::change))
        .route("/api/v1/me", get(session::me))
        .route("/api/v1/roles", get(employees::roles))
        // FR-01, FR-03: business profile and settings.
        .route("/api/v1/business", get(business::get).patch(business::update))
        .route("/api/v1/business/settings", get(business::get_settings).put(business::put_settings))
        .route("/api/v1/business/tax-rates", get(business::list_tax).post(business::add_tax))
        // FR-02, FR-03: stores.
        .route("/api/v1/stores", get(stores::list).post(stores::create))
        .route("/api/v1/stores/{id}", get(stores::get).patch(stores::update))
        .route("/api/v1/stores/{id}/settings", get(stores::get_settings).put(stores::put_settings))
        // FR-04, FR-05: employees and roles.
        .route("/api/v1/employees", get(employees::list).post(employees::create))
        .route("/api/v1/employees/{id}", get(employees::get).patch(employees::update))
        .route("/api/v1/employees/{id}/roles", post(employees::grant))
        .route("/api/v1/employees/{id}/roles/{membership_id}", delete(employees::revoke))
        .route("/api/v1/employees/{id}/sessions/revoke", post(employees::revoke_sessions))
        .route("/api/v1/audit", get(employees::audit))
}
