//! Складские операции. Остаток нигде не перезаписывается: каждая операция добавляет
//! движения, а кэш остатков меняется на ту же величину в той же транзакции.
//! Отрицательный остаток допускается — операция, которая реально произошла, не отклоняется
//! из-за того, что учёт отстаёт.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use time::Date;
use uuid::Uuid;

use super::values::{StockUnit, parse_delta, parse_qty, parse_unit_cost};
use super::{OpCtx, OpError, OpResult};

const MAX_ITEMS: usize = 200;
const MAX_REASON: usize = 500;
const MAX_NOTE: usize = 2000;

/// То, что нужно знать о товаре для складской операции. Размер пачки берётся только отсюда,
/// а не из присланных данных.
pub struct ProductInfo {
    pub pack_size: Option<i32>,
    pub allow_fraction: bool,
}

pub async fn product_info(conn: &mut PgConnection, product_id: Uuid) -> Result<ProductInfo, OpError> {
    let row: Option<(Option<i32>, bool)> = sqlx::query_as(
        "SELECT p.pack_size, u.allow_fraction FROM products p JOIN units u ON u.code = p.unit_code WHERE p.id = $1",
    )
    .bind(product_id)
    .fetch_optional(conn)
    .await?;
    let (pack_size, allow_fraction) = row.ok_or(OpError::Rejected("product_not_found"))?;
    Ok(ProductInfo { pack_size, allow_fraction })
}

impl ProductInfo {
    /// Проверяет количество в выбранной единице учёта.
    pub fn qty(&self, unit: StockUnit, raw: &str) -> Result<String, OpError> {
        self.require_unit(unit)?;
        parse_qty(raw, self.fraction_allowed(unit)).ok_or(OpError::Rejected("invalid_qty"))
    }

    fn delta(&self, unit: StockUnit, raw: &str) -> Result<String, OpError> {
        self.require_unit(unit)?;
        parse_delta(raw, self.fraction_allowed(unit)).ok_or(OpError::Rejected("invalid_qty"))
    }

    pub fn require_unit(&self, unit: StockUnit) -> Result<(), OpError> {
        if unit == StockUnit::Pack && self.pack_size.is_none() {
            return Err(OpError::Rejected("no_pack"));
        }
        Ok(())
    }

    /// Пачки — всегда целые; штуки — дробные, если это позволяет единица товара (кг, м).
    pub fn fraction_allowed(&self, unit: StockUnit) -> bool {
        unit == StockUnit::Unit && self.allow_fraction
    }

    /// Во сколько базовых единиц обходится одна единица учёта.
    pub fn multiplier(&self, unit: StockUnit) -> i32 {
        match unit {
            StockUnit::Unit => 1,
            StockUnit::Pack => self.pack_size.unwrap_or(1),
        }
    }
}

pub async fn require_warehouse(conn: &mut PgConnection, warehouse_id: Uuid) -> OpResult {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM warehouses WHERE id = $1)")
        .bind(warehouse_id)
        .fetch_one(conn)
        .await?;
    if exists { Ok(()) } else { Err(OpError::Rejected("warehouse_not_found")) }
}

pub struct Movement<'a> {
    pub product_id: Uuid,
    pub warehouse_id: Uuid,
    pub unit: StockUnit,
    /// Количество со знаком, десятичной строкой.
    pub qty: &'a str,
    pub kind: &'static str,
    pub unit_cost: Option<String>,
    pub supply_item_id: Option<Uuid>,
    pub transfer_item_id: Option<Uuid>,
    pub sale_item_id: Option<Uuid>,
    pub return_item_id: Option<Uuid>,
    pub reason: Option<&'a str>,
}

