//! Публичное API каталога для сайта (docs/design.md, раздел 12.1). Без входа, только чтение.
//!
//! Наружу отдаётся только то, что можно видеть любому: нет остатков в цифрах, складов,
//! закупочных цен и служебных полей. Скрытые и удалённые товары и товары из удалённых
//! категорий не показываются вовсе.

use axum::Json;
use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::AppState;
use crate::auth::extract::{ApiPath, ApiQuery};
use crate::error::{ApiError, ApiResult, FieldErrors};

const DEFAULT_PER_PAGE: i64 = 12;
const MAX_PER_PAGE: i64 = 48;
const MAX_QUERY_CHARS: usize = 100;

/// Категории, видимые на сайте: не удалённые и без удалённых предков.
pub const VISIBLE_CATEGORIES: &str = "visible AS (
        SELECT id, parent_id, name, slug, sort_order FROM categories
        WHERE parent_id IS NULL AND NOT is_deleted
      UNION ALL
        SELECT c.id, c.parent_id, c.name, c.slug, c.sort_order
        FROM categories c JOIN visible v ON c.parent_id = v.id
        WHERE NOT c.is_deleted
    )";

const PRODUCT_COLUMNS: &str = "p.id, p.name, p.description, p.category_id,
        u.code AS unit_code, u.short_name AS unit_short_name, u.allow_fraction AS unit_allow_fraction,
        p.price::text AS price, p.pack_size, p.pack_price::text AS pack_price,
        product_availability(p.id) AS availability,
        COALESCE((
            SELECT json_agg(json_build_object('file', i.file_name, 'thumb', i.thumb_name)
                            ORDER BY i.sort_order, i.created_at, i.id)
            FROM product_images i WHERE i.product_id = p.id AND NOT i.is_deleted
        ), '[]'::json) AS images";

#[derive(Serialize, sqlx::FromRow)]
struct CategoryDto {
    id: Uuid,
    parent_id: Option<Uuid>,
    name: String,
    slug: String,
    sort_order: i32,
}

pub async fn categories(State(state): State<AppState>) -> ApiResult<Response> {
    let items: Vec<CategoryDto> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE {VISIBLE_CATEGORIES}
         SELECT id, parent_id, name, slug, sort_order FROM visible ORDER BY sort_order, name, id"
    )))
    .fetch_all(&state.db)
    .await?;
    Ok(cached(Json(json!({ "items": items }))))
}

#[derive(sqlx::FromRow)]
struct ProductRow {
    id: Uuid,
    name: String,
    description: String,
    category_id: Uuid,
    unit_code: String,
    unit_short_name: String,
    unit_allow_fraction: bool,
    price: String,
    pack_size: Option<i32>,
    pack_price: Option<String>,
    availability: String,
    images: SqlJson<Vec<ImageFiles>>,
}

#[derive(Deserialize)]
struct ImageFiles {
    file: String,
    thumb: String,
}

#[derive(Serialize)]
struct ProductDto {
    id: Uuid,
    name: String,
    description: String,
    category_id: Uuid,
    unit: UnitDto,
    price: String,
    pack: Option<PackDto>,
    images: Vec<ImageDto>,
    availability: String,
}

#[derive(Serialize)]
struct UnitDto {
    code: String,
    short_name: String,
    allow_fraction: bool,
}

#[derive(Serialize)]
struct PackDto {
    size: i32,
    price: String,
}

#[derive(Serialize)]
pub struct ImageDto {
    pub url: String,
    pub thumb_url: String,
}

impl ImageDto {
    /// Сайт и сервер живут на разных адресах, поэтому ссылки на фото — полные.
    pub fn new(public_base_url: &str, file_name: &str, thumb_name: &str) -> Self {
        Self {
            url: format!("{public_base_url}/media/products/{file_name}"),
            thumb_url: format!("{public_base_url}/media/products/{thumb_name}"),
        }
    }
}

