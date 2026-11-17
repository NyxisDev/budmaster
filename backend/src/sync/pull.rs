//! Загрузка изменений на устройство (`GET /sync/pull`) и чтение движений (`GET /stock/movements`).
//!
//! Устройство хранит курсор — последний полученный `change_seq` — и спрашивает «всё, что
//! изменилось после него». Все таблицы читаются в одном снимке базы (REPEATABLE READ):
//! иначе запись, сделанная между двумя запросами, могла бы проскочить мимо курсора.
//! Номера `change_seq` выдаются под общей блокировкой до конца транзакции, поэтому в снимке
//! видны все номера меньше любого ещё не зафиксированного — курсор их не перепрыгнет.
//!
//! Удалённые записи приходят с `is_deleted: true` («надгробие»), чтобы устройство их убрало.
//! Список сотрудников, средняя себестоимость и журнал на устройство не отдаются.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::auth::ActiveUser;
use crate::auth::extract::ApiQuery;
use crate::error::{ApiError, ApiResult, FieldErrors};

const DEFAULT_LIMIT: i64 = 500;
const MAX_LIMIT: i64 = 2000;

#[derive(Deserialize)]
pub struct PullQuery {
    #[serde(default)]
    cursor: i64,
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct Change {
    #[serde(rename = "type")]
    kind: String,
    seq: i64,
    data: Value,
}

/// Изменения всех таблиц одним списком по возрастанию `change_seq`.
/// Деньги и количества — строками, как во всём API.
const CHANGES: &str = "
    SELECT 'warehouse' AS kind, change_seq AS seq, jsonb_build_object(
        'id', id, 'name', name, 'address', address, 'is_active', is_active) AS data
    FROM warehouses WHERE change_seq > $1
  UNION ALL
    SELECT 'category', change_seq, jsonb_build_object(
        'id', id, 'parent_id', parent_id, 'name', name, 'slug', slug,
        'sort_order', sort_order, 'is_deleted', is_deleted)
    FROM categories WHERE change_seq > $1
  UNION ALL
    SELECT 'product', change_seq, jsonb_build_object(
        'id', id, 'category_id', category_id, 'name', name, 'description', description,
        'unit_code', unit_code, 'price', price::text, 'pack_size', pack_size,
        'pack_price', pack_price::text, 'is_published', is_published, 'is_deleted', is_deleted)
    FROM products WHERE change_seq > $1
  UNION ALL
    SELECT 'product_image', change_seq, jsonb_build_object(
        'id', id, 'product_id', product_id,
        'url', $3 || '/media/products/' || file_name, 'thumb_url', $3 || '/media/products/' || thumb_name,
        'sort_order', sort_order, 'is_deleted', is_deleted)
    FROM product_images WHERE change_seq > $1
  UNION ALL
    SELECT 'supplier', change_seq, jsonb_build_object(
        'id', id, 'name', name, 'contact_person', contact_person, 'phone', phone,
        'note', note, 'is_deleted', is_deleted)
    FROM suppliers WHERE change_seq > $1
  UNION ALL
    SELECT 'stock_balance', change_seq, jsonb_build_object(
        'product_id', product_id, 'warehouse_id', warehouse_id,
        'qty_units', qty_units::text, 'qty_packs', qty_packs::text,
        'reserved_units', reserved_units::text, 'reserved_packs', reserved_packs::text)
    FROM stock_balances WHERE change_seq > $1
  UNION ALL
    -- Закупочные цены в строках поставки видит и работник: он сам вводит их с накладной.
    SELECT 'supply', s.change_seq, jsonb_build_object(
        'id', s.id, 'number', s.number, 'supplier_id', s.supplier_id, 'warehouse_id', s.warehouse_id,
        'status', s.status, 'expected_on', s.expected_on, 'note', s.note,
        'created_at', s.created_at, 'received_at', s.received_at,
        'items', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                'id', i.id, 'product_id', i.product_id, 'stock_unit', i.stock_unit,
                'expected_qty', i.expected_qty::text, 'received_qty', i.received_qty::text,
                'unit_cost', i.unit_cost::text) ORDER BY i.id)
            FROM supply_items i WHERE i.supply_id = s.id
        ), '[]'::jsonb))
    FROM supplies s WHERE s.change_seq > $1
  UNION ALL
    SELECT 'order', o.change_seq, jsonb_build_object(
        'id', o.id, 'number', o.number, 'source', o.source, 'status', o.status,
        'customer_name', o.customer_name, 'customer_phone', o.customer_phone, 'comment', o.comment,
        'delivery_type', o.delivery_type, 'delivery_address', o.delivery_address,
        'warehouse_id', o.warehouse_id, 'total', o.total::text, 'cancel_reason', o.cancel_reason,
        'created_at', o.created_at,
        'items', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                'id', i.id, 'product_id', i.product_id, 'stock_unit', i.stock_unit,
                'qty', i.qty::text, 'price', i.price::text) ORDER BY i.id)
            FROM order_items i WHERE i.order_id = o.id
        ), '[]'::jsonb))
    FROM orders o WHERE o.change_seq > $1
  UNION ALL
    -- Продажи — без себестоимости: она не уходит на устройства.
    SELECT 'sale', s.change_seq, jsonb_build_object(
        'id', s.id, 'number', s.number, 'channel', s.channel, 'order_id', s.order_id,
        'warehouse_id', s.warehouse_id, 'payment_type', s.payment_type, 'total', s.total::text,
        'occurred_at', s.occurred_at,
        'items', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                'id', i.id, 'product_id', i.product_id, 'stock_unit', i.stock_unit,
                'qty', i.qty::text, 'price', i.price::text) ORDER BY i.id)
            FROM sale_items i WHERE i.sale_id = s.id
        ), '[]'::jsonb))
    FROM sales s WHERE s.change_seq > $1
  ORDER BY seq
  LIMIT $2";