/// Добавляет движение и на ту же величину меняет кэш остатка.
pub async fn add_movement(conn: &mut PgConnection, ctx: &OpCtx<'_>, movement: Movement<'_>) -> OpResult {
    sqlx::query(
        "INSERT INTO stock_movements
            (id, product_id, warehouse_id, stock_unit, qty, kind, unit_cost, operation_id,
             supply_item_id, transfer_item_id, sale_item_id, return_item_id, reason, user_id, device_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5::numeric, $6::movement_kind, $7::numeric, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
    )
    .bind(Uuid::new_v4())
    .bind(movement.product_id)
    .bind(movement.warehouse_id)
    .bind(movement.unit)
    .bind(movement.qty)
    .bind(movement.kind)
    .bind(movement.unit_cost.as_deref())
    .bind(ctx.op_id)
    .bind(movement.supply_item_id)
    .bind(movement.transfer_item_id)
    .bind(movement.sale_item_id)
    .bind(movement.return_item_id)
    .bind(movement.reason)
    .bind(ctx.user.id)
    .bind(ctx.user.device_id)
    .bind(ctx.at)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "INSERT INTO stock_balances (product_id, warehouse_id, qty_units, qty_packs)
         VALUES ($1, $2,
                 CASE WHEN $3 = 'unit' THEN $4::numeric ELSE 0 END,
                 CASE WHEN $3 = 'pack' THEN $4::numeric ELSE 0 END)
         ON CONFLICT (product_id, warehouse_id) DO UPDATE SET
            qty_units = stock_balances.qty_units + EXCLUDED.qty_units,
            qty_packs = stock_balances.qty_packs + EXCLUDED.qty_packs,
            updated_at = now()",
    )
    .bind(movement.product_id)
    .bind(movement.warehouse_id)
    .bind(movement.unit.as_str())
    .bind(movement.qty)
    .execute(conn)
    .await?;
    Ok(())
}

/// Текущая средняя себестоимость за выбранную единицу учёта (за пачку = за штуку × размер пачки).
async fn current_cost(conn: &mut PgConnection, product_id: Uuid, multiplier: i32) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT (avg_cost * $2)::numeric(14,4)::text FROM product_costs WHERE product_id = $1")
        .bind(product_id)
        .bind(multiplier)
        .fetch_optional(conn)
        .await
}

/// Пересчитывает среднюю себестоимость при приёмке (docs/design.md, раздел 6).
/// Вызывается до проведения прихода: «остаток» — то, что было до него, по всем складам,
/// пачки пересчитаны в штуки. Если остаток нулевой или отрицательный, средняя равна цене поставки.
async fn update_average_cost(
    conn: &mut PgConnection,
    product_id: Uuid,
    qty: &str,
    unit_cost: &str,
    multiplier: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        "WITH stock AS (
            SELECT COALESCE(sum(b.qty_units + b.qty_packs * COALESCE(p.pack_size, 0)), 0) AS units
            FROM stock_balances b JOIN products p ON p.id = b.product_id
            WHERE b.product_id = $1
         ), incoming AS (
            SELECT $2::numeric * $4 AS units, $3::numeric / $4 AS cost
         )
         INSERT INTO product_costs (product_id, avg_cost, updated_at)
         SELECT $1,
                -- Остатка нет, или он есть, но цена его неизвестна (внесён корректировкой,
                -- а не приёмкой): средняя — цена этой поставки, а не смесь с нулём.
                CASE WHEN stock.units <= 0 OR pc.avg_cost IS NULL THEN round(incoming.cost, 4)
                     ELSE round((stock.units * COALESCE(pc.avg_cost, 0) + incoming.units * incoming.cost)
                                / (stock.units + incoming.units), 4)
                END,
                now()
         FROM stock CROSS JOIN incoming LEFT JOIN product_costs pc ON pc.product_id = $1
         ON CONFLICT (product_id) DO UPDATE SET avg_cost = EXCLUDED.avg_cost, updated_at = now()",
    )
    .bind(product_id)
    .bind(qty)
    .bind(unit_cost)
    .bind(multiplier)
    .execute(conn)
    .await?;
    Ok(())
}

pub fn clean_reason(raw: &str) -> Result<String, OpError> {
    let reason: String = raw.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_owned();
    if (1..=MAX_REASON).contains(&reason.chars().count()) {
        Ok(reason)
    } else {
        Err(OpError::Rejected("reason_required"))
    }
}

