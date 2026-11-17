//! Заказы: создание (с сайта и вручную), правка до выдачи, смена статусов.
//!
//! Статусы: new → confirmed → assembled → completed; cancelled — из new, confirmed, assembled.
//! - confirmed: выбирается склад, товар резервируется (резерв уменьшает «доступно»);
//! - completed: резерв снимается, проводится продажа (списание со склада, себестоимость);
//! - cancelled: резерв снимается.
//!
//! Каждая смена статуса — в истории статусов и в журнале.

use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{Channel, Customer, DeliveryType, NewSale, PaymentType, SaleLine, check_customer, record_sale, reserve};
use crate::sync::stock::product_info;
use crate::sync::values::StockUnit;
use crate::sync::{OpCtx, OpError, OpResult};

pub const MAX_LINES: usize = 100;

/// Склад для заказа: существует и не выключен (выключенный склад не выбрать).
async fn require_active_warehouse(conn: &mut PgConnection, warehouse_id: Uuid) -> OpResult {
    let active: Option<bool> = sqlx::query_scalar("SELECT is_active FROM warehouses WHERE id = $1")
        .bind(warehouse_id)
        .fetch_optional(conn)
        .await?;
    match active {
        None => Err(OpError::Rejected("warehouse_not_found")),
        Some(false) => Err(OpError::Rejected("warehouse_inactive")),
        Some(true) => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "order_status", rename_all = "lowercase")]
pub enum Status {
    New,
    Confirmed,
    Assembled,
    Completed,
    Cancelled,
}

impl Status {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "new" => Self::New,
            "confirmed" => Self::Confirmed,
            "assembled" => Self::Assembled,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Confirmed => "confirmed",
            Self::Assembled => "assembled",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Товар под заказ зарезервирован на складе.
    fn reserves(self) -> bool {
        matches!(self, Self::Confirmed | Self::Assembled)
    }

