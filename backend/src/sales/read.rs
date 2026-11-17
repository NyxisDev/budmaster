//! Чтение заказов и продаж для приложения (docs/design.md, раздел 12.4).
//! Свежие заказы и продажи приходят на устройство синхронизацией; здесь — поиск и старые.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::auth::ActiveUser;
use crate::auth::extract::{ApiPath, ApiQuery};
use crate::error::{ApiError, ApiResult, FieldErrors};

fn paging(page: Option<i64>, per_page: Option<i64>) -> ApiResult<(i64, i64)> {
    let page = page.unwrap_or(1);
    let per_page = per_page.unwrap_or(50);
    let mut fields = FieldErrors::new();
    if !(1..=10_000).contains(&page) {
        fields.insert("page", "invalid");
    }
    if !(1..=200).contains(&per_page) {
        fields.insert("per_page", "invalid");
    }
    if fields.is_empty() { Ok((page, per_page)) } else { Err(ApiError::Validation(fields)) }
}

#[derive(Deserialize)]
pub struct OrdersQuery {
    status: Option<String>,
    /// Номер заказа или цифры телефона.
    q: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    from: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    to: Option<OffsetDateTime>,
    page: Option<i64>,
    per_page: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct OrderListRow {
    id: Uuid,
    number: i64,
    source: String,
    status: String,
    customer_name: String,
    customer_phone: String,
    delivery_type: String,
    warehouse_id: Option<Uuid>,
    total: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// `GET /orders`
pub async fn list_orders(
    State(state): State<AppState>,
    _user: ActiveUser,
    ApiQuery(query): ApiQuery<OrdersQuery>,
) -> ApiResult<Json<Value>> {
    let (page, per_page) = paging(query.page, query.per_page)?;
    const STATUSES: [&str; 5] = ["new", "confirmed", "assembled", "completed", "cancelled"];
    if query.status.as_deref().is_some_and(|status| !STATUSES.contains(&status)) {
        return Err(ApiError::field("status", "invalid"));
    }
    let digits: Option<String> = query
        .q
        .as_deref()
        .map(|q| q.chars().filter(char::is_ascii_digit).take(13).collect::<String>())
        .filter(|digits| !digits.is_empty());

    const FILTER: &str = "FROM orders
        WHERE ($1::text IS NULL OR status::text = $1)
          AND ($2::timestamptz IS NULL OR created_at >= $2)
          AND ($3::timestamptz IS NULL OR created_at < $3)
          AND ($4::text IS NULL OR number::text = $4 OR customer_phone LIKE '%' || $4 || '%')";
    let items: Vec<OrderListRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, number, source::text, status::text, customer_name, customer_phone, delivery_type::text,
                warehouse_id, total::text, created_at
         {FILTER} ORDER BY created_at DESC, number DESC LIMIT $5 OFFSET $6"
    )))
    .bind(&query.status)
    .bind(query.from)
    .bind(query.to)
    .bind(&digits)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) {FILTER}")))
        .bind(&query.status)
        .bind(query.from)
        .bind(query.to)
        .bind(&digits)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!({ "items": items, "page": page, "per_page": per_page, "total": total })))
}

