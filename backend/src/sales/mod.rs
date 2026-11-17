//! Заказы, продажи, возвраты (docs/design.md, разделы 4.4, 6, 9.4).
//!
//! Деньги считаются только в `numeric` базы: сумма строки — `round(qty × price, 2)`,
//! сумма документа — сумма округлённых строк. Так же считает корзина сайта.

pub mod orders;
pub mod pos;
pub mod public;
pub mod read;
pub mod reports;

use serde::Deserialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::sync::stock::{Movement, add_movement};
use crate::sync::values::StockUnit;
use crate::sync::{OpCtx, OpError, OpResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, sqlx::Type)]
#[sqlx(type_name = "payment_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum PaymentType {
    Cash,
    Terminal,
    CardTransfer,
    BankTransfer,
    EVidnovlennia,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, sqlx::Type)]
#[sqlx(type_name = "sale_channel", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Site,
    Manual,
    Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, sqlx::Type)]
#[sqlx(type_name = "delivery_type", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum DeliveryType {
    Pickup,
    Delivery,
}

/// Украинский номер в виде `+380XXXXXXXXX`. Принимает «050 123 45 67», «+38 (050) 123-45-67»,
/// «380501234567». Так же нормализует номер сайт.
pub fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    let national = if digits.len() == 12 && digits.starts_with("380") {
        &digits[3..]
    } else if digits.len() == 11 && digits.starts_with("80") {
        &digits[2..]
    } else if digits.len() == 10 && digits.starts_with('0') {
        &digits[1..]
    } else {
        return None;
    };
    if national.starts_with('0') {
        return None;
    }
    Some(format!("+380{national}"))
}

/// Данные клиента заказа после проверки.
pub struct Customer {
    pub name: String,
    pub phone: String,
    pub comment: Option<String>,
    pub delivery_type: DeliveryType,
    pub delivery_address: Option<String>,
}

/// Проверяет данные клиента. Ошибка — имя поля и код.
pub fn check_customer(
    name: &str,
    phone: &str,
    comment: Option<&str>,
    delivery_type: DeliveryType,
    address: Option<&str>,
) -> Result<Customer, (&'static str, &'static str)> {
    let name = crate::catalog::clean_line(name);
    if !(2..=100).contains(&name.chars().count()) {
        return Err(("customer_name", "invalid"));
    }
    let phone = normalize_phone(phone).ok_or(("customer_phone", "invalid"))?;
    let comment = comment.map(crate::catalog::clean_text).filter(|text| !text.is_empty());
    if comment.as_ref().is_some_and(|text| text.chars().count() > 1000) {
        return Err(("comment", "too_long"));
    }
    let address = match delivery_type {
        DeliveryType::Pickup => None,
        DeliveryType::Delivery => {
            let address = address.map(crate::catalog::clean_line).filter(|text| !text.is_empty());
            match address {
                None => return Err(("delivery_address", "required")),
                Some(text) if text.chars().count() > 300 => return Err(("delivery_address", "too_long")),
                Some(text) => Some(text),
            }
        }
    };
    Ok(Customer { name, phone, comment, delivery_type, delivery_address: address })
}

/// Добавляет (`sign = 1`) или снимает (`sign = -1`) резерв всех строк заказа на его складе.
/// Резерв лежит в кэше остатков и меняется в той же транзакции, что и заказ.
pub async fn reserve(conn: &mut PgConnection, order_id: Uuid, sign: i32) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO stock_balances (product_id, warehouse_id, reserved_units, reserved_packs)
         SELECT i.product_id, o.warehouse_id,
                $2 * COALESCE(sum(i.qty) FILTER (WHERE i.stock_unit = 'unit'), 0),
                $2 * COALESCE(sum(i.qty) FILTER (WHERE i.stock_unit = 'pack'), 0)
         FROM order_items i JOIN orders o ON o.id = i.order_id
         WHERE o.id = $1 AND o.warehouse_id IS NOT NULL
         GROUP BY i.product_id, o.warehouse_id
         ON CONFLICT (product_id, warehouse_id) DO UPDATE SET
            reserved_units = stock_balances.reserved_units + EXCLUDED.reserved_units,
            reserved_packs = stock_balances.reserved_packs + EXCLUDED.reserved_packs,
            updated_at = now()",
    )
    .bind(order_id)
    .bind(sign)
    .execute(conn)
    .await?;
    Ok(())
}

/// Строка продажи перед проведением.
pub struct SaleLine {
    pub id: Uuid,
    pub product_id: Uuid,
    pub unit: StockUnit,
    pub qty: String,
    pub price: String,
}

pub struct NewSale {
    pub id: Uuid,
    pub channel: Channel,
    pub order_id: Option<Uuid>,
    pub warehouse_id: Uuid,
    pub payment_type: PaymentType,
    pub lines: Vec<SaleLine>,
}

