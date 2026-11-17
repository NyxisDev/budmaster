//! Общие помощники тестов. Каждый тест получает свою пустую базу с применёнными миграциями.
//!
//! Сервер в тестах ходит в базу под ролью `budmaster_app` — той же, что в работе,
//! поэтому тесты ловят и нехватку прав, и лишние права.

#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use budmaster_server::cli::{self, NewOwner};
use budmaster_server::config::Config;
use budmaster_server::{AppState, routes};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

pub const OWNER_PASSWORD: &str = "owner-password-1";
pub const WORKER_PASSWORD: &str = "worker-password-1";

pub struct TestApp {
    router: Router,
    /// Соединения под ролью-владельцем базы: для подготовки данных и проверок.
    pub admin: PgPool,
    /// Соединения под ролью сервера.
    pub app_db: PgPool,
    pub warehouse_id: Uuid,
    pub media_dir: std::path::PathBuf,
}

pub struct Session {
    pub access: String,
    pub refresh: String,
    pub device_id: Uuid,
    pub user_id: Uuid,
}

pub struct Response {
    pub status: StatusCode,
    pub body: Value,
    pub retry_after: Option<String>,
}

impl Response {
    /// Код ошибки из тела ответа.
    pub fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or_default()
    }
}

impl TestApp {
    pub async fn new(pool_opts: PgPoolOptions, conn_opts: PgConnectOptions) -> Self {
        let admin = pool_opts.connect_with(conn_opts.clone()).await.expect("база для тестов");
        let app_db = PgPoolOptions::new()
            .max_connections(5)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    conn.execute("SET ROLE budmaster_app").await?;
                    Ok(())
                })
            })
            .connect_with(conn_opts)
            .await
            .expect("соединение под ролью сервера");

        // У каждого теста своя папка для фото.
        let media_dir = std::env::temp_dir().join(format!("budmaster-test-{}", Uuid::new_v4()));
        let config = Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            jwt_secret: b"test-secret-test-secret-test-secret!".to_vec(),
            access_ttl_secs: 900,
            refresh_ttl_days: 60,
            trust_proxy: false,
            cors_origins: vec![],
            public_base_url: "http://api.test".to_owned(),
            media_dir: media_dir.clone(),
        };
        let state = AppState::new(app_db.clone(), config).unwrap();
        let warehouse_id = cli::create_warehouse(&app_db, "Склад 1", None).await.unwrap();

        Self { router: routes::router(state), admin, app_db, warehouse_id, media_dir }
    }

    pub async fn request(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> Response {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        self.raw(request).await
    }

    /// Отправляет заранее собранный запрос (для проверки битых тел).
    pub async fn raw(&self, request: Request<Body>) -> Response {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let retry_after =
            response.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Response { status, body, retry_after }
    }

    /// Сам маршрутизатор — для запросов, которым нужен разбор заголовков ответа.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub async fn get(&self, path: &str, token: &str) -> Response {
        self.request("GET", path, Some(token), None).await
    }

    pub async fn post(&self, path: &str, token: &str, body: Value) -> Response {
        self.request("POST", path, Some(token), Some(body)).await
    }

    pub async fn create_owner(&self, login: &str) -> Uuid {
        cli::create_owner(
            &self.app_db,
            NewOwner {
                login: login.to_owned(),
                first_name: "Олена".to_owned(),
                last_name: "Власник".to_owned(),
                password: OWNER_PASSWORD.to_owned(),
            },
        )
        .await
        .unwrap()
    }

    pub async fn try_login(&self, login: &str, password: &str, device_id: Uuid) -> Response {
        self.request(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({ "login": login, "password": password, "device_id": device_id, "device_model": "Test" })),
        )
        .await
    }

    pub async fn login(&self, login: &str, password: &str) -> Session {
        let device_id = Uuid::new_v4();
        let response = self.try_login(login, password, device_id).await;
        assert_eq!(response.status, StatusCode::OK, "вход {login}: {}", response.body);
        Session {
            access: response.body["access_token"].as_str().unwrap().to_owned(),
            refresh: response.body["refresh_token"].as_str().unwrap().to_owned(),
            device_id,
            user_id: response.body["user"]["id"].as_str().unwrap().parse().unwrap(),
        }
    }

    /// Владелец, уже вошедший в систему.
    pub async fn owner(&self) -> Session {
        self.create_owner("owner").await;
        self.login("owner", OWNER_PASSWORD).await
    }

    /// Создаёт работника через API. Возвращает его id.
    pub async fn create_worker(&self, owner: &Session, login: &str, permissions: i64) -> Uuid {
        let response = self
            .post(
                "/api/v1/users",
                &owner.access,
                json!({
                    "login": login,
                    "password": WORKER_PASSWORD,
                    "first_name": "Іван",
                    "last_name": "Працівник",
                    "default_warehouse_id": self.warehouse_id,
                    "permissions": permissions,
                }),
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
        response.body["id"].as_str().unwrap().parse().unwrap()
    }

    /// Работник, который уже сменил временный пароль и может работать.
    pub async fn active_worker(&self, owner: &Session, login: &str) -> Session {
        self.create_worker(owner, login, 0).await;
        let first = self.login(login, WORKER_PASSWORD).await;
        let response = self
            .post(
                "/api/v1/auth/change-password",
                &first.access,
                json!({ "current_password": WORKER_PASSWORD, "new_password": "worker-password-2" }),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        Session {
            access: response.body["access_token"].as_str().unwrap().to_owned(),
            refresh: response.body["refresh_token"].as_str().unwrap().to_owned(),
            device_id: first.device_id,
            user_id: first.user_id,
        }
    }

    pub async fn audit_count(&self, action: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
            .bind(action)
            .fetch_one(&self.admin)
            .await
            .unwrap()
    }
}