/// `GET /orders/{id}` — заказ со строками, историей статусов и доступным количеством.
///
/// `available` по каждой строке — сколько есть на каждом активном складе за вычетом резерва
/// других заказов. Это подсказка «на складе нет нужного количества (есть X из Y)» (раздел 9.5).
pub async fn get_order(
    State(state): State<AppState>,
    _user: ActiveUser,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let order: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'id', o.id, 'number', o.number, 'source', o.source, 'status', o.status,
            'customer_name', o.customer_name, 'customer_phone', o.customer_phone, 'comment', o.comment,
            'delivery_type', o.delivery_type, 'delivery_address', o.delivery_address,
            'warehouse_id', o.warehouse_id, 'total', o.total::text, 'cancel_reason', o.cancel_reason,
            'created_at', o.created_at,
            'sale_id', (SELECT s.id FROM sales s WHERE s.order_id = o.id),
            'items', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'id', i.id, 'product_id', i.product_id, 'product_name', p.name, 'stock_unit', i.stock_unit,
                    'qty', i.qty::text, 'price', i.price::text, 'amount', round(i.qty * i.price, 2)::text,
                    'available', COALESCE((
                        SELECT jsonb_agg(jsonb_build_object(
                            'warehouse_id', w.id,
                            'qty', (CASE WHEN i.stock_unit = 'unit'
                                         THEN COALESCE(b.qty_units, 0) - COALESCE(b.reserved_units, 0)
                                         ELSE COALESCE(b.qty_packs, 0) - COALESCE(b.reserved_packs, 0) END
                                    -- свой резерв этого заказа — не чужой
                                    + CASE WHEN o.status IN ('confirmed', 'assembled') AND o.warehouse_id = w.id
                                           THEN i.qty ELSE 0 END)::text
                        ) ORDER BY w.name)
                        FROM warehouses w
                        LEFT JOIN stock_balances b ON b.warehouse_id = w.id AND b.product_id = i.product_id
                        WHERE w.is_active
                    ), '[]'::jsonb)
                ) ORDER BY p.name)
                FROM order_items i JOIN products p ON p.id = i.product_id WHERE i.order_id = o.id
            ), '[]'::jsonb),
            'history', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'from', h.from_status, 'to', h.to_status, 'occurred_at', h.occurred_at,
                    'user_full_name', COALESCE(u.first_name || ' ' || u.last_name, 'Сайт')
                ) ORDER BY h.occurred_at, h.synced_at)
                FROM order_status_history h LEFT JOIN users u ON u.id = h.user_id WHERE h.order_id = o.id
            ), '[]'::jsonb))
         FROM orders o WHERE o.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(order.ok_or(ApiError::NotFound)?))
}

#[derive(Deserialize)]
pub struct SalesQuery {
    #[serde(default, with = "time::serde::rfc3339::option")]
    from: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    to: Option<OffsetDateTime>,
    page: Option<i64>,
    per_page: Option<i64>,
}

/// `GET /sales` — продажи, новые сверху. Себестоимости здесь нет.
pub async fn list_sales(
    State(state): State<AppState>,
    _user: ActiveUser,
    ApiQuery(query): ApiQuery<SalesQuery>,
) -> ApiResult<Json<Value>> {
    let (page, per_page) = paging(query.page, query.per_page)?;
    let items: Value = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(row ORDER BY (row->>'occurred_at') DESC), '[]'::jsonb) FROM (
            SELECT jsonb_build_object(
                'id', s.id, 'number', s.number, 'channel', s.channel, 'order_id', s.order_id,
                'warehouse_id', s.warehouse_id, 'payment_type', s.payment_type, 'total', s.total::text,
                'occurred_at', s.occurred_at) AS row
            FROM sales s
            WHERE ($1::timestamptz IS NULL OR s.occurred_at >= $1) AND ($2::timestamptz IS NULL OR s.occurred_at < $2)
            ORDER BY s.occurred_at DESC, s.number DESC
            LIMIT $3 OFFSET $4) t",
    )
    .bind(query.from)
    .bind(query.to)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "items": items, "page": page, "per_page": per_page })))
}

/// `GET /sales/{id}` — продажа со строками и тем, сколько по каждой уже вернули.
/// Себестоимость строк — только владельцу.
pub async fn get_sale(
    State(state): State<AppState>,
    ActiveUser(user): ActiveUser,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let sale: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'id', s.id, 'number', s.number, 'channel', s.channel, 'order_id', s.order_id,
            'warehouse_id', s.warehouse_id, 'payment_type', s.payment_type, 'total', s.total::text,
            'occurred_at', s.occurred_at,
            'items', COALESCE((
                SELECT jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
                    'id', i.id, 'product_id', i.product_id, 'product_name', p.name, 'stock_unit', i.stock_unit,
                    'qty', i.qty::text, 'price', i.price::text,
                    'unit_cost', CASE WHEN $2 THEN i.unit_cost::text END,
                    'returned_qty', COALESCE((SELECT sum(r.qty) FROM return_items r WHERE r.sale_item_id = i.id), 0)::text
                )) ORDER BY p.name)
                FROM sale_items i JOIN products p ON p.id = i.product_id WHERE i.sale_id = s.id
            ), '[]'::jsonb))
         FROM sales s WHERE s.id = $1",
    )
    .bind(id)
    .bind(user.is_owner())
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(sale.ok_or(ApiError::NotFound)?))
}
