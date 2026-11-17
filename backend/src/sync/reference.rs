//! Справочники с устройства: склады, поставщики, категории, товары.
//!
//! Правка — «побеждает последняя по времени правка, отдельно по каждому полю»
//! (docs/design.md, раздел 9.4). У каждой записи хранится время последней правки каждого поля.
//! Пришедшее значение поля применяется, только если оно новее сохранённого; иначе правка
//! этого поля проиграла. В журнал попадают обе: и применённые поля, и проигравшие.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::{OpCtx, OpError, OpResult};
use crate::catalog::{self, CatalogError, NewCategory, NewProduct, clean_line, clean_text, parse_money};
use crate::permissions;

/// Поле, которое можно править: имя колонки и приведение типа в SQL.
struct Field {
    name: &'static str,
    cast: &'static str,
}

/// Проверенное значение поля: текст для SQL (или NULL).
struct Value_ {
    field: &'static Field,
    value: Option<String>,
}

const WAREHOUSE_FIELDS: &[Field] = &[Field { name: "name", cast: "text" }, Field { name: "address", cast: "text" }];
const SUPPLIER_FIELDS: &[Field] = &[
    Field { name: "name", cast: "text" },
    Field { name: "contact_person", cast: "text" },
    Field { name: "phone", cast: "text" },
    Field { name: "note", cast: "text" },
];
const CATEGORY_FIELDS: &[Field] = &[
    Field { name: "name", cast: "text" },
    Field { name: "parent_id", cast: "uuid" },
    Field { name: "sort_order", cast: "int" },
];
const PRODUCT_FIELDS: &[Field] = &[
    Field { name: "category_id", cast: "uuid" },
    Field { name: "name", cast: "text" },
    Field { name: "description", cast: "text" },
    Field { name: "unit_code", cast: "text" },
    Field { name: "price", cast: "numeric" },
    Field { name: "pack_size", cast: "int" },
    Field { name: "pack_price", cast: "numeric" },
    Field { name: "is_published", cast: "boolean" },
];
const IS_DELETED: Field = Field { name: "is_deleted", cast: "boolean" };
const IS_ACTIVE: Field = Field { name: "is_active", cast: "boolean" };

fn field(list: &'static [Field], name: &str) -> Result<&'static Field, OpError> {
    list.iter().find(|field| field.name == name).ok_or(OpError::Rejected("unknown_field"))
}

/// Итог правки: какие поля применены, какие проиграли более поздней правке.
struct Outcome {
    applied: Map<String, Value>,
    ignored: Vec<&'static str>,
}

/// Применяет поля по правилу «побеждает последняя правка». `table` — константа из кода.
async fn apply_lww(
    conn: &mut PgConnection,
    table: &'static str,
    id: Uuid,
    at: OffsetDateTime,
    values: Vec<Value_>,
) -> Result<Outcome, OpError> {
    let stamps: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT field_updated_at FROM {table} WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let stamps = stamps.ok_or(OpError::Rejected("not_found"))?;

    let mut winners = Vec::new();
    let mut outcome = Outcome { applied: Map::new(), ignored: Vec::new() };
    for value in values {
        let previous = stamps
            .get(value.field.name)
            .and_then(Value::as_str)
            .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok());
        // При равном времени побеждает пришедшая позже.
        if previous.is_none_or(|previous| previous <= at) {
            outcome.applied.insert(value.field.name.to_owned(), value.value.clone().map_or(Value::Null, Value::String));
            winners.push(value);
        } else {
            outcome.ignored.push(value.field.name);
        }
    }
    if winners.is_empty() {
        return Ok(outcome);
    }

    let at_text = at.format(&Rfc3339).map_err(|_| OpError::Rejected("invalid_time"))?;
    let stamp_update: Map<String, Value> =
        winners.iter().map(|value| (value.field.name.to_owned(), Value::String(at_text.clone()))).collect();
    let assignments: Vec<String> = winners
        .iter()
        .enumerate()
        .map(|(index, value)| format!("{} = ${}::{}", value.field.name, index + 2, value.field.cast))
        .collect();
    let stamps_param = winners.len() + 2;
    let sql = format!(
        "UPDATE {table} SET {}, field_updated_at = field_updated_at || ${stamps_param}::jsonb, updated_at = now()
         WHERE id = $1",
        assignments.join(", ")
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id);
    for value in &winners {
        query = query.bind(value.value.as_deref());
    }
    query.bind(Value::Object(stamp_update)).execute(&mut *conn).await?;
    Ok(outcome)
}