/// Проводит продажу: документ, строки с себестоимостью на этот момент, движения товара.
/// Возвращает сумму продажи.
pub async fn record_sale(conn: &mut PgConnection, ctx: &OpCtx<'_>, sale: NewSale) -> Result<String, OpError> {
    sqlx::query(
        "INSERT INTO sales (id, channel, order_id, warehouse_id, payment_type, total, user_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5, 0, $6, $7)",
    )
    .bind(sale.id)
    .bind(sale.channel)
    .bind(sale.order_id)
    .bind(sale.warehouse_id)
    .bind(sale.payment_type)
    .bind(ctx.user.id)
    .bind(ctx.at)
    .execute(&mut *conn)
    .await?;

    for line in &sale.lines {
        let multiplier = multiplier(conn, line.product_id, line.unit).await?;
        // Себестоимость фиксируется сейчас и задним числом не пересчитывается (раздел 6).
        let cost: Option<String> =
            sqlx::query_scalar("SELECT (avg_cost * $2)::numeric(14,4)::text FROM product_costs WHERE product_id = $1")
                .bind(line.product_id)
                .bind(multiplier)
                .fetch_optional(&mut *conn)
                .await?;
        sqlx::query(
            "INSERT INTO sale_items (id, sale_id, product_id, stock_unit, qty, price, unit_cost)
             VALUES ($1, $2, $3, $4, $5::numeric, $6::numeric, $7::numeric)",
        )
        .bind(line.id)
        .bind(sale.id)
        .bind(line.product_id)
        .bind(line.unit)
        .bind(&line.qty)
        .bind(&line.price)
        .bind(&cost)
        .execute(&mut *conn)
        .await?;
        add_movement(
            conn,
            ctx,
            Movement {
                product_id: line.product_id,
                warehouse_id: sale.warehouse_id,
                unit: line.unit,
                qty: &format!("-{}", line.qty),
                kind: "sale",
                unit_cost: cost,
                supply_item_id: None,
                transfer_item_id: None,
                sale_item_id: Some(line.id),
                return_item_id: None,
                reason: None,
            },
        )
        .await?;
    }

    let total: String = sqlx::query_scalar(
        "UPDATE sales SET total = COALESCE((
            SELECT sum(round(qty * price, 2)) FROM sale_items WHERE sale_id = $1), 0)
         WHERE id = $1 RETURNING total::text",
    )
    .bind(sale.id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(total)
}

/// Во сколько штук обходится единица учёта (пачка — в размер пачки).
async fn multiplier(conn: &mut PgConnection, product_id: Uuid, unit: StockUnit) -> Result<i32, OpError> {
    if unit == StockUnit::Unit {
        return Ok(1);
    }
    let size: Option<Option<i32>> = sqlx::query_scalar("SELECT pack_size FROM products WHERE id = $1")
        .bind(product_id)
        .fetch_optional(conn)
        .await?;
    size.flatten().ok_or(OpError::Rejected("no_pack"))
}

/// Проверяет, что в строках нет двух одинаковых пар «товар + единица учёта».
pub fn unique_lines<'a>(lines: impl Iterator<Item = (&'a Uuid, &'a StockUnit)>) -> OpResult {
    let mut seen = std::collections::HashSet::new();
    for (product, unit) in lines {
        if !seen.insert((*product, *unit)) {
            return Err(OpError::Rejected("duplicate_line"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones() {
        for (raw, expected) in [
            ("050 123 45 67", "+380501234567"),
            ("+38 (050) 123-45-67", "+380501234567"),
            ("380971234567", "+380971234567"),
            ("80501234567", "+380501234567"),
        ] {
            assert_eq!(normalize_phone(raw).as_deref(), Some(expected), "{raw}");
        }
        for bad in ["", "0001234567", "+48 501 234 567", "050123456", "+3800501234567"] {
            assert_eq!(normalize_phone(bad), None, "{bad}");
        }
    }

    #[test]
    fn customers() {
        let ok = check_customer(" Іван  ", "050 123 45 67", Some(" коментар "), DeliveryType::Delivery, Some(" вул. 1 "));
        let customer = ok.unwrap_or_else(|_| panic!("valid"));
        assert_eq!(customer.name, "Іван");
        assert_eq!(customer.delivery_address.as_deref(), Some("вул. 1"));
        assert_eq!(customer.comment.as_deref(), Some("коментар"));
        assert_eq!(check_customer("Іван", "050 123 45 67", None, DeliveryType::Delivery, None).err(), Some(("delivery_address", "required")));
        assert_eq!(check_customer("І", "050 123 45 67", None, DeliveryType::Pickup, None).err(), Some(("customer_name", "invalid")));
        // При самовывозе адрес не сохраняется, даже если прислали.
        let pickup = check_customer("Іван", "0501234567", None, DeliveryType::Pickup, Some("вул. 1")).ok().unwrap();
        assert!(pickup.delivery_address.is_none());
    }
}
