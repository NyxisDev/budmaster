//! Маршруты API. Базовый путь — `/api/v1`.

pub mod audit_log;
pub mod auth;
pub mod users;

use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::AppState;
use crate::auth::ActiveUser;
use crate::error::{ApiError, ApiResult};

/// Запросы этого этапа — небольшие JSON; всё крупнее отклоняется до разбора.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/change-password", post(auth::change_password))
        .route("/me", get(auth::me))
        .route("/warehouses", get(list_warehouses))
        .route("/users", get(users::list).post(users::create))
        .route("/users/{id}", patch(users::update))
        .route("/users/{id}/block", post(users::block))
        .route("/users/{id}/unblock", post(users::unblock))
        .route("/users/{id}/reset-password", post(users::reset_password))
        .route("/audit-log", get(audit_log::list))
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .fallback(|| async { ApiError::NotFound });

    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", api)
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(30)))
        .layer(cors_layer(&state.config.cors_origins))
        // В журнал сервера идут метод, путь и код ответа. Тела запросов и заголовки
        // (а значит, пароли и токены) туда не попадают.
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Запросы из браузера разрешены только перечисленным сайтам. Список пуст — никому.
fn cors_layer(origins: &[String]) -> CorsLayer {
    let allowed: Vec<HeaderValue> = origins.iter().filter_map(|origin| origin.parse().ok()).collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods([Method::GET, Method::POST, Method::PATCH])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .max_age(Duration::from_secs(3600))
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map(|_| Json(json!({ "status": "ok" })))
        .map_err(|error| {
            tracing::error!(?error, "база данных недоступна");
            StatusCode::SERVICE_UNAVAILABLE
        })
}

#[derive(Serialize, sqlx::FromRow)]
struct WarehouseDto {
    id: Uuid,
    name: String,
    address: Option<String>,
    is_active: bool,
}

/// Список складов — нужен уже сейчас, чтобы выбрать склад работнику.
async fn list_warehouses(
    State(state): State<AppState>,
    _user: ActiveUser,
) -> ApiResult<Json<serde_json::Value>> {
    let items: Vec<WarehouseDto> =
        sqlx::query_as("SELECT id, name, address, is_active FROM warehouses ORDER BY name")
            .fetch_all(&state.db)
            .await?;
    Ok(Json(json!({ "items": items })))
}