pub async fn pull(
    State(state): State<AppState>,
    _user: ActiveUser,
    ApiQuery(query): ApiQuery<PullQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    let mut fields = FieldErrors::new();
    if query.cursor < 0 {
        fields.insert("cursor", "invalid");
    }
    if !(1..=MAX_LIMIT).contains(&limit) {
        fields.insert("limit", "invalid");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }

    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;

    let changes: Vec<Change> = sqlx::query_as(CHANGES)
        .bind(query.cursor)
        .bind(limit)
        .bind(&state.config.public_base_url)
        .fetch_all(&mut *tx)
        .await?;

    // Справочник единиц не меняется через приложение; отдаётся при первой загрузке.
    let units: Option<Value> = if query.cursor == 0 {
        Some(
            sqlx::query_scalar(
                "SELECT COALESCE(jsonb_agg(jsonb_build_object(
                    'code', code, 'name', name, 'short_name', short_name, 'allow_fraction', allow_fraction)
                    ORDER BY code), '[]'::jsonb) FROM units",
            )
            .fetch_one(&mut *tx)
            .await?,
        )
    } else {
        None
    };
    tx.commit().await?;

    let cursor = changes.last().map_or(query.cursor, |change| change.seq);
    let has_more = i64::try_from(changes.len()).unwrap_or(i64::MAX) == limit;
    let mut body = json!({ "changes": changes, "cursor": cursor, "has_more": has_more });
    if let Some(units) = units {
        body["units"] = units;
    }
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct MovementsQuery {
    product_id: Uuid,
    warehouse_id: Option<Uuid>,
    page: Option<i64>,
    per_page: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct MovementRow {
    id: Uuid,
    product_id: Uuid,
    warehouse_id: Uuid,
    stock_unit: String,
    qty: String,
    kind: String,
    /// Себестоимость или закупочная цена — только для владельца.
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_cost: Option<String>,
    reason: Option<String>,
    user_full_name: String,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

/// `GET /stock/movements` — история движений товара, новые сверху.
pub async fn movements(
    State(state): State<AppState>,
    ActiveUser(user): ActiveUser,
    ApiQuery(query): ApiQuery<MovementsQuery>,
) -> ApiResult<Json<Value>> {
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(50);
    let mut fields = FieldErrors::new();
    if !(1..=10_000).contains(&page) {
        fields.insert("page", "invalid");
    }
    if !(1..=200).contains(&per_page) {
        fields.insert("per_page", "invalid");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }

    let mut items: Vec<MovementRow> = sqlx::query_as(
        "SELECT m.id, m.product_id, m.warehouse_id, m.stock_unit::text, m.qty::text, m.kind::text,
                m.unit_cost::text, m.reason, u.first_name || ' ' || u.last_name AS user_full_name, m.occurred_at
         FROM stock_movements m JOIN users u ON u.id = m.user_id
         WHERE m.product_id = $1 AND ($2::uuid IS NULL OR m.warehouse_id = $2)
         ORDER BY m.occurred_at DESC, m.seq DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(query.product_id)
    .bind(query.warehouse_id)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(&state.db)
    .await?;

    // Закупочную цену прихода работник и так видит в поставке; себестоимость списаний — нет.
    if !user.is_owner() {
        for item in &mut items {
            if item.kind != "receipt" {
                item.unit_cost = None;
            }
        }
    }
    Ok(Json(json!({ "items": items, "page": page, "per_page": per_page })))
}