fn clean_note(raw: Option<&str>) -> Result<Option<String>, OpError> {
    let Some(raw) = raw else { return Ok(None) };
    let note: String = raw.chars().filter(|c| *c == '\n' || !c.is_control()).collect::<String>().trim().to_owned();
    if note.chars().count() > MAX_NOTE {
        return Err(OpError::Rejected("note_too_long"));
    }
    Ok(Some(note).filter(|note| !note.is_empty()))
}

// --- Вскрытие пачки -----------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackOpen {
    product_id: Uuid,
    warehouse_id: Uuid,
    /// Сколько пачек вскрыто (целое число).
    packs: String,
}

/// Пачек −k, штук +k × (штук в пачке).
pub async fn pack_open(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: PackOpen) -> OpResult {
    let product = product_info(conn, op.product_id).await?;
    require_warehouse(conn, op.warehouse_id).await?;
    let packs = product.qty(StockUnit::Pack, &op.packs)?;
    let pack_size = product.pack_size.ok_or(OpError::Rejected("no_pack"))?;
    let units: String = sqlx::query_scalar("SELECT ($1::numeric * $2)::text")
        .bind(&packs)
        .bind(pack_size)
        .fetch_one(&mut *conn)
        .await?;

    let minus_packs = format!("-{packs}");
    for (unit, qty) in [(StockUnit::Pack, minus_packs.as_str()), (StockUnit::Unit, units.as_str())] {
        add_movement(
            conn,
            ctx,
            Movement {
                product_id: op.product_id,
                warehouse_id: op.warehouse_id,
                unit,
                qty,
                kind: "pack_open",
                unit_cost: None,
                supply_item_id: None,
                transfer_item_id: None,
                sale_item_id: None,
                return_item_id: None,
                reason: None,
            },
        )
        .await?;
    }
    ctx.audit(
        conn,
        "stock.pack_open",
        Some(("product", op.product_id)),
        Some(op.warehouse_id),
        json!({ "packs": packs, "units": units, "pack_size": pack_size }),
    )
    .await?;
    Ok(())
}

// --- Списание и корректировка ---------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteOff {
    product_id: Uuid,
    warehouse_id: Uuid,
    stock_unit: StockUnit,
    qty: String,
    reason: String,
}

