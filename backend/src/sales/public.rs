//! Заявка с сайта: `POST /api/v1/public/orders` (docs/design.md, раздел 12.1).
//!
//! Цены берутся только с сервера, товары — только видимые на сайте. Повторная отправка
//! с тем же `id` возвращает уже созданный заказ. От спама: лимит частоты (с адреса и на номер),
//! скрытое поле-ловушка и минимальное время заполнения формы.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::orders::{MAX_LINES, NewOrder, OrderLine, Source, insert_order};
use super::{DeliveryType, check_customer};
use crate::AppState;
use crate::audit::{self, Actor, Entry};
use crate::auth::ClientIp;
use crate::auth::extract::ApiJson;
use crate::catalog::public::VISIBLE_CATEGORIES;
use crate::error::{ApiError, ApiResult};
use crate::sync::values::{StockUnit, parse_qty};

/// Быстрее человек форму не заполнит.
const MIN_FILL_MS: i64 = 2000;
/// Больше этого в одной строке заявки не бывает (docs/design.md, 12.1).
const MAX_QTY: f64 = 99_999.0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderItem {
    product_id: Uuid,
    stock_unit: StockUnit,
    /// Количество — числом или строкой; не больше 3 знаков после точки.
    qty: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderRequest {
    id: Uuid,
    customer_name: String,
    customer_phone: String,
    #[serde(default)]
    comment: Option<String>,
    delivery_type: DeliveryType,
    #[serde(default)]
    delivery_address: Option<String>,
    items: Vec<OrderItem>,
    /// Поле-ловушка: человек его не видит и не заполняет.
    #[serde(default)]
    website: Option<String>,
    /// Сколько миллисекунд форма была открыта.
    #[serde(default)]
    elapsed_ms: Option<i64>,
}

pub async fn create(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    ApiJson(request): ApiJson<OrderRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if request.website.as_deref().is_some_and(|text| !text.is_empty()) {
        return Err(ApiError::field("website", "invalid"));
    }
    if request.elapsed_ms.is_none_or(|ms| ms < MIN_FILL_MS) {
        return Err(ApiError::field("elapsed_ms", "too_fast"));
    }

    let customer = check_customer(
        &request.customer_name,
        &request.customer_phone,
        request.comment.as_deref(),
        request.delivery_type,
        request.delivery_address.as_deref(),
    )
    .map_err(|(field, code)| ApiError::field(field, code))?;
    if request.items.is_empty() || request.items.len() > MAX_LINES {
        return Err(ApiError::field("items", "invalid"));
    }

    // Лимиты проверяются до общей блокировки: отклонённые запросы не стоят в очереди на запись.
    // Засчитываются они ниже, только когда заявка действительно создана.
    let ip_key = ip.to_string();
    let limited = |retry_after_secs| ApiError::RateLimited { retry_after_secs };

    let mut tx = state.db.begin().await?;
    crate::sync::lock_changes(&mut tx).await?;

    // Повтор той же заявки (связь оборвалась, человек нажал ещё раз) — тот же заказ.
    // Номер и сумму отдаём, только если совпадает телефон: иначе по чужому id не узнать ничего.
    let existing: Option<(i64, String, String)> =
        sqlx::query_as("SELECT number, total::text, customer_phone FROM orders WHERE id = $1")
            .bind(request.id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((number, total, phone)) = existing {
        tx.commit().await?;
        if phone != customer.phone {
            return Err(ApiError::field("id", "conflict"));
        }
        return Ok((StatusCode::OK, Json(json!({ "id": request.id, "number": number, "total": total }))));
    }

    // Повтор не тратит попытку; новая заявка — проверяется по обоим лимитам.
    state.order_ip_limiter.check(&ip_key).map_err(limited)?;
    state.order_phone_limiter.check(&customer.phone).map_err(limited)?;

    let mut lines = Vec::with_capacity(request.items.len());
    let mut seen = std::collections::HashSet::new();
    for item in &request.items {
        if !seen.insert((item.product_id, item.stock_unit)) {
            return Err(ApiError::field("items", "invalid"));
        }
        // Только товары, которые сайт показывает, и только их цены.
        let product: Option<(String, Option<String>, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH RECURSIVE {VISIBLE_CATEGORIES}
             SELECT p.price::text, p.pack_price::text, u.allow_fraction
             FROM products p JOIN units u ON u.code = p.unit_code
             WHERE p.id = $1 AND p.is_published AND NOT p.is_deleted
               AND p.category_id IN (SELECT id FROM visible)"
        )))
        .bind(item.product_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (price, pack_price, allow_fraction) = product.ok_or_else(|| ApiError::field("items", "invalid"))?;
        let price = match item.stock_unit {
            StockUnit::Unit => price,
            StockUnit::Pack => pack_price.ok_or_else(|| ApiError::field("items", "invalid"))?,
        };
        let raw_qty = match &item.qty {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => return Err(ApiError::field("items", "invalid")),
        };
        let qty = parse_qty(&raw_qty, item.stock_unit == StockUnit::Unit && allow_fraction)
            .filter(|qty| qty.parse::<f64>().is_ok_and(|value| value <= MAX_QTY))
            .ok_or_else(|| ApiError::field("items", "invalid"))?;
        lines.push(OrderLine { id: Uuid::new_v4(), product_id: item.product_id, unit: item.stock_unit, qty, price });
    }

    let order = NewOrder {
        id: request.id,
        source: Source::Site,
        customer,
        created_by: None,
        created_at: OffsetDateTime::now_utc(),
        lines,
    };
    let (number, total) = insert_order(&mut tx, &order).await.map_err(|error| {
        // Сумма вышла за пределы — значит, количества негодные, а не сбой сервера.
        if error.as_database_error().and_then(|e| e.code()).is_some_and(|code| code.starts_with("22")) {
            ApiError::field("items", "invalid")
        } else {
            ApiError::from(error)
        }
    })?;
    // Попытка засчитывается только за созданную заявку: отказ по данным её не тратит.
    // Заявки создаются по одной (общая блокировка), поэтому проверка выше ещё в силе.
    state.order_ip_limiter.hit(&ip_key).map_err(limited)?;
    state.order_phone_limiter.hit(&order.customer.phone).map_err(limited)?;
    // Имя, телефон и адрес в неизменяемый журнал не пишутся.
    let actor = Actor { user_id: None, full_name: "Сайт".to_owned(), device_id: None };
    audit::record(
        &mut *tx,
        Entry::new(&actor, "order.create")
            .entity("order", request.id)
            .details(json!({ "number": number, "total": total, "items": order.lines.len(), "source": "site" })),
    )
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(json!({ "id": request.id, "number": number, "total": total }))))
}
