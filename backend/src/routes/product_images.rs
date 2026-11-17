//! Фото товаров: загрузка, порядок, удаление.
//! Доступ — владелец или работник с правом «товары» (бит PRODUCTS_EDIT).

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::extract::{FromRef, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::AppState;
use crate::audit::{self, Entry};
use crate::auth::extract::{ApiJson, ApiPath};
use crate::auth::{ActiveUser, AuthUser};
use crate::catalog::images::{self, ImageError, MAX_IMAGES_PER_PRODUCT, StoredFiles};
use crate::catalog::public::ImageDto;
use crate::error::{ApiError, ApiResult};
use crate::permissions;

/// Сотрудник с правом править товары. Проверяется до того, как сервер начнёт читать
/// тело запроса: без права 10 МБ файла даже не принимаются.
pub struct ProductEditor(pub AuthUser);

impl<S> FromRequestParts<S> for ProductEditor
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ActiveUser(user) = ActiveUser::from_request_parts(parts, state).await?;
        if !user.can(permissions::PRODUCTS_EDIT) {
            return Err(ApiError::Forbidden("permission_required"));
        }
        Ok(Self(user))
    }
}

#[derive(Serialize)]
pub struct UploadedImage {
    id: Uuid,
    product_id: Uuid,
    #[serde(flatten)]
    urls: ImageDto,
    width: u32,
    height: u32,
    sort_order: i32,
}

/// Тело запроса — сам файл картинки (JPEG, PNG или WebP), до 10 МБ.
pub async fn upload(
    State(state): State<AppState>,
    ProductEditor(user): ProductEditor,
    ApiPath(product_id): ApiPath<Uuid>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<UploadedImage>)> {
    let body = body.map_err(|rejection| match rejection {
        BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => ApiError::PayloadTooLarge,
        _ => ApiError::BadRequest("invalid_body"),
    })?;
    if body.is_empty() {
        return Err(ApiError::field("image", "empty"));
    }

    // Товар и лимит фото проверяем до тяжёлой обработки, чтобы не тратить её впустую.
    // Окончательно лимит проверяется ещё раз под блокировкой, перед сохранением.
    {
        let mut conn = state.db.acquire().await?;
        image_count(&mut conn, product_id, false).await?;
    }

    let processed = images::process(body).await.map_err(|error| match error {
        ImageError::UnsupportedFormat => ApiError::field("image", "unsupported_format"),
        ImageError::TooLarge => ApiError::field("image", "dimensions_too_large"),
        ImageError::Corrupted => ApiError::field("image", "corrupted"),
        ImageError::Busy => ApiError::Busy,
        ImageError::Other(error) => ApiError::Internal(error),
    })?;

    let image_id = Uuid::new_v4();
    let (file_name, thumb_name) = images::file_names(image_id);
    let dir = images::products_dir(&state.config.media_dir);

    // Файлы пишутся до транзакции: запись на диск не держит блокировки базы. Если дальше
    // что-то пойдёт не так — ошибка, отказ по лимиту или обрыв запроса, — `stored` при выходе
    // из функции удалит файлы. Остаются они только после успешной записи в базу.
    let stored = StoredFiles::new(&dir, &[&file_name, &thumb_name]);
    images::store(&dir, &file_name, &processed.full)
        .await
        .map_err(|error| ApiError::Internal(anyhow::Error::new(error).context("не удалось сохранить фото")))?;
    images::store(&dir, &thumb_name, &processed.thumb)
        .await
        .map_err(|error| ApiError::Internal(anyhow::Error::new(error).context("не удалось сохранить фото")))?;

    let mut tx = state.db.begin().await?;
    // Сначала общая блокировка изменений, как у всех пишущих транзакций: иначе загрузка фото
    // и правка того же товара с устройства могли бы ждать друг друга бесконечно.
    crate::sync::lock_changes(&mut tx).await?;
    // Строка товара блокируется: одновременные загрузки не превысят лимит фото вместе.
    let (count, max_sort) = image_count(&mut tx, product_id, true).await?;
    if count >= MAX_IMAGES_PER_PRODUCT {
        return Err(ApiError::field("image", "limit_reached"));
    }
    let sort_order = max_sort.map_or(0, |max| max.saturating_add(1));

    sqlx::query(
        "INSERT INTO product_images (id, product_id, file_name, thumb_name, width, height, sort_order, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(image_id)
    .bind(product_id)
    .bind(&file_name)
    .bind(&thumb_name)
    .bind(i32::try_from(processed.width).unwrap_or(i32::MAX))
    .bind(i32::try_from(processed.height).unwrap_or(i32::MAX))
    .bind(sort_order)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        Entry::new(&user.actor(), "product.image_add")
            .entity("product", product_id)
            .details(json!({ "image_id": image_id })),
    )
    .await?;
    tx.commit().await?;
    stored.keep();

    Ok((
        StatusCode::CREATED,
        Json(UploadedImage {
            id: image_id,
            product_id,
            urls: ImageDto::new(&state.config.public_base_url, &file_name, &thumb_name),
            width: processed.width,
            height: processed.height,
            sort_order,
        }),
    ))
}

