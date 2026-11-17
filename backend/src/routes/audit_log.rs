//! Просмотр журнала действий. Только владелец, только чтение.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Owner;
use crate::auth::extract::ApiQuery;
use crate::error::{ApiError, ApiResult, FieldErrors};

const DEFAULT_PER_PAGE: i64 = 50;
const MAX_PER_PAGE: i64 = 200;

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default, with = "time::serde::rfc3339::option")]
    from: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    to: Option<OffsetDateTime>,
    user_id: Option<Uuid>,
    warehouse_id: Option<Uuid>,
    action: Option<String>,
    page: Option<i64>,
    per_page: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AuditRow {
    id: Uuid,
    user_id: Option<Uuid>,
    user_full_name: String,
    device_id: Option<Uuid>,
    warehouse_id: Option<Uuid>,
    action: String,
    entity_type: Option<String>,
    entity_id: Option<Uuid>,
    details: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    synced_at: OffsetDateTime,
}

pub async fn list(
    State(state): State<AppState>,
    _owner: Owner,
    ApiQuery(query): ApiQuery<AuditQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(DEFAULT_PER_PAGE);

    let mut fields = FieldErrors::new();
    if !(1..=100_000).contains(&page) {
        fields.insert("page", "invalid");
    }
    if !(1..=MAX_PER_PAGE).contains(&per_page) {
        fields.insert("per_page", "invalid");
    }
    // Коды действий — латиница, цифры, точка и подчёркивание.
    let action_ok = |action: &str| {
        (1..=64).contains(&action.len())
            && action.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
    };
    if query.action.as_deref().is_some_and(|action| !action_ok(action)) {
        fields.insert("action", "invalid");
    }
    if query.from.zip(query.to).is_some_and(|(from, to)| from > to) {
        fields.insert("from", "after_to");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }

    // Фильтры необязательны: NULL в параметре означает «не фильтровать».
    // Фильтр по действию — точное совпадение или префикс до точки («user» найдёт «user.block»).
    const FILTER: &str = "($1::timestamptz IS NULL OR synced_at >= $1)
           AND ($2::timestamptz IS NULL OR synced_at < $2)
           AND ($3::uuid IS NULL OR user_id = $3)
           AND ($4::uuid IS NULL OR warehouse_id = $4)
           AND ($5::text IS NULL OR action = $5 OR starts_with(action, $5 || '.'))";

    let items: Vec<AuditRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, user_id, user_full_name, device_id, warehouse_id, action, entity_type, entity_id,
                details, occurred_at, synced_at
         FROM audit_log
         WHERE {FILTER}
         ORDER BY synced_at DESC, id
         LIMIT $6 OFFSET $7"
    )))
    .bind(query.from)
    .bind(query.to)
    .bind(query.user_id)
    .bind(query.warehouse_id)
    .bind(&query.action)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(&state.db)
    .await?;

    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM audit_log WHERE {FILTER}"
    )))
    .bind(query.from)
    .bind(query.to)
    .bind(query.user_id)
    .bind(query.warehouse_id)
    .bind(&query.action)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "items": items, "page": page, "per_page": per_page, "total": total })))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ArchiveRow {
    id: Uuid,
    dataset: String,
    period_kind: String,
    period_start: time::Date,
    row_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// `GET /audit-log/archives` — список архивов.
pub async fn archives(State(state): State<AppState>, _owner: Owner) -> ApiResult<Json<serde_json::Value>> {
    let items: Vec<ArchiveRow> = sqlx::query_as(
        "SELECT id, dataset, period_kind, period_start, row_count, created_at FROM audit_archives
         ORDER BY dataset, period_start DESC",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct ArchivePage {
    page: Option<i64>,
    per_page: Option<i64>,
}

/// `GET /audit-log/archives/{id}?page=` — записи архивного периода, только чтение.
/// Файл проверяется по контрольной сумме из реестра.
pub async fn archive(
    State(state): State<AppState>,
    _owner: Owner,
    crate::auth::extract::ApiPath(id): crate::auth::extract::ApiPath<Uuid>,
    ApiQuery(query): ApiQuery<ArchivePage>,
) -> ApiResult<Json<serde_json::Value>> {
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(DEFAULT_PER_PAGE);
    if !(1..=100_000).contains(&page) || !(1..=MAX_PER_PAGE).contains(&per_page) {
        return Err(ApiError::field("page", "invalid"));
    }
    let row: Option<(String, Vec<u8>, String)> =
        sqlx::query_as("SELECT file_name, sha256, dataset FROM audit_archives WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let (file_name, sha256, dataset) = row.ok_or(ApiError::NotFound)?;
    let lines = crate::archive::read_file(&state.config.archive_dir, &file_name, &sha256).await?;
    let total = lines.len();
    let start = usize::try_from((page - 1) * per_page).unwrap_or(usize::MAX);
    let items: Vec<serde_json::Value> = lines
        .iter()
        .skip(start)
        .take(usize::try_from(per_page).unwrap_or(0))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Ok(Json(json!({ "dataset": dataset, "items": items, "page": page, "per_page": per_page, "total": total })))
}