async fn log_update(
    conn: &mut PgConnection,
    ctx: &OpCtx<'_>,
    action: &'static str,
    entity: &'static str,
    id: Uuid,
    outcome: &Outcome,
) -> OpResult {
    ctx.audit(
        conn,
        action,
        Some((entity, id)),
        None,
        json!({ "applied": outcome.applied, "ignored_older_than_saved": outcome.ignored }),
    )
    .await?;
    Ok(())
}

fn require(ctx: &OpCtx<'_>, permission: i64) -> OpResult {
    if ctx.user.can(permission) { Ok(()) } else { Err(OpError::Rejected("permission_required")) }
}

/// Строка из JSON: строка, null (только если `nullable`) или отказ.
fn text(value: &Value, nullable: bool) -> Result<Option<String>, OpError> {
    match value {
        Value::String(text) => Ok(Some(text.clone())),
        Value::Null if nullable => Ok(None),
        _ => Err(OpError::Rejected("invalid_field")),
    }
}

fn line(value: &Value, max: usize, nullable: bool) -> Result<Option<String>, OpError> {
    let Some(raw) = text(value, nullable)? else { return Ok(None) };
    let cleaned = clean_line(&raw);
    if cleaned.is_empty() {
        return if nullable { Ok(None) } else { Err(OpError::Rejected("invalid_field")) };
    }
    if cleaned.chars().count() > max {
        return Err(OpError::Rejected("invalid_field"));
    }
    Ok(Some(cleaned))
}

fn uuid_value(value: &Value, nullable: bool) -> Result<Option<String>, OpError> {
    let Some(raw) = text(value, nullable)? else { return Ok(None) };
    raw.parse::<Uuid>().map(|id| Some(id.to_string())).map_err(|_| OpError::Rejected("invalid_field"))
}

fn bool_value(value: &Value) -> Result<Option<String>, OpError> {
    value.as_bool().map(|flag| Some(flag.to_string())).ok_or(OpError::Rejected("invalid_field"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    id: Uuid,
    fields: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ById {
    id: Uuid,
}

fn non_empty(update: &Update) -> OpResult {
    if update.fields.is_empty() || update.fields.len() > 16 {
        return Err(OpError::Rejected("invalid_field"));
    }
    Ok(())
}

/// Мягкое удаление или выключение записи: таблица, сущность в журнале, действие, поле и значение.
struct Flag {
    table: &'static str,
    entity: &'static str,
    action: &'static str,
    field: &'static Field,
    value: bool,
}

/// Одно булево поле по тому же правилу «побеждает последняя правка».
async fn set_flag(conn: &mut PgConnection, ctx: &OpCtx<'_>, flag: Flag, id: Uuid) -> OpResult {
    let values = vec![Value_ { field: flag.field, value: Some(flag.value.to_string()) }];
    let outcome = apply_lww(conn, flag.table, id, ctx.at, values).await?;
    log_update(conn, ctx, flag.action, flag.entity, id, &outcome).await
}

// --- Склады -------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarehouseCreate {
    id: Uuid,
    name: String,
    #[serde(default)]
    address: Option<String>,
}

pub async fn warehouse_create(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: WarehouseCreate) -> OpResult {
    require(ctx, permissions::WAREHOUSES_EDIT)?;
    let name = line(&Value::String(op.name), 100, false)?.ok_or(OpError::Rejected("invalid_field"))?;
    let address = line(&op.address.map_or(Value::Null, Value::String), 300, true)?;
    if name_taken(conn, "warehouses", &name, op.id).await? {
        return Err(OpError::Rejected("name_taken"));
    }
    sqlx::query("INSERT INTO warehouses (id, name, address) VALUES ($1, $2, $3)")
        .bind(op.id)
        .bind(&name)
        .bind(&address)
        .execute(&mut *conn)
        .await?;
    ctx.audit(conn, "warehouse.create", Some(("warehouse", op.id)), None, json!({ "name": name, "address": address }))
        .await?;
    Ok(())
}

pub async fn warehouse_update(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: Update) -> OpResult {
    require(ctx, permissions::WAREHOUSES_EDIT)?;
    non_empty(&op)?;
    let mut values = Vec::new();
    for (name, value) in &op.fields {
        let field = field(WAREHOUSE_FIELDS, name)?;
        let checked = match field.name {
            "name" => {
                let name = line(value, 100, false)?;
                if let Some(name) = &name
                    && name_taken(conn, "warehouses", name, op.id).await?
                {
                    return Err(OpError::Rejected("name_taken"));
                }
                name
            }
            _ => line(value, 300, true)?,
        };
        values.push(Value_ { field, value: checked });
    }
    let outcome = apply_lww(conn, "warehouses", op.id, ctx.at, values).await?;
    log_update(conn, ctx, "warehouse.update", "warehouse", op.id, &outcome).await
}

pub async fn warehouse_deactivate(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: ById) -> OpResult {
    require(ctx, permissions::WAREHOUSES_EDIT)?;
    let flag = Flag { table: "warehouses", entity: "warehouse", action: "warehouse.deactivate", field: &IS_ACTIVE, value: false };
    set_flag(conn, ctx, flag, op.id).await
}

async fn name_taken(conn: &mut PgConnection, table: &'static str, name: &str, except: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE name = $1 AND id <> $2)"
    )))
    .bind(name)
    .bind(except)
    .fetch_one(conn)
    .await
}