    fn is_open(self) -> bool {
        matches!(self, Self::New | Self::Confirmed | Self::Assembled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "order_source", rename_all = "lowercase")]
pub enum Source {
    Site,
    Manual,
}

pub struct OrderLine {
    pub id: Uuid,
    pub product_id: Uuid,
    pub unit: StockUnit,
    pub qty: String,
    pub price: String,
}

pub struct NewOrder {
    pub id: Uuid,
    pub source: Source,
    pub customer: Customer,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub lines: Vec<OrderLine>,
}

/// Записывает новый заказ со строками и первой записью истории. Возвращает (номер, сумма).
pub async fn insert_order(conn: &mut PgConnection, order: &NewOrder) -> sqlx::Result<(i64, String)> {
    let customer = &order.customer;
    sqlx::query(
        "INSERT INTO orders (id, source, customer_name, customer_phone, comment, delivery_type, delivery_address,
                             total, created_by, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, $9)",
    )
    .bind(order.id)
    .bind(order.source)
    .bind(&customer.name)
    .bind(&customer.phone)
    .bind(&customer.comment)
    .bind(customer.delivery_type)
    .bind(&customer.delivery_address)
    .bind(order.created_by)
    .bind(order.created_at)
    .execute(&mut *conn)
    .await?;
    insert_lines(conn, order.id, &order.lines).await?;
    sqlx::query(
        "INSERT INTO order_status_history (id, order_id, from_status, to_status, user_id, occurred_at)
         VALUES ($1, $2, NULL, 'new', $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(order.id)
    .bind(order.created_by)
    .bind(order.created_at)
    .execute(&mut *conn)
    .await?;
    update_total(conn, order.id).await
}

async fn insert_lines(conn: &mut PgConnection, order_id: Uuid, lines: &[OrderLine]) -> sqlx::Result<()> {
    for line in lines {
        sqlx::query(
            "INSERT INTO order_items (id, order_id, product_id, stock_unit, qty, price)
             VALUES ($1, $2, $3, $4, $5::numeric, $6::numeric)",
        )
        .bind(line.id)
        .bind(order_id)
        .bind(line.product_id)
        .bind(line.unit)
        .bind(&line.qty)
        .bind(&line.price)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Сумма заказа — сумма строк, каждая округлена до копеек.
async fn update_total(conn: &mut PgConnection, order_id: Uuid) -> sqlx::Result<(i64, String)> {
    sqlx::query_as(
        "UPDATE orders SET total = COALESCE((
            SELECT sum(round(qty * price, 2)) FROM order_items WHERE order_id = $1), 0),
            updated_at = now()
         WHERE id = $1 RETURNING number, total::text",
    )
    .bind(order_id)
    .fetch_one(conn)
    .await
}

/// Текущая цена товара за единицу учёта: цена штуки или цена пачки.
pub async fn current_price(conn: &mut PgConnection, product_id: Uuid, unit: StockUnit) -> Result<String, OpError> {
    let row: Option<(String, Option<String>, bool)> =
        sqlx::query_as("SELECT price::text, pack_price::text, is_deleted FROM products WHERE id = $1")
            .bind(product_id)
            .fetch_optional(conn)
            .await?;
    let (price, pack_price, deleted) = row.ok_or(OpError::Rejected("product_not_found"))?;
    if deleted {
        return Err(OpError::Rejected("product_not_found"));
    }
    match unit {
        StockUnit::Unit => Ok(price),
        StockUnit::Pack => pack_price.ok_or(OpError::Rejected("no_pack")),
    }
}

// --- Заказ вручную ------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineInput {
    id: Uuid,
    product_id: Uuid,
    stock_unit: StockUnit,
    qty: String,
    /// Цена строки. Задать может только владелец (P4); иначе — текущая цена товара.
    #[serde(default)]
    price: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderCreate {
    id: Uuid,
    customer_name: String,
    customer_phone: String,
    #[serde(default)]
    comment: Option<String>,
    delivery_type: DeliveryType,
    #[serde(default)]
    delivery_address: Option<String>,
    items: Vec<LineInput>,
}

fn customer_error((field, code): (&'static str, &'static str)) -> OpError {
    OpError::Rejected(match (field, code) {
        ("customer_phone", _) => "invalid_phone",
        ("delivery_address", "required") => "address_required",
        _ => "invalid_field",
    })
}

/// Проверяет строки заказа и определяет их цены.
async fn check_lines(
    conn: &mut PgConnection,
    ctx: &OpCtx<'_>,
    items: &[LineInput],
    keep_prices: &[(Uuid, StockUnit, String)],
) -> Result<Vec<OrderLine>, OpError> {
    if items.is_empty() || items.len() > MAX_LINES {
        return Err(OpError::Rejected("invalid_items"));
    }
    super::unique_lines(items.iter().map(|item| (&item.product_id, &item.stock_unit)))?;
    let mut lines = Vec::with_capacity(items.len());
    for item in items {
        let product = product_info(conn, item.product_id).await?;
        let qty = product.qty(item.stock_unit, &item.qty)?;
        let price = match &item.price {
            Some(_) if !ctx.user.is_owner() => return Err(OpError::Rejected("permission_required")),
            Some(raw) => crate::catalog::parse_money(raw).ok_or(OpError::Rejected("invalid_price"))?,
            None => match keep_prices.iter().find(|(p, u, _)| *p == item.product_id && *u == item.stock_unit) {
                // Строка уже была в заказе — её цена сохраняется (зафиксирована при создании).
                Some((_, _, price)) => price.clone(),
                None => current_price(conn, item.product_id, item.stock_unit).await?,
            },
        };
        lines.push(OrderLine { id: item.id, product_id: item.product_id, unit: item.stock_unit, qty, price });
    }
    Ok(lines)
}

pub async fn create(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: OrderCreate) -> OpResult {
    let customer = check_customer(
        &op.customer_name,
        &op.customer_phone,
        op.comment.as_deref(),
        op.delivery_type,
        op.delivery_address.as_deref(),
    )
    .map_err(customer_error)?;
    let lines = check_lines(conn, ctx, &op.items, &[]).await?;
    let order = NewOrder {
        id: op.id,
        source: Source::Manual,
        customer,
        created_by: Some(ctx.user.id),
        created_at: ctx.at,
        lines,
    };
    let (number, total) = insert_order(conn, &order).await?;
    // Телефон, имя и адрес в неизменяемый журнал не пишутся — только номер и сумма.
    ctx.audit(
        conn,
        "order.create",
        Some(("order", op.id)),
        None,
        json!({ "number": number, "total": total, "items": order.lines.len(), "source": "manual" }),
    )
    .await?;
    Ok(())
}

// --- Правка заказа ------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderUpdate {
    id: Uuid,
    #[serde(default)]
    customer_name: Option<String>,
    #[serde(default)]
    customer_phone: Option<String>,
    /// `null` — убрать комментарий.
    #[serde(default, with = "double_option")]
    comment: Option<Option<String>>,
    #[serde(default)]
    delivery_type: Option<DeliveryType>,
    #[serde(default)]
    delivery_address: Option<String>,
    #[serde(default)]
    warehouse_id: Option<Uuid>,
    /// Новый состав целиком.
    #[serde(default)]
    items: Option<Vec<LineInput>>,
}

/// Различает «поля нет» и «поле равно null».
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::deserialize(deserializer)?))
    }
}

