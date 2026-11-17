//! Разбор операции по типу и вызов нужного действия. Список типов и их данные —
//! docs/design.md, раздел 12.3.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{OpCtx, OpError, OpResult, reference, stock};
use crate::sales::{orders, pos};

pub async fn apply(conn: &mut PgConnection, ctx: &OpCtx<'_>, op_type: &str, payload: &Value) -> OpResult {
    match op_type {
        "stock.pack_open" => stock::pack_open(conn, ctx, parse(payload)?).await,
        "stock.write_off" => stock::write_off(conn, ctx, parse(payload)?).await,
        "stock.adjust" => stock::adjust(conn, ctx, parse(payload)?).await,
        "transfer.create" => stock::transfer(conn, ctx, parse(payload)?).await,
        "supply.create" => stock::supply_create(conn, ctx, parse(payload)?).await,
        "supply.receive" => stock::supply_receive(conn, ctx, parse(payload)?).await,
        "supply.cancel" => stock::supply_cancel(conn, ctx, parse(payload)?).await,

        "warehouse.create" => reference::warehouse_create(conn, ctx, parse(payload)?).await,
        "warehouse.update" => reference::warehouse_update(conn, ctx, parse(payload)?).await,
        "warehouse.deactivate" => reference::warehouse_deactivate(conn, ctx, parse(payload)?).await,
        "supplier.create" => reference::supplier_create(conn, ctx, parse(payload)?).await,
        "supplier.update" => reference::supplier_update(conn, ctx, parse(payload)?).await,
        "supplier.delete" => reference::supplier_delete(conn, ctx, parse(payload)?).await,
        "category.create" => reference::category_create(conn, ctx, parse(payload)?).await,
        "category.update" => reference::category_update(conn, ctx, parse(payload)?).await,
        "category.delete" => reference::category_delete(conn, ctx, parse(payload)?).await,
        "product.create" => reference::product_create(conn, ctx, parse(payload)?).await,
        "product.update" => reference::product_update(conn, ctx, parse(payload)?).await,
        "product.delete" => reference::product_delete(conn, ctx, parse(payload)?).await,

        "session.warehouse_switch" => warehouse_switch(conn, ctx, parse(payload)?).await,

        "order.create" => orders::create(conn, ctx, parse(payload)?).await,
        "order.update" => orders::update(conn, ctx, parse(payload)?).await,
        "order.set_status" => orders::set_status(conn, ctx, parse(payload)?).await,
        "sale.create" => pos::sale(conn, ctx, parse(payload)?).await,
        "return.create" => pos::return_goods(conn, ctx, parse(payload)?).await,
        _ => Err(OpError::Rejected("unknown_type")),
    }
}

fn parse<T: DeserializeOwned>(payload: &Value) -> Result<T, OpError> {
    serde_json::from_value(payload.clone()).map_err(|_| OpError::Rejected("invalid_payload"))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WarehouseSwitch {
    warehouse_id: Uuid,
}

/// Сотрудник переключился на другой склад. Ничего не меняет — только запись в журнал.
async fn warehouse_switch(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: WarehouseSwitch) -> OpResult {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM warehouses WHERE id = $1)")
        .bind(op.warehouse_id)
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Err(OpError::Rejected("warehouse_not_found"));
    }
    ctx.audit(conn, "session.warehouse_switch", None, Some(op.warehouse_id), json!({})).await?;
    Ok(())
}
