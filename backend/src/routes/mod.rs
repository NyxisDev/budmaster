//! Маршруты API. Базовый путь — `/api/v1`.

pub mod audit_log;
pub mod auth;
pub mod product_images;
pub mod users;

use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::routing::{get, patch, post};
use tower_http::services::ServeDir;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::AppState;
use crate::auth::ActiveUser;
use crate::catalog::{images, public};
use crate::{sales, sync};
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
        .route("/audit-log/archives", get(audit_log::archives))
        .route("/audit-log/archives/{id}", get(audit_log::archive))
        .route("/public/categories", get(public::categories))
        .route("/public/products", get(public::products))
        .route("/public/products/{id}", get(public::product))
        .route(
            "/products/{id}/images/{image_id}",
            patch(product_images::reorder).delete(product_images::remove),
        )
        .route("/sync/pull", get(sync::pull::pull))
        .route("/orders", get(sales::read::list_orders))
        .route("/orders/{id}", get(sales::read::get_order))
        .route("/sales", get(sales::read::list_sales))
        .route("/sales/{id}", get(sales::read::get_sale))
        .route("/reports/summary", get(sales::reports::summary))
        .route("/reports/products", get(sales::reports::products))
        .route("/public/orders", post(sales::public::create))
        .route("/stock/movements", get(sync::pull::movements))
        // Обычный запрос должен уложиться в 30 секунд. Действует на маршруты, объявленные выше.
        .route_layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(30)))
        // Пачка операций с устройства: до 2 МБ и 2 минуты на обработку.
        .route(
            "/sync/push",
            post(sync::push).layer(
                tower::ServiceBuilder::new()
                    .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(120)))
                    .layer(DefaultBodyLimit::max(sync::MAX_PUSH_BYTES)),
            ),
        )
        .route(
            "/sync/drain",
            post(sync::drain).layer(
                tower::ServiceBuilder::new()
                    .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(120)))
                    .layer(DefaultBodyLimit::max(sync::MAX_PUSH_BYTES)),
            ),
        )
        // Загрузка фото: лимит тела 10 МБ вместо 64 КБ и 3 минуты на запрос —
        // 10 МБ по медленной мобильной сети в 30 секунд не успевают.
        .route(
            "/products/{id}/images",
            post(product_images::upload).layer(
                tower::ServiceBuilder::new()
                    .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(180)))
                    .layer(DefaultBodyLimit::max(images::MAX_UPLOAD_BYTES)),
            ),
        )
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .fallback(|| async { ApiError::NotFound });

    // Фото отдаются как файлы. В папке лежат только JPEG, созданные самим сервером.
    let media = ServeDir::new(&state.config.media_dir).append_index_html_on_directories(false);
    let media = Router::new().fallback_service(media).layer(axum::middleware::map_response(media_response));

    Router::new()
        .route("/health", get(health))
        .nest("/media", media)
        .nest("/api/v1", api)
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors_layer(&state.config.cors_origins))
        // В журнал сервера идут метод, путь и код ответа. Тела запросов и заголовки
        // (а значит, пароли и токены) туда не попадают.
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Заголовки для фото. nosniff запрещает браузеру трактовать файл как что-то, кроме картинки.
/// Кэш — на сутки, а не навсегда: убранное фото не должно жить в кэшах браузеров год.
/// Любой отказ (нет файла, не тот метод) отдаётся в общем формате ошибок и не кэшируется.
async fn media_response(response: axum::response::Response) -> axum::response::Response {
    use axum::response::IntoResponse;

    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_MODIFIED || status == StatusCode::PARTIAL_CONTENT {
        let mut response = response;
        let headers = response.headers_mut();
        headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
        return response;
    }
    let error = if status == StatusCode::METHOD_NOT_ALLOWED { ApiError::MethodNotAllowed } else { ApiError::NotFound };
    let mut response = error.into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Запросы из браузера разрешены только перечисленным сайтам. Список пуст — никому.
fn cors_layer(origins: &[String]) -> CorsLayer {
    let allowed: Vec<HeaderValue> = origins.iter().filter_map(|origin| origin.parse().ok()).collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
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