// --- Поставщики ------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplierCreate {
    id: Uuid,
    name: String,
    #[serde(default)]
    contact_person: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

fn supplier_value(field: &'static Field, value: &Value) -> Result<Option<String>, OpError> {
    match field.name {
        "name" => line(value, 200, false),
        "contact_person" => line(value, 200, true),
        "phone" => line(value, 50, true),
        _ => {
            let Some(raw) = text(value, true)? else { return Ok(None) };
            let note = clean_text(&raw);
            if note.chars().count() > 2000 {
                return Err(OpError::Rejected("invalid_field"));
            }
            Ok(Some(note).filter(|note| !note.is_empty()))
        }
    }
}

pub async fn supplier_create(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: SupplierCreate) -> OpResult {
    require(ctx, permissions::SUPPLIERS_EDIT)?;
    let opt = |value: Option<String>| value.map_or(Value::Null, Value::String);
    let name = supplier_value(&SUPPLIER_FIELDS[0], &Value::String(op.name))?.ok_or(OpError::Rejected("invalid_field"))?;
    let contact = supplier_value(&SUPPLIER_FIELDS[1], &opt(op.contact_person))?;
    let phone = supplier_value(&SUPPLIER_FIELDS[2], &opt(op.phone))?;
    let note = supplier_value(&SUPPLIER_FIELDS[3], &opt(op.note))?;
    sqlx::query("INSERT INTO suppliers (id, name, contact_person, phone, note) VALUES ($1, $2, $3, $4, $5)")
        .bind(op.id)
        .bind(&name)
        .bind(&contact)
        .bind(&phone)
        .bind(&note)
        .execute(&mut *conn)
        .await?;
    ctx.audit(conn, "supplier.create", Some(("supplier", op.id)), None, json!({ "name": name, "phone": phone }))
        .await?;
    Ok(())
}

pub async fn supplier_update(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: Update) -> OpResult {
    require(ctx, permissions::SUPPLIERS_EDIT)?;
    non_empty(&op)?;
    let mut values = Vec::new();
    for (name, value) in &op.fields {
        let field = field(SUPPLIER_FIELDS, name)?;
        values.push(Value_ { field, value: supplier_value(field, value)? });
    }
    let outcome = apply_lww(conn, "suppliers", op.id, ctx.at, values).await?;
    log_update(conn, ctx, "supplier.update", "supplier", op.id, &outcome).await
}

pub async fn supplier_delete(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: ById) -> OpResult {
    require(ctx, permissions::SUPPLIERS_EDIT)?;
    let flag = Flag { table: "suppliers", entity: "supplier", action: "supplier.delete", field: &IS_DELETED, value: true };
    set_flag(conn, ctx, flag, op.id).await
}

// --- Категории --------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryCreate {
    id: Uuid,
    #[serde(default)]
    parent_id: Option<Uuid>,
    name: String,
    #[serde(default)]
    sort_order: i32,
}

pub async fn category_create(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: CategoryCreate) -> OpResult {
    require(ctx, permissions::CATEGORIES_EDIT)?;
    let new = NewCategory { id: op.id, parent_id: op.parent_id, name: op.name, sort_order: op.sort_order };
    match catalog::create_category(conn, &ctx.user.actor(), new).await {
        Ok(true) => Ok(()),
        // Категория с таким id уже есть, но это не повтор операции (повторы отсеивает реестр).
        Ok(false) => Err(OpError::Rejected("already_exists")),
        Err(error) => Err(catalog_error(error)),
    }
}

