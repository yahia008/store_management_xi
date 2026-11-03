#![allow(dead_code)]
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use store_api::{app, CapturingNotifier, Config};
use tower::ServiceExt;
use uuid::Uuid;

pub const PASSWORD: &str = "correct-horse-42";

pub struct Api {
    pub router: Router,
    pub pool: PgPool,
    pub notifier: Arc<CapturingNotifier>,
}

pub struct Resp {
    pub status: StatusCode,
    pub body: Value,
}

impl Resp {
    pub fn id(&self, key: &str) -> Uuid {
        self.body[key].as_str().unwrap_or_else(|| panic!("no {key} in {}", self.body)).parse().unwrap()
    }
}

impl Api {
    pub fn new(pool: PgPool) -> Api {
        Api::with_config(pool, Config::default())
    }

    pub fn with_config(pool: PgPool, config: Config) -> Api {
        let notifier = Arc::new(CapturingNotifier::default());
        Api { router: app(pool.clone(), config, notifier.clone()), pool, notifier }
    }

    pub async fn call(&self, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> Resp {
        self.call_h(method, path, token, body, &[]).await
    }

    /// POST with an Idempotency-Key header (sales and returns).
    pub async fn post_idem(&self, path: &str, token: &str, key: &str, body: Value) -> Resp {
        self.call_h(Method::POST, path, Some(token), Some(body), &[("idempotency-key", key)]).await
    }

    pub async fn call_h(&self, method: Method, path: &str, token: Option<&str>, body: Option<Value>, headers: &[(&str, &str)]) -> Resp {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())).unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp { status, body }
    }

    pub async fn get(&self, path: &str, token: &str) -> Resp {
        self.call(Method::GET, path, Some(token), None).await
    }
    pub async fn post(&self, path: &str, token: &str, body: Value) -> Resp {
        self.call(Method::POST, path, Some(token), Some(body)).await
    }
    pub async fn put(&self, path: &str, token: &str, body: Value) -> Resp {
        self.call(Method::PUT, path, Some(token), Some(body)).await
    }
    pub async fn patch(&self, path: &str, token: &str, body: Value) -> Resp {
        self.call(Method::PATCH, path, Some(token), Some(body)).await
    }
    pub async fn delete(&self, path: &str, token: &str) -> Resp {
        self.call(Method::DELETE, path, Some(token), None).await
    }

    pub async fn login(&self, email: &str, password: &str) -> Resp {
        self.call(Method::POST, "/api/v1/auth/login", None, Some(json!({ "email": email, "password": password }))).await
    }

    pub async fn token(&self, email: &str) -> String {
        let r = self.login(email, PASSWORD).await;
        assert_eq!(r.status, StatusCode::OK, "login {email}: {}", r.body);
        r.body["token"].as_str().unwrap().to_string()
    }

    /// Sets up the business and returns the owner's token.
    pub async fn setup(&self) -> String {
        let r = self
            .call(
                Method::POST,
                "/api/v1/setup",
                None,
                Some(json!({
                    "legal_name": "Amiable Retail Ltd", "base_currency": "NGN", "timezone": "Africa/Lagos",
                    "owner_email": "owner@amiable.test", "owner_name": "Ada Owner", "owner_password": PASSWORD,
                })),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        self.token("owner@amiable.test").await
    }

    pub async fn store(&self, owner: &str, name: &str) -> Uuid {
        let r = self.post("/api/v1/stores", owner, json!({ "name": name })).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        r.id("id")
    }

    /// Creates an employee with one role and returns (user id, token).
    pub async fn employee(&self, owner: &str, email: &str, role: &str, store: Option<Uuid>) -> (Uuid, String) {
        let r = self
            .post("/api/v1/employees", owner, json!({ "email": email, "full_name": email, "password": PASSWORD, "role": role, "store_id": store }))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        (r.id("id"), self.token(email).await)
    }

    pub async fn audit_count(&self, action: &str, outcome: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1 AND outcome = $2").bind(action).bind(outcome).fetch_one(&self.pool).await.unwrap()
    }
}