/// Списание: брак, бой, пересорт. Себестоимость фиксируется — это потери в отчёте.
pub async fn write_off(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: WriteOff) -> OpResult {
    let product = product_info(conn, op.product_id).await?;
    require_warehouse(conn, op.warehouse_id).await?;
    let qty = product.qty(op.stock_unit, &op.qty)?;
    let reason = clean_reason(&op.reason)?;
    let cost = current_cost(conn, op.product_id, product.multiplier(op.stock_unit)).await?;

    add_movement(
        conn,
        ctx,
        Movement {
            product_id: op.product_id,
            warehouse_id: op.warehouse_id,
            unit: op.stock_unit,
            qty: &format!("-{qty}"),
            kind: "write_off",
            unit_cost: cost,
            supply_item_id: None,
            transfer_item_id: None,
            sale_item_id: None,
            return_item_id: None,
            reason: Some(&reason),
        },
    )
    .await?;
    ctx.audit(
        conn,
        "stock.write_off",
        Some(("product", op.product_id)),
        Some(op.warehouse_id),
        json!({ "stock_unit": op.stock_unit.as_str(), "qty": qty, "reason": reason }),
    )
    .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adjust {
    product_id: Uuid,
    warehouse_id: Uuid,
    stock_unit: StockUnit,
    /// Изменение со знаком: «+3» нашлось, «-2» не хватает.
    delta: String,
    reason: String,
}

/// Корректировка после пересчёта. Себестоимость не меняет.
pub async fn adjust(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: Adjust) -> OpResult {
    let product = product_info(conn, op.product_id).await?;
    require_warehouse(conn, op.warehouse_id).await?;
    let delta = product.delta(op.stock_unit, &op.delta)?;
    let reason = clean_reason(&op.reason)?;

    add_movement(
        conn,
        ctx,
        Movement {
            product_id: op.product_id,
            warehouse_id: op.warehouse_id,
            unit: op.stock_unit,
            qty: &delta,
            kind: "adjustment",
            unit_cost: None,
            supply_item_id: None,
            transfer_item_id: None,
            sale_item_id: None,
            return_item_id: None,
            reason: Some(&reason),
        },
    )
    .await?;
    ctx.audit(
        conn,
        "stock.adjust",
        Some(("product", op.product_id)),
        Some(op.warehouse_id),
        json!({ "stock_unit": op.stock_unit.as_str(), "delta": delta, "reason": reason }),
    )
    .await?;
    Ok(())
}

// --- Перемещение ------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferItem {
    id: Uuid,
    product_id: Uuid,
    stock_unit: StockUnit,
    qty: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transfer {
    id: Uuid,
    from_warehouse_id: Uuid,
    to_warehouse_id: Uuid,
    #[serde(default)]
    note: Option<String>,
    items: Vec<TransferItem>,
}

/// Перемещение — одно действие: с одного склада списывается, на другой приходит.
pub async fn transfer(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: Transfer) -> OpResult {
    if op.from_warehouse_id == op.to_warehouse_id {
        return Err(OpError::Rejected("same_warehouse"));
    }
    if op.items.is_empty() || op.items.len() > MAX_ITEMS {
        return Err(OpError::Rejected("invalid_items"));
    }
    require_warehouse(conn, op.from_warehouse_id).await?;
    require_warehouse(conn, op.to_warehouse_id).await?;
    let note = clean_note(op.note.as_deref())?;
    unique_lines(op.items.iter().map(|item| (item.product_id, item.stock_unit)))?;

    sqlx::query(
        "INSERT INTO transfers (id, from_warehouse_id, to_warehouse_id, note, user_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(op.id)
    .bind(op.from_warehouse_id)
    .bind(op.to_warehouse_id)
    .bind(&note)
    .bind(ctx.user.id)
    .bind(ctx.at)
    .execute(&mut *conn)
    .await?;

    let mut lines = Vec::with_capacity(op.items.len());
    for item in &op.items {
        let product = product_info(conn, item.product_id).await?;
        let qty = product.qty(item.stock_unit, &item.qty)?;
        sqlx::query(
            "INSERT INTO transfer_items (id, transfer_id, product_id, stock_unit, qty) VALUES ($1, $2, $3, $4, $5::numeric)",
        )
        .bind(item.id)
        .bind(op.id)
        .bind(item.product_id)
        .bind(item.stock_unit)
        .bind(&qty)
        .execute(&mut *conn)
        .await?;

        let minus = format!("-{qty}");
        for (warehouse_id, kind, signed) in
            [(op.from_warehouse_id, "transfer_out", minus.as_str()), (op.to_warehouse_id, "transfer_in", qty.as_str())]
        {
            add_movement(
                conn,
                ctx,
                Movement {
                    product_id: item.product_id,
                    warehouse_id,
                    unit: item.stock_unit,
                    qty: signed,
                    kind,
                    unit_cost: None,
                    supply_item_id: None,
                    transfer_item_id: Some(item.id),
                    sale_item_id: None,
                    return_item_id: None,
                    reason: None,
                },
            )
            .await?;
        }
        lines.push(json!({ "product_id": item.product_id, "stock_unit": item.stock_unit.as_str(), "qty": qty }));
    }

    ctx.audit(
        conn,
        "transfer.create",
        Some(("transfer", op.id)),
        Some(op.from_warehouse_id),
        json!({ "to_warehouse_id": op.to_warehouse_id, "items": lines }),
    )
    .await?;
    Ok(())
}

fn unique_lines(lines: impl Iterator<Item = (Uuid, StockUnit)>) -> OpResult {
    let mut seen = HashSet::new();
    for line in lines {
        if !seen.insert(line) {
            return Err(OpError::Rejected("duplicate_line"));
        }
    }
    Ok(())
}

// --- Поставки ---------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedItem {
    id: Uuid,
    product_id: Uuid,
    stock_unit: StockUnit,
    expected_qty: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyCreate {
    id: Uuid,
    supplier_id: Uuid,
    warehouse_id: Uuid,
    /// Ожидаемая дата, «2026-11-20».
    #[serde(default)]
    expected_on: Option<String>,
    #[serde(default)]
    note: Option<String>,
    items: Vec<PlannedItem>,
}

async fn require_supplier(conn: &mut PgConnection, supplier_id: Uuid) -> OpResult {
    let deleted: Option<bool> = sqlx::query_scalar("SELECT is_deleted FROM suppliers WHERE id = $1")
        .bind(supplier_id)
        .fetch_optional(conn)
        .await?;
    match deleted {
        None => Err(OpError::Rejected("supplier_not_found")),
        Some(true) => Err(OpError::Rejected("supplier_deleted")),
        Some(false) => Ok(()),
    }
}

/// Запланированная поставка: что и сколько ждём. Остаток не меняет; товары из неё на сайте
/// показываются как «скоро з'явиться».
pub async fn supply_create(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: SupplyCreate) -> OpResult {
    if op.items.is_empty() || op.items.len() > MAX_ITEMS {
        return Err(OpError::Rejected("invalid_items"));
    }
    require_supplier(conn, op.supplier_id).await?;
    require_warehouse(conn, op.warehouse_id).await?;
    let note = clean_note(op.note.as_deref())?;
    let expected_on = match op.expected_on.as_deref() {
        None => None,
        Some(raw) => Some(
            Date::parse(raw, time::macros::format_description!("[year]-[month]-[day]"))
                .map_err(|_| OpError::Rejected("invalid_date"))?,
        ),
    };
    unique_lines(op.items.iter().map(|item| (item.product_id, item.stock_unit)))?;

    sqlx::query(
        "INSERT INTO supplies (id, supplier_id, warehouse_id, status, expected_on, note, created_by, created_at)
         VALUES ($1, $2, $3, 'planned', $4, $5, $6, $7)",
    )
    .bind(op.id)
    .bind(op.supplier_id)
    .bind(op.warehouse_id)
    .bind(expected_on)
    .bind(&note)
    .bind(ctx.user.id)
    .bind(ctx.at)
    .execute(&mut *conn)
    .await?;

    for item in &op.items {
        let product = product_info(conn, item.product_id).await?;
        let qty = product.qty(item.stock_unit, &item.expected_qty)?;
        sqlx::query(
            "INSERT INTO supply_items (id, supply_id, product_id, stock_unit, expected_qty)
             VALUES ($1, $2, $3, $4, $5::numeric)",
        )
        .bind(item.id)
        .bind(op.id)
        .bind(item.product_id)
        .bind(item.stock_unit)
        .bind(&qty)
        .execute(&mut *conn)
        .await?;
    }
    ctx.audit(
        conn,
        "supply.create",
        Some(("supply", op.id)),
        Some(op.warehouse_id),
        json!({ "supplier_id": op.supplier_id, "items": op.items.len() }),
    )
    .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedItem {
    /// id строки. Для строки запланированной поставки сервер сопоставляет её по товару
    /// и единице учёта и сохраняет свой id; для новой строки берётся этот.
    id: Uuid,
    product_id: Uuid,
    stock_unit: StockUnit,
    /// Сколько пришло фактически. «0» — не пришло вовсе.
    received_qty: String,
    /// Закупочная цена за единицу учёта (за пачку, если принимали пачками).
    unit_cost: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyReceive {
    supply_id: Uuid,
    /// Нужны, только если поставка не была запланирована заранее.
    #[serde(default)]
    supplier_id: Option<Uuid>,
    #[serde(default)]
    warehouse_id: Option<Uuid>,
    #[serde(default)]
    note: Option<String>,
    items: Vec<ReceivedItem>,
}

/// Приёмка: работник вписывает, что и сколько пришло. Если поставку планировали заранее,
/// фактическое количество сравнивается с заказанным, и расхождения пишутся в журнал.
pub async fn supply_receive(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: SupplyReceive) -> OpResult {
    if op.items.is_empty() || op.items.len() > MAX_ITEMS {
        return Err(OpError::Rejected("invalid_items"));
    }
    unique_lines(op.items.iter().map(|item| (item.product_id, item.stock_unit)))?;
    let note = clean_note(op.note.as_deref())?;

    // Строка поставки блокируется: второе устройство, принимающее ту же поставку,
    // дождётся первого и получит отказ «уже принята».
    let existing: Option<(String, Uuid, Uuid)> =
        sqlx::query_as("SELECT status::text, warehouse_id, supplier_id FROM supplies WHERE id = $1 FOR UPDATE")
            .bind(op.supply_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some((_, warehouse_id, supplier_id)) = &existing
        && (op.warehouse_id.is_some_and(|id| id != *warehouse_id) || op.supplier_id.is_some_and(|id| id != *supplier_id))
    {
        return Err(OpError::Rejected("supply_mismatch"));
    }

    let was_planned = existing.is_some();
    let warehouse_id = match existing {
        Some((status, warehouse_id, _)) => {
            match status.as_str() {
                "planned" => {}
                "received" => return Err(OpError::Rejected("already_received")),
                _ => return Err(OpError::Rejected("supply_cancelled")),
            }
            sqlx::query(
                "UPDATE supplies SET status = 'received', received_by = $2, received_at = $3,
                        note = COALESCE($4, note)
                 WHERE id = $1",
            )
            .bind(op.supply_id)
            .bind(ctx.user.id)
            .bind(ctx.at)
            .bind(&note)
            .execute(&mut *conn)
            .await?;
            warehouse_id
        }
        None => {
            let supplier_id = op.supplier_id.ok_or(OpError::Rejected("supplier_required"))?;
            let warehouse_id = op.warehouse_id.ok_or(OpError::Rejected("warehouse_required"))?;
            require_supplier(conn, supplier_id).await?;
            require_warehouse(conn, warehouse_id).await?;
            sqlx::query(
                "INSERT INTO supplies
                    (id, supplier_id, warehouse_id, status, note, created_by, created_at, received_by, received_at)
                 VALUES ($1, $2, $3, 'received', $4, $5, $6, $5, $6)",
            )
            .bind(op.supply_id)
            .bind(supplier_id)
            .bind(warehouse_id)
            .bind(&note)
            .bind(ctx.user.id)
            .bind(ctx.at)
            .execute(&mut *conn)
            .await?;
            warehouse_id
        }
    };

    let mut discrepancies = Vec::new();
    let mut received_lines = HashSet::new();
    for item in &op.items {
        let product = product_info(conn, item.product_id).await?;
        product.require_unit(item.stock_unit)?;
        let received = parse_qty(&item.received_qty, product.fraction_allowed(item.stock_unit));
        let received_zero = parse_decimal_zero(&item.received_qty);
        let received = match (received, received_zero) {
            (Some(qty), _) => qty,
            (None, true) => "0".to_owned(),
            (None, false) => return Err(OpError::Rejected("invalid_qty")),
        };
        let unit_cost = parse_unit_cost(&item.unit_cost).ok_or(OpError::Rejected("invalid_cost"))?;

        // Строка из плана — обновляется; новая — добавляется с ожидаемым количеством NULL.
        let planned: Option<(Uuid, Option<String>)> = sqlx::query_as(
            "UPDATE supply_items SET received_qty = $4::numeric, unit_cost = $5::numeric
             WHERE supply_id = $1 AND product_id = $2 AND stock_unit = $3
             RETURNING id, expected_qty::text",
        )
        .bind(op.supply_id)
        .bind(item.product_id)
        .bind(item.stock_unit)
        .bind(&received)
        .bind(&unit_cost)
        .fetch_optional(&mut *conn)
        .await?;
        let (item_id, expected) = match planned {
            Some(row) => row,
            None => {
                sqlx::query(
                    "INSERT INTO supply_items (id, supply_id, product_id, stock_unit, received_qty, unit_cost)
                     VALUES ($1, $2, $3, $4, $5::numeric, $6::numeric)",
                )
                .bind(item.id)
                .bind(op.supply_id)
                .bind(item.product_id)
                .bind(item.stock_unit)
                .bind(&received)
                .bind(&unit_cost)
                .execute(&mut *conn)
                .await?;
                (item.id, None)
            }
        };
        received_lines.insert((item.product_id, item.stock_unit));

        // Расхождение: пришло не столько, сколько ждали, или пришло то, чего в плане не было.
        let differs = match &expected {
            Some(expected) => normalize(expected) != normalize(&received),
            None => was_planned && received != "0",
        };
        if differs {
            discrepancies.push(json!({
                "product_id": item.product_id,
                "stock_unit": item.stock_unit.as_str(),
                "expected": expected,
                "received": received,
            }));
        }

        if received != "0" {
            let multiplier = product.multiplier(item.stock_unit);
            update_average_cost(conn, item.product_id, &received, &unit_cost, multiplier).await?;
            add_movement(
                conn,
                ctx,
                Movement {
                    product_id: item.product_id,
                    warehouse_id,
                    unit: item.stock_unit,
                    qty: &received,
                    kind: "receipt",
                    unit_cost: Some(unit_cost.clone()),
                    supply_item_id: Some(item_id),
                    transfer_item_id: None,
                    sale_item_id: None,
                    return_item_id: None,
                    reason: None,
                },
            )
            .await?;
        }
    }

    // Строки плана, которых нет в приёмке, не пришли вовсе.
    let missing: Vec<(Uuid, StockUnit, String)> = sqlx::query_as(
        "UPDATE supply_items SET received_qty = 0, unit_cost = 0
         WHERE supply_id = $1 AND received_qty IS NULL
         RETURNING product_id, stock_unit, expected_qty::text",
    )
    .bind(op.supply_id)
    .fetch_all(&mut *conn)
    .await?;
    for (product_id, unit, expected) in missing {
        if !received_lines.contains(&(product_id, unit)) {
            discrepancies.push(json!({
                "product_id": product_id,
                "stock_unit": unit.as_str(),
                "expected": expected,
                "received": "0",
            }));
        }
    }

    ctx.audit(
        conn,
        "supply.receive",
        Some(("supply", op.supply_id)),
        Some(warehouse_id),
        json!({ "items": op.items.len() }),
    )
    .await?;
    if !discrepancies.is_empty() {
        ctx.audit(
            conn,
            "supply.discrepancy",
            Some(("supply", op.supply_id)),
            Some(warehouse_id),
            json!({ "lines": discrepancies }),
        )
        .await?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyCancel {
    supply_id: Uuid,
}

pub async fn supply_cancel(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: SupplyCancel) -> OpResult {
    let status: Option<String> = sqlx::query_scalar("SELECT status::text FROM supplies WHERE id = $1 FOR UPDATE")
        .bind(op.supply_id)
        .fetch_optional(&mut *conn)
        .await?;
    match status.as_deref() {
        None => return Err(OpError::Rejected("supply_not_found")),
        Some("planned") => {}
        Some("received") => return Err(OpError::Rejected("already_received")),
        Some(_) => return Err(OpError::Rejected("supply_cancelled")),
    }
    sqlx::query("UPDATE supplies SET status = 'cancelled' WHERE id = $1").bind(op.supply_id).execute(&mut *conn).await?;
    ctx.audit(conn, "supply.cancel", Some(("supply", op.supply_id)), None, json!({})).await?;
    Ok(())
}

/// «0», «0.0», «0,000» — ноль.
fn parse_decimal_zero(raw: &str) -> bool {
    let raw = raw.trim().replace(',', ".");
    !raw.is_empty()
        && raw.chars().all(|c| c == '0' || c == '.')
        && raw.matches('.').count() <= 1
        && !raw.starts_with('.')
        && !raw.ends_with('.')
}

/// Нормализует десятичную строку из базы («5.000» → «5») для сравнения.
fn normalize(value: &str) -> String {
    if value.contains('.') {
        let trimmed = value.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_owned()
    } else {
        value.to_owned()
    }
}

/// Пересчитывает кэш остатков: остаток на начало + движения, плюс резервы открытых заказов. Команда сервера: на случай, если кэш когда-нибудь
/// разойдётся с движениями. Возвращает число строк, которые пришлось исправить.
pub async fn rebuild_balances(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    let mut tx = db.begin().await?;
    super::lock_changes(&mut tx).await?;
    let result = sqlx::query(
        "WITH sums AS (
            -- остаток на начало (движения, ушедшие в архив) + движения, оставшиеся в базе
            SELECT product_id, warehouse_id, sum(units) AS units, sum(packs) AS packs FROM (
                SELECT product_id, warehouse_id,
                       COALESCE(sum(qty) FILTER (WHERE stock_unit = 'unit'), 0) AS units,
                       COALESCE(sum(qty) FILTER (WHERE stock_unit = 'pack'), 0) AS packs
                FROM stock_movements GROUP BY product_id, warehouse_id
              UNION ALL
                SELECT product_id, warehouse_id, qty_units, qty_packs FROM stock_opening_balances
            ) t GROUP BY product_id, warehouse_id
         ), reserves AS (
            -- резерв — строки подтверждённых и собранных заказов на их складе
            SELECT i.product_id, o.warehouse_id,
                   COALESCE(sum(i.qty) FILTER (WHERE i.stock_unit = 'unit'), 0) AS units,
                   COALESCE(sum(i.qty) FILTER (WHERE i.stock_unit = 'pack'), 0) AS packs
            FROM order_items i JOIN orders o ON o.id = i.order_id
            WHERE o.status IN ('confirmed', 'assembled')
            GROUP BY i.product_id, o.warehouse_id
         ), keys AS (
            SELECT product_id, warehouse_id FROM sums
            UNION SELECT product_id, warehouse_id FROM reserves
            UNION SELECT product_id, warehouse_id FROM stock_balances
         ), target AS (
            SELECT k.product_id, k.warehouse_id,
                   COALESCE(s.units, 0) AS units, COALESCE(s.packs, 0) AS packs,
                   COALESCE(r.units, 0) AS reserved_units, COALESCE(r.packs, 0) AS reserved_packs
            FROM keys k
            LEFT JOIN sums s ON s.product_id = k.product_id AND s.warehouse_id = k.warehouse_id
            LEFT JOIN reserves r ON r.product_id = k.product_id AND r.warehouse_id = k.warehouse_id
         )
         INSERT INTO stock_balances (product_id, warehouse_id, qty_units, qty_packs, reserved_units, reserved_packs)
         SELECT product_id, warehouse_id, units, packs, reserved_units, reserved_packs FROM target
         ON CONFLICT (product_id, warehouse_id) DO UPDATE
            SET qty_units = EXCLUDED.qty_units, qty_packs = EXCLUDED.qty_packs,
                reserved_units = EXCLUDED.reserved_units, reserved_packs = EXCLUDED.reserved_packs,
                updated_at = now()
            WHERE (stock_balances.qty_units, stock_balances.qty_packs,
                   stock_balances.reserved_units, stock_balances.reserved_packs)
               IS DISTINCT FROM
                  (EXCLUDED.qty_units, EXCLUDED.qty_packs, EXCLUDED.reserved_units, EXCLUDED.reserved_packs)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_normalization() {
        assert!(parse_decimal_zero("0"));
        assert!(parse_decimal_zero("0,000"));
        assert!(!parse_decimal_zero("0.1"));
        assert!(!parse_decimal_zero(""));
        assert!(!parse_decimal_zero("."));
        assert_eq!(normalize("5.000"), "5");
        assert_eq!(normalize("2.500"), "2.5");
        assert_eq!(normalize("10"), "10");
    }
}