#[derive(sqlx::FromRow)]
struct OrderRow {
    number: i64,
    source: Source,
    status: Status,
    customer_name: String,
    customer_phone: String,
    comment: Option<String>,
    delivery_type: DeliveryType,
    delivery_address: Option<String>,
    warehouse_id: Option<Uuid>,
    total: String,
}

async fn lock_order(conn: &mut PgConnection, id: Uuid) -> Result<OrderRow, OpError> {
    let row: Option<OrderRow> = sqlx::query_as(
        "SELECT number, source, status, customer_name, customer_phone, comment, delivery_type, delivery_address,
                warehouse_id, total::text AS total
         FROM orders WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.ok_or(OpError::Rejected("order_not_found"))
}

/// Правка заказа до выдачи: данные клиента, склад, состав. Цены новых строк — текущие,
/// у оставшихся строк — прежние; изменить цену строки может только владелец.
pub async fn update(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: OrderUpdate) -> OpResult {
    let order = lock_order(conn, op.id).await?;
    if !order.status.is_open() {
        return Err(OpError::Rejected("order_closed"));
    }

    let delivery_type = op.delivery_type.unwrap_or(order.delivery_type);
    let customer = check_customer(
        op.customer_name.as_deref().unwrap_or(&order.customer_name),
        op.customer_phone.as_deref().unwrap_or(&order.customer_phone),
        match &op.comment {
            Some(comment) => comment.as_deref(),
            None => order.comment.as_deref(),
        },
        delivery_type,
        op.delivery_address.as_deref().or(order.delivery_address.as_deref()),
    )
    .map_err(customer_error)?;

    if let Some(warehouse_id) = op.warehouse_id {
        require_active_warehouse(conn, warehouse_id).await?;
    }

    // Резерв снимается со старого склада и старого состава и ставится заново после правки.
    if order.status.reserves() {
        reserve(conn, op.id, -1).await?;
    }

    sqlx::query(
        "UPDATE orders SET customer_name = $2, customer_phone = $3, comment = $4, delivery_type = $5,
                delivery_address = $6, warehouse_id = COALESCE($7, warehouse_id), updated_at = now()
         WHERE id = $1",
    )
    .bind(op.id)
    .bind(&customer.name)
    .bind(&customer.phone)
    .bind(&customer.comment)
    .bind(customer.delivery_type)
    .bind(&customer.delivery_address)
    .bind(op.warehouse_id)
    .execute(&mut *conn)
    .await?;

    if let Some(items) = &op.items {
        let previous: Vec<(Uuid, StockUnit, String)> =
            sqlx::query_as("SELECT product_id, stock_unit, price::text FROM order_items WHERE order_id = $1")
                .bind(op.id)
                .fetch_all(&mut *conn)
                .await?;
        let lines = check_lines(conn, ctx, items, &previous).await?;
        sqlx::query("DELETE FROM order_items WHERE order_id = $1").bind(op.id).execute(&mut *conn).await?;
        insert_lines(conn, op.id, &lines).await?;
    }

    if order.status.reserves() {
        reserve(conn, op.id, 1).await?;
    }
    let (_, total) = update_total(conn, op.id).await?;

    let mut changed = Vec::new();
    for (field, present) in [
        ("customer_name", op.customer_name.is_some()),
        ("customer_phone", op.customer_phone.is_some()),
        ("comment", op.comment.is_some()),
        ("delivery_type", op.delivery_type.is_some()),
        ("delivery_address", op.delivery_address.is_some()),
        ("warehouse_id", op.warehouse_id.is_some()),
        ("items", op.items.is_some()),
    ] {
        if present {
            changed.push(field);
        }
    }
    // Ручная правка цены (только владелец, P4) — с товаром и новой ценой.
    let price_overrides: Vec<_> = op
        .items
        .iter()
        .flatten()
        .filter_map(|item| item.price.as_ref().map(|price| json!({ "product_id": item.product_id, "price": price })))
        .collect();
    let mut details = json!({ "number": order.number, "changed": changed, "total_before": order.total, "total_after": total });
    if !price_overrides.is_empty() {
        details["price_overrides"] = json!(price_overrides);
    }
    ctx.audit(conn, "order.update", Some(("order", op.id)), op.warehouse_id.or(order.warehouse_id), details).await?;
    Ok(())
}

// --- Смена статуса -----------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetStatus {
    id: Uuid,
    status: String,
    /// Склад при подтверждении (если не выбран раньше).
    #[serde(default)]
    warehouse_id: Option<Uuid>,
    /// При выдаче: способ оплаты и id продажи, которую создаст сервер.
    #[serde(default)]
    payment_type: Option<PaymentType>,
    #[serde(default)]
    sale_id: Option<Uuid>,
    #[serde(default)]
    cancel_reason: Option<String>,
}