pub async fn category_update(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: Update) -> OpResult {
    require(ctx, permissions::CATEGORIES_EDIT)?;
    non_empty(&op)?;
    let mut values = Vec::new();
    for (name, value) in &op.fields {
        let field = field(CATEGORY_FIELDS, name)?;
        let checked = match field.name {
            "name" => line(value, 100, false)?,
            "sort_order" => Some(
                value
                    .as_i64()
                    .filter(|order| (-100_000..=100_000).contains(order))
                    .ok_or(OpError::Rejected("invalid_field"))?
                    .to_string(),
            ),
            _ => {
                let parent = uuid_value(value, true)?;
                if let Some(parent) = &parent {
                    let parent: Uuid = parent.parse().map_err(|_| OpError::Rejected("invalid_field"))?;
                    check_parent(conn, op.id, parent).await?;
                }
                parent
            }
        };
        values.push(Value_ { field, value: checked });
    }
    let outcome = apply_lww(conn, "categories", op.id, ctx.at, values).await?;
    log_update(conn, ctx, "category.update", "category", op.id, &outcome).await
}

/// Новый родитель существует, не удалён и не лежит внутри самой категории:
/// иначе дерево замкнулось бы в кольцо.
async fn check_parent(conn: &mut PgConnection, category: Uuid, parent: Uuid) -> OpResult {
    if parent == category {
        return Err(OpError::Rejected("category_cycle"));
    }
    let deleted: Option<bool> = sqlx::query_scalar("SELECT is_deleted FROM categories WHERE id = $1")
        .bind(parent)
        .fetch_optional(&mut *conn)
        .await?;
    match deleted {
        None => return Err(OpError::Rejected("parent_not_found")),
        Some(true) => return Err(OpError::Rejected("parent_deleted")),
        Some(false) => {}
    }
    let cycle: bool = sqlx::query_scalar(
        "WITH RECURSIVE ancestors AS (
            SELECT id, parent_id, 1 AS depth FROM categories WHERE id = $1
          UNION ALL
            SELECT c.id, c.parent_id, a.depth + 1 FROM categories c JOIN ancestors a ON c.id = a.parent_id
            WHERE a.depth < 100
         )
         SELECT EXISTS (SELECT 1 FROM ancestors WHERE id = $2)",
    )
    .bind(parent)
    .bind(category)
    .fetch_one(conn)
    .await?;
    if cycle { Err(OpError::Rejected("category_cycle")) } else { Ok(()) }
}

pub async fn category_delete(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: ById) -> OpResult {
    require(ctx, permissions::CATEGORIES_EDIT)?;
    let flag = Flag { table: "categories", entity: "category", action: "category.delete", field: &IS_DELETED, value: true };
    set_flag(conn, ctx, flag, op.id).await
}

// --- Товары -----------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackInput {
    size: i32,
    price: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductCreate {
    id: Uuid,
    category_id: Uuid,
    name: String,
    #[serde(default)]
    description: String,
    unit_code: String,
    price: String,
    #[serde(default)]
    pack: Option<PackInput>,
    #[serde(default = "yes")]
    is_published: bool,
}

fn yes() -> bool {
    true
}

/// Новый товар — право «товары». Начальная цена задаётся вместе с ним (**предложение**:
/// отдельное право на цены нужно, чтобы менять цену уже существующего товара).
pub async fn product_create(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: ProductCreate) -> OpResult {
    require(ctx, permissions::PRODUCTS_EDIT)?;
    let new = NewProduct {
        id: op.id,
        category_id: op.category_id,
        name: op.name,
        description: op.description,
        unit_code: op.unit_code,
        price: op.price,
        pack: op.pack.map(|pack| (pack.size, pack.price)),
        is_published: op.is_published,
    };
    match catalog::create_product(conn, &ctx.user.actor(), new).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(OpError::Rejected("already_exists")),
        Err(error) => Err(catalog_error(error)),
    }
}