impl ProductRow {
    fn into_dto(self, public_base_url: &str) -> ProductDto {
        ProductDto {
            id: self.id,
            name: self.name,
            description: self.description,
            category_id: self.category_id,
            unit: UnitDto {
                code: self.unit_code,
                short_name: self.unit_short_name,
                allow_fraction: self.unit_allow_fraction,
            },
            price: self.price,
            pack: self.pack_size.zip(self.pack_price).map(|(size, price)| PackDto { size, price }),
            images: self.images.0.iter().map(|i| ImageDto::new(public_base_url, &i.file, &i.thumb)).collect(),
            availability: self.availability,
        }
    }
}

#[derive(Deserialize)]
pub struct ProductsQuery {
    category_id: Option<Uuid>,
    q: Option<String>,
    page: Option<i64>,
    per_page: Option<i64>,
}

pub async fn products(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ProductsQuery>,
) -> ApiResult<Response> {
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(DEFAULT_PER_PAGE);
    let search = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty());

    let mut fields = FieldErrors::new();
    if !(1..=10_000).contains(&page) {
        fields.insert("page", "invalid");
    }
    if !(1..=MAX_PER_PAGE).contains(&per_page) {
        fields.insert("per_page", "invalid");
    }
    if search.is_some_and(|q| q.chars().count() > MAX_QUERY_CHARS || q.chars().any(char::is_control)) {
        fields.insert("q", "invalid");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }
    let pattern = search.map(like_pattern);

    // Фильтр по категории включает все её подкатегории. Неизвестная или скрытая категория
    // даёт пустой список, а не весь каталог.
    const FILTER: &str = "FROM products p
         JOIN units u ON u.code = p.unit_code
         WHERE NOT p.is_deleted AND p.is_published
           AND p.category_id IN (SELECT id FROM visible)
           AND ($1::uuid IS NULL OR p.category_id IN (SELECT id FROM subtree))
           AND ($2::text IS NULL OR p.name ILIKE $2 ESCAPE '\\' OR p.description ILIKE $2 ESCAPE '\\')";
    const SUBTREE: &str = "subtree AS (
            SELECT id FROM visible WHERE id = $1
          UNION ALL
            SELECT v.id FROM visible v JOIN subtree s ON v.parent_id = s.id
        )";

    let rows: Vec<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE {VISIBLE_CATEGORIES}, {SUBTREE}
         SELECT {PRODUCT_COLUMNS} {FILTER}
         ORDER BY p.name, p.id
         LIMIT $3 OFFSET $4"
    )))
    .bind(query.category_id)
    .bind(&pattern)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(&state.db)
    .await?;

    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE {VISIBLE_CATEGORIES}, {SUBTREE} SELECT count(*) {FILTER}"
    )))
    .bind(query.category_id)
    .bind(&pattern)
    .fetch_one(&state.db)
    .await?;

    let base = &state.config.public_base_url;
    let items: Vec<ProductDto> = rows.into_iter().map(|row| row.into_dto(base)).collect();
    Ok(cached(Json(json!({ "items": items, "page": page, "per_page": per_page, "total": total }))))
}

pub async fn product(State(state): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Response> {
    let row: Option<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE {VISIBLE_CATEGORIES}
         SELECT {PRODUCT_COLUMNS}
         FROM products p JOIN units u ON u.code = p.unit_code
         WHERE p.id = $1 AND NOT p.is_deleted AND p.is_published
           AND p.category_id IN (SELECT id FROM visible)"
    )))
    .bind(id)
    .fetch_optional(&state.db)
    .await?;

    let row = row.ok_or(ApiError::NotFound)?;
    Ok(cached(Json(row.into_dto(&state.config.public_base_url))))
}

/// Шаблон для ILIKE «содержит текст». Знаки `%`, `_` и `\` в запросе ищутся буквально,
/// а не работают как подстановочные.
fn like_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// Каталог меняется редко: минуту ответ можно держать в кэше браузера.
fn cached(body: impl IntoResponse) -> Response {
    ([(CACHE_CONTROL, "public, max-age=60")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::like_pattern;

    #[test]
    fn wildcards_in_search_are_literal() {
        assert_eq!(like_pattern("цемент"), "%цемент%");
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b\\c"), "%a\\_b\\\\c%");
    }
}