/// Число фото товара и наибольший номер порядка. Товара нет или он удалён — 404,
/// лимит фото уже набран — 422. `lock` — заблокировать строку товара до конца транзакции.
async fn image_count(conn: &mut PgConnection, product_id: Uuid, lock: bool) -> ApiResult<(i64, Option<i32>)> {
    let query = if lock {
        "SELECT is_deleted FROM products WHERE id = $1 FOR UPDATE"
    } else {
        "SELECT is_deleted FROM products WHERE id = $1"
    };
    let deleted: Option<bool> = sqlx::query_scalar(query).bind(product_id).fetch_optional(&mut *conn).await?;
    if deleted != Some(false) {
        return Err(ApiError::NotFound);
    }
    let (count, max_sort): (i64, Option<i32>) = sqlx::query_as(
        "SELECT count(*), max(sort_order) FROM product_images WHERE product_id = $1 AND NOT is_deleted",
    )
    .bind(product_id)
    .fetch_one(&mut *conn)
    .await?;
    if count >= MAX_IMAGES_PER_PRODUCT {
        return Err(ApiError::field("image", "limit_reached"));
    }
    Ok((count, max_sort))
}

#[derive(Deserialize)]
pub struct ReorderRequest {
    sort_order: i32,
}

/// Меняет место фото в карточке товара. Первым показывается фото с наименьшим номером.
pub async fn reorder(
    State(state): State<AppState>,
    ProductEditor(user): ProductEditor,
    ApiPath((product_id, image_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<ReorderRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    if !(0..=1000).contains(&body.sort_order) {
        return Err(ApiError::field("sort_order", "invalid"));
    }

    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE product_images SET sort_order = $3 WHERE id = $2 AND product_id = $1 AND NOT is_deleted",
    )
    .bind(product_id)
    .bind(image_id)
    .bind(body.sort_order)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit::record(
        &mut *tx,
        Entry::new(&user.actor(), "product.image_reorder")
            .entity("product", product_id)
            .details(json!({ "image_id": image_id, "sort_order": body.sort_order })),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

/// Убирает фото. Запись в базе помечается удалённой (история остаётся), файлы стираются с диска.
pub async fn remove(
    State(state): State<AppState>,
    ProductEditor(user): ProductEditor,
    ApiPath((product_id, image_id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut tx = state.db.begin().await?;
    let names: Option<(String, String)> = sqlx::query_as(
        "UPDATE product_images SET is_deleted = true
         WHERE id = $2 AND product_id = $1 AND NOT is_deleted
         RETURNING file_name, thumb_name",
    )
    .bind(product_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (file_name, thumb_name) = names.ok_or(ApiError::NotFound)?;
    audit::record(
        &mut *tx,
        Entry::new(&user.actor(), "product.image_remove")
            .entity("product", product_id)
            .details(json!({ "image_id": image_id })),
    )
    .await?;
    tx.commit().await?;

    images::remove(&images::products_dir(&state.config.media_dir), &[&file_name, &thumb_name]).await;
    Ok(Json(json!({ "ok": true })))
}
