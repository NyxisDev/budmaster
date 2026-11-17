//! Продажа с кассы и возврат от покупателя.

use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Channel, NewSale, PaymentType, SaleLine, record_sale};
use crate::sync::stock::{Movement, add_movement, clean_reason, product_info, require_warehouse};
use crate::sync::values::StockUnit;
use crate::sync::{OpCtx, OpError, OpResult};

const MAX_LINES: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaleItem {
    id: Uuid,
    product_id: Uuid,
    stock_unit: StockUnit,
    qty: String,
    /// Цена, по которой продали. Принимается как есть, даже если цена товара с тех пор
    /// изменилась: покупатель уже заплатил (P18). Расхождение отмечается в журнале.
    price: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaleCreate {
    id: Uuid,
    warehouse_id: Uuid,
    payment_type: PaymentType,
    items: Vec<SaleItem>,
}

pub async fn sale(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: SaleCreate) -> OpResult {
    if op.items.is_empty() || op.items.len() > MAX_LINES {
        return Err(OpError::Rejected("invalid_items"));
    }
    super::unique_lines(op.items.iter().map(|item| (&item.product_id, &item.stock_unit)))?;
    require_warehouse(conn, op.warehouse_id).await?;

    let mut lines = Vec::with_capacity(op.items.len());
    let mut price_differs = Vec::new();
    let mut hidden = Vec::new();
    for item in &op.items {
        let product = product_info(conn, item.product_id).await?;
        let qty = product.qty(item.stock_unit, &item.qty)?;
        let price = crate::catalog::parse_money(&item.price).ok_or(OpError::Rejected("invalid_price"))?;
        let (current, current_pack, visible): (String, Option<String>, bool) = sqlx::query_as(
            "SELECT price::text, pack_price::text, is_published AND NOT is_deleted FROM products WHERE id = $1",
        )
        .bind(item.product_id)
        .fetch_one(&mut *conn)
        .await?;
        let current = if item.stock_unit == StockUnit::Pack { current_pack } else { Some(current) };
        if current.as_deref() != Some(price.as_str()) {
            price_differs.push(json!({ "product_id": item.product_id, "sold": price, "current": current }));
        }
        // Товар, который за это время скрыли или удалили, всё равно продан — продажа принимается.
        if !visible {
            hidden.push(item.product_id);
        }
        lines.push(SaleLine { id: item.id, product_id: item.product_id, unit: item.stock_unit, qty, price });
    }

    let sale = NewSale {
        id: op.id,
        channel: Channel::Pos,
        order_id: None,
        warehouse_id: op.warehouse_id,
        payment_type: op.payment_type,
        lines,
    };
    let total = record_sale(conn, ctx, sale).await?;
    let mut details = json!({ "total": total, "items": op.items.len() });
    if !price_differs.is_empty() {
        details["price_differs"] = json!(price_differs);
    }
    if !hidden.is_empty() {
        details["hidden_products"] = json!(hidden);
    }
    ctx.audit(conn, "sale.create", Some(("sale", op.id)), Some(op.warehouse_id), details).await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReturnItem {
    id: Uuid,
    /// Строка исходной продажи. Нет — возврат без привязки (продажа уже в архиве).
    #[serde(default)]
    sale_item_id: Option<Uuid>,
    product_id: Uuid,
    stock_unit: StockUnit,
    qty: String,
    /// Цена возврата — только для возврата без привязки; иначе берётся из продажи.
    #[serde(default)]
    price: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReturnCreate {
    id: Uuid,
    #[serde(default)]
    sale_id: Option<Uuid>,
    warehouse_id: Uuid,
    reason: String,
    payment_type: PaymentType,
    /// Канал — только для возврата без привязки (по умолчанию касса).
    #[serde(default)]
    channel: Option<Channel>,
    items: Vec<ReturnItem>,
}

struct ReturnLine {
    id: Uuid,
    sale_item_id: Option<Uuid>,
    product_id: Uuid,
    unit: StockUnit,
    qty: String,
    price: String,
    cost: Option<String>,
}

/// Возврат: товар возвращается на склад, выручка уменьшается в канале исходной продажи.
/// По строке продажи нельзя вернуть больше, чем по ней продали (с учётом прежних возвратов).
pub async fn return_goods(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: ReturnCreate) -> OpResult {
    if op.items.is_empty() || op.items.len() > MAX_LINES {
        return Err(OpError::Rejected("invalid_items"));
    }
    require_warehouse(conn, op.warehouse_id).await?;
    let reason = clean_reason(&op.reason)?;

    let channel = match op.sale_id {
        Some(sale_id) => {
            if op.channel.is_some() {
                return Err(OpError::Rejected("invalid_field"));
            }
            let channel: Option<Channel> = sqlx::query_scalar("SELECT channel FROM sales WHERE id = $1")
                .bind(sale_id)
                .fetch_optional(&mut *conn)
                .await?;
            channel.ok_or(OpError::Rejected("sale_not_found"))?
        }
        None => op.channel.unwrap_or(Channel::Pos),
    };

    let mut lines = Vec::with_capacity(op.items.len());
    let mut seen = std::collections::HashSet::new();
    for item in &op.items {
        let product = product_info(conn, item.product_id).await?;
        let qty = product.qty(item.stock_unit, &item.qty)?;
        let line = match (op.sale_id, item.sale_item_id) {
            (Some(sale_id), Some(sale_item_id)) => {
                if !seen.insert(sale_item_id) || item.price.is_some() {
                    return Err(OpError::Rejected("invalid_items"));
                }
                // Что продано по этой строке и сколько уже вернули.
                let row: Option<(Uuid, StockUnit, String, Option<String>, bool)> = sqlx::query_as(
                    "SELECT i.product_id, i.stock_unit, i.price::text, i.unit_cost::text,
                            i.qty >= COALESCE((SELECT sum(r.qty) FROM return_items r WHERE r.sale_item_id = i.id), 0)
                                     + $3::numeric AS fits
                     FROM sale_items i WHERE i.id = $1 AND i.sale_id = $2",
                )
                .bind(sale_item_id)
                .bind(sale_id)
                .bind(&qty)
                .fetch_optional(&mut *conn)
                .await?;
                let (product_id, unit, price, cost, fits) = row.ok_or(OpError::Rejected("sale_item_not_found"))?;
                if product_id != item.product_id || unit != item.stock_unit {
                    return Err(OpError::Rejected("sale_item_mismatch"));
                }
                if !fits {
                    return Err(OpError::Rejected("return_exceeds_sale"));
                }
                ReturnLine { id: item.id, sale_item_id: Some(sale_item_id), product_id, unit, qty, price, cost }
            }
            (None, None) => {
                let raw = item.price.as_deref().ok_or(OpError::Rejected("price_required"))?;
                let price = crate::catalog::parse_money(raw).ok_or(OpError::Rejected("invalid_price"))?;
                // Без привязки себестоимость — текущая средняя (раздел 4.4).
                let cost: Option<String> = sqlx::query_scalar(
                    "SELECT (avg_cost * $2)::numeric(14,4)::text FROM product_costs WHERE product_id = $1",
                )
                .bind(item.product_id)
                .bind(product.multiplier(item.stock_unit))
                .fetch_optional(&mut *conn)
                .await?;
                ReturnLine {
                    id: item.id,
                    sale_item_id: None,
                    product_id: item.product_id,
                    unit: item.stock_unit,
                    qty,
                    price,
                    cost,
                }
            }
            // Строки продажи без продажи и наоборот не сочетаются.
            _ => return Err(OpError::Rejected("invalid_items")),
        };
        lines.push(line);
    }

    sqlx::query(
        "INSERT INTO returns (id, sale_id, channel, warehouse_id, reason, refund_total, payment_type, user_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5, 0, $6, $7, $8)",
    )
    .bind(op.id)
    .bind(op.sale_id)
    .bind(channel)
    .bind(op.warehouse_id)
    .bind(&reason)
    .bind(op.payment_type)
    .bind(ctx.user.id)
    .bind(ctx.at)
    .execute(&mut *conn)
    .await?;

    for line in &lines {
        sqlx::query(
            "INSERT INTO return_items (id, return_id, sale_item_id, product_id, stock_unit, qty, price, unit_cost)
             VALUES ($1, $2, $3, $4, $5, $6::numeric, $7::numeric, $8::numeric)",
        )
        .bind(line.id)
        .bind(op.id)
        .bind(line.sale_item_id)
        .bind(line.product_id)
        .bind(line.unit)
        .bind(&line.qty)
        .bind(&line.price)
        .bind(&line.cost)
        .execute(&mut *conn)
        .await?;
        add_movement(
            conn,
            ctx,
            Movement {
                product_id: line.product_id,
                warehouse_id: op.warehouse_id,
                unit: line.unit,
                qty: &line.qty,
                kind: "return",
                unit_cost: line.cost.clone(),
                supply_item_id: None,
                transfer_item_id: None,
                sale_item_id: None,
                return_item_id: Some(line.id),
                reason: Some(&reason),
            },
        )
        .await?;
    }

    let total: String = sqlx::query_scalar(
        "UPDATE returns SET refund_total = COALESCE((
            SELECT sum(round(qty * price, 2)) FROM return_items WHERE return_id = $1), 0)
         WHERE id = $1 RETURNING refund_total::text",
    )
    .bind(op.id)
    .fetch_one(&mut *conn)
    .await?;
    ctx.audit(
        conn,
        "return.create",
        Some(("return", op.id)),
        Some(op.warehouse_id),
        // Причина хранится в возврате; свободный текст в неизменяемый журнал не пишем.
        json!({ "sale_id": op.sale_id, "refund_total": total, "items": lines.len() }),
    )
    .await?;
    Ok(())
}