/// Правка товара. Цена и цена пачки — только с правом «цены»: если в правке есть цена,
/// а права нет, отклоняется вся правка (а не только цена), чтобы работник сразу видел отказ.
/// Единицу измерения и размер пачки нельзя менять, когда по товару уже есть движения:
/// иначе старые остатки пересчитались бы неверно.
pub async fn product_update(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: Update) -> OpResult {
    require(ctx, permissions::PRODUCTS_EDIT)?;
    non_empty(&op)?;
    let touches_price = op.fields.keys().any(|name| name == "price" || name == "pack_price");
    if touches_price && !ctx.user.can(permissions::PRICES_EDIT) {
        return Err(OpError::Rejected("permission_required"));
    }
    let touches_unit = op.fields.keys().any(|name| name == "unit_code" || name == "pack_size");
    if touches_unit {
        let has_movements: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM stock_movements WHERE product_id = $1)")
                .bind(op.id)
                .fetch_one(&mut *conn)
                .await?;
        if has_movements {
            return Err(OpError::Rejected("unit_frozen"));
        }
    }

    let mut values = Vec::new();
    for (name, value) in &op.fields {
        let field = field(PRODUCT_FIELDS, name)?;
        let checked = match field.name {
            "category_id" => {
                let category = uuid_value(value, false)?.ok_or(OpError::Rejected("invalid_field"))?;
                let deleted: Option<bool> = sqlx::query_scalar("SELECT is_deleted FROM categories WHERE id = $1::uuid")
                    .bind(&category)
                    .fetch_optional(&mut *conn)
                    .await?;
                if deleted != Some(false) {
                    return Err(OpError::Rejected("category_not_found"));
                }
                Some(category)
            }
            "name" => line(value, 200, false)?,
            "description" => {
                let description = clean_text(&text(value, false)?.unwrap_or_default());
                if description.chars().count() > 5000 {
                    return Err(OpError::Rejected("invalid_field"));
                }
                Some(description)
            }
            "unit_code" => {
                let code = text(value, false)?.unwrap_or_default();
                let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM units WHERE code = $1)")
                    .bind(&code)
                    .fetch_one(&mut *conn)
                    .await?;
                if !exists {
                    return Err(OpError::Rejected("unit_not_found"));
                }
                Some(code)
            }
            "price" => Some(money(value, false)?.ok_or(OpError::Rejected("invalid_field"))?),
            "pack_price" => money(value, true)?,
            "pack_size" => match value {
                Value::Null => None,
                _ => Some(
                    value
                        .as_i64()
                        .filter(|size| (2..=1_000_000).contains(size))
                        .ok_or(OpError::Rejected("invalid_field"))?
                        .to_string(),
                ),
            },
            _ => bool_value(value)?,
        };
        values.push(Value_ { field, value: checked });
    }
    let outcome = match apply_lww(conn, "products", op.id, ctx.at, values).await {
        Ok(outcome) => outcome,
        // Задан только размер пачки или только её цена — так нельзя.
        Err(OpError::Db(error))
            if error.as_database_error().and_then(|e| e.constraint()) == Some("products_pack_both_or_none") =>
        {
            return Err(OpError::Rejected("pack_incomplete"));
        }
        Err(error) => return Err(error),
    };

    // Итоговая карточка должна остаться осмысленной: фасовка задана целиком и только у штучного.
    let (pack_size, pack_price, allow_fraction): (Option<i32>, Option<String>, bool) = sqlx::query_as(
        "SELECT p.pack_size, p.pack_price::text, u.allow_fraction FROM products p JOIN units u ON u.code = p.unit_code
         WHERE p.id = $1",
    )
    .bind(op.id)
    .fetch_one(&mut *conn)
    .await?;
    if pack_size.is_some() != pack_price.is_some() {
        return Err(OpError::Rejected("pack_incomplete"));
    }
    if pack_size.is_some() && allow_fraction {
        return Err(OpError::Rejected("pack_requires_countable_unit"));
    }
    log_update(conn, ctx, "product.update", "product", op.id, &outcome).await
}

fn money(value: &Value, nullable: bool) -> Result<Option<String>, OpError> {
    let Some(raw) = text(value, nullable)? else { return Ok(None) };
    parse_money(&raw).map(Some).ok_or(OpError::Rejected("invalid_field"))
}

pub async fn product_delete(conn: &mut PgConnection, ctx: &OpCtx<'_>, op: ById) -> OpResult {
    require(ctx, permissions::PRODUCTS_EDIT)?;
    let flag = Flag { table: "products", entity: "product", action: "product.delete", field: &IS_DELETED, value: true };
    set_flag(conn, ctx, flag, op.id).await
}

fn catalog_error(error: CatalogError) -> OpError {
    match error {
        CatalogError::Validation(fields) => {
            // Первое поле с ошибкой определяет код: «category_id.not_found» → category_not_found и т. п.
            let code = match fields.iter().next().map(|(field, code)| (*field, *code)) {
                Some(("category_id", _)) => "category_not_found",
                Some(("parent_id", "deleted")) => "parent_deleted",
                Some(("parent_id", _)) => "parent_not_found",
                Some(("unit_code", _)) => "unit_not_found",
                Some(("pack", "unit_not_countable")) => "pack_requires_countable_unit",
                _ => "invalid_field",
            };
            OpError::Rejected(code)
        }
        CatalogError::Db(error) => OpError::Db(error),
    }
}