fn allowed(from: Status, to: Status) -> bool {
    use Status::*;
    matches!(
        (from, to),
        (New, Confirmed) | (Confirmed, Assembled) | (Assembled, Completed) | (New | Confirmed | Assembled, Cancelled)
    )
}

pub async fn set_status(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: SetStatus) -> OpResult {
    let target = Status::parse(&op.status).ok_or(OpError::Rejected("invalid_status"))?;
    let order = lock_order(conn, op.id).await?;

    if order.status == target {
        // Выдать заказ дважды нельзя: второе устройство должно узнать, что его уже выдали.
        // Любая другая одинаковая смена статуса с двух устройств — просто повтор.
        return if target == Status::Completed { Err(OpError::Rejected("already_completed")) } else { Ok(()) };
    }
    if !allowed(order.status, target) {
        return Err(OpError::Rejected(match order.status {
            Status::Completed => "already_completed",
            Status::Cancelled => "order_cancelled",
            _ => "invalid_transition",
        }));
    }

    let mut warehouse_id = order.warehouse_id;
    let mut details = json!({ "number": order.number, "from": order.status.as_str(), "to": target.as_str() });

    match target {
        Status::Confirmed => {
            let chosen = op.warehouse_id.or(order.warehouse_id).ok_or(OpError::Rejected("warehouse_required"))?;
            require_active_warehouse(conn, chosen).await?;
            warehouse_id = Some(chosen);
            sqlx::query("UPDATE orders SET warehouse_id = $2 WHERE id = $1")
                .bind(op.id)
                .bind(chosen)
                .execute(&mut *conn)
                .await?;
            reserve(conn, op.id, 1).await?;
        }
        Status::Assembled => {}
        Status::Completed => {
            let payment_type = op.payment_type.ok_or(OpError::Rejected("payment_type_required"))?;
            let sale_id = op.sale_id.ok_or(OpError::Rejected("sale_id_required"))?;
            let warehouse = warehouse_id.ok_or(OpError::Rejected("warehouse_required"))?;
            reserve(conn, op.id, -1).await?;
            let lines: Vec<(Uuid, StockUnit, String, String)> = sqlx::query_as(
                "SELECT product_id, stock_unit, qty::text, price::text FROM order_items WHERE order_id = $1 ORDER BY id",
            )
            .bind(op.id)
            .fetch_all(&mut *conn)
            .await?;
            let sale = NewSale {
                id: sale_id,
                channel: match order.source {
                    Source::Site => Channel::Site,
                    Source::Manual => Channel::Manual,
                },
                order_id: Some(op.id),
                warehouse_id: warehouse,
                payment_type,
                lines: lines
                    .into_iter()
                    .map(|(product_id, unit, qty, price)| SaleLine { id: Uuid::new_v4(), product_id, unit, qty, price })
                    .collect(),
            };
            let total = record_sale(conn, ctx, sale).await?;
            details["sale_id"] = json!(sale_id);
            details["total"] = json!(total);
        }
        Status::Cancelled => {
            if order.status.reserves() {
                reserve(conn, op.id, -1).await?;
            }
            let reason = op.cancel_reason.as_deref().map(crate::catalog::clean_line).filter(|text| !text.is_empty());
            if reason.as_ref().is_some_and(|text| text.chars().count() > 500) {
                return Err(OpError::Rejected("invalid_field"));
            }
            sqlx::query("UPDATE orders SET cancel_reason = $2 WHERE id = $1")
                .bind(op.id)
                .bind(&reason)
                .execute(&mut *conn)
                .await?;
            // Текст причины хранится в заказе; в неизменяемый журнал свободный текст не пишем —
            // туда случайно могут попасть данные клиента.
            details["with_reason"] = json!(reason.is_some());
        }
        Status::New => return Err(OpError::Rejected("invalid_transition")),
    }

    sqlx::query("UPDATE orders SET status = $2, updated_at = now() WHERE id = $1")
        .bind(op.id)
        .bind(target)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO order_status_history (id, order_id, from_status, to_status, user_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(op.id)
    .bind(order.status)
    .bind(target)
    .bind(ctx.user.id)
    .bind(ctx.at)
    .execute(&mut *conn)
    .await?;
    ctx.audit(conn, "order.status", Some(("order", op.id)), warehouse_id, details).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        use Status::*;
        assert!(allowed(New, Confirmed));
        assert!(allowed(Assembled, Completed));
        assert!(allowed(Confirmed, Cancelled));
        assert!(!allowed(New, Completed), "без подтверждения и сборки не выдаётся");
        assert!(!allowed(Completed, Cancelled), "после выдачи — только возврат");
        assert!(!allowed(Cancelled, Confirmed));
    }
}
