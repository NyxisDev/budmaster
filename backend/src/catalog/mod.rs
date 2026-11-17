//! Каталог: категории, товары, фото.
//!
//! Здесь лежат функции создания категорий и товаров. Сейчас ими пользуется только
//! команда импорта; на этапе синхронизации их же будет вызывать приём операций от приложения
//! (тогда же появятся правка, удаление и правило «побеждает последняя правка»).

pub mod images;
pub mod import;
pub mod public;
pub mod slug;

use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, Entry};
use crate::error::{ApiError, FieldErrors};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("ошибка в данных: {0:?}")]
    Validation(FieldErrors),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl CatalogError {
    fn field(name: &'static str, code: &'static str) -> Self {
        Self::Validation(FieldErrors::from([(name, code)]))
    }
}

impl From<CatalogError> for ApiError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Validation(fields) => Self::Validation(fields),
            CatalogError::Db(error) => error.into(),
        }
    }
}

pub struct NewCategory {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub sort_order: i32,
}

/// Создаёт категорию. Возвращает `false`, если категория с таким id уже есть
/// (повторная отправка той же записи ничего не дублирует).
pub async fn create_category(
    conn: &mut PgConnection,
    actor: &Actor,
    new: NewCategory,
) -> Result<bool, CatalogError> {
    // Запись с таким id уже есть — это повтор, а не новая категория. Проверяется первым:
    // повтор проходит, даже если категорию или её родителя с тех пор удалили.
    if exists(conn, "categories", new.id).await? {
        return Ok(false);
    }
    let name = clean_line(&new.name);
    if !(1..=100).contains(&name.chars().count()) {
        return Err(CatalogError::field("name", "invalid"));
    }
    if let Some(parent_id) = new.parent_id {
        let parent_deleted: Option<bool> = sqlx::query_scalar("SELECT is_deleted FROM categories WHERE id = $1")
            .bind(parent_id)
            .fetch_optional(&mut *conn)
            .await?;
        match parent_deleted {
            Some(false) => {}
            Some(true) => return Err(CatalogError::field("parent_id", "deleted")),
            None => return Err(CatalogError::field("parent_id", "not_found")),
        }
    }

    let slug = slug::unique_slug(conn, &name).await?;
    let inserted = sqlx::query(
        "INSERT INTO categories (id, parent_id, name, slug, sort_order) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(new.id)
    .bind(new.parent_id)
    .bind(&name)
    .bind(&slug)
    .bind(new.sort_order)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(false);
    }

    audit::record(
        &mut *conn,
        Entry::new(actor, "category.create")
            .entity("category", new.id)
            .details(json!({ "name": name, "slug": slug, "parent_id": new.parent_id })),
    )
    .await?;
    Ok(true)
}

pub struct NewProduct {
    pub id: Uuid,
    pub category_id: Uuid,
    pub name: String,
    pub description: String,
    pub unit_code: String,
    /// Цена десятичной строкой: «185.00». Деньги не проходят через числа с плавающей точкой.
    pub price: String,
    /// Фасовка: сколько штук в пачке и цена пачки.
    pub pack: Option<(i32, String)>,
    pub is_published: bool,
}

/// Создаёт товар. Возвращает `false`, если товар с таким id уже есть.
pub async fn create_product(
    conn: &mut PgConnection,
    actor: &Actor,
    new: NewProduct,
) -> Result<bool, CatalogError> {
    if exists(conn, "products", new.id).await? {
        return Ok(false);
    }
    let name = clean_line(&new.name);
    let description = clean_text(&new.description);

    let mut fields = FieldErrors::new();
    if !(1..=200).contains(&name.chars().count()) {
        fields.insert("name", "invalid");
    }
    if description.chars().count() > 5000 {
        fields.insert("description", "too_long");
    }
    let price = parse_money(&new.price);
    if price.is_none() {
        fields.insert("price", "invalid");
    }
    let pack = match &new.pack {
        None => None,
        Some((size, pack_price)) => match parse_money(pack_price) {
            Some(pack_price) if (2..=1_000_000).contains(size) => Some((*size, pack_price)),
            _ => {
                fields.insert("pack", "invalid");
                None
            }
        },
    };

    let allow_fraction: Option<bool> = sqlx::query_scalar("SELECT allow_fraction FROM units WHERE code = $1")
        .bind(&new.unit_code)
        .fetch_optional(&mut *conn)
        .await?;
    match allow_fraction {
        None => {
            fields.insert("unit_code", "not_found");
        }
        // Пачками продаётся только то, что считается штуками: «пачка из 2,5 кг» не бывает.
        Some(true) if new.pack.is_some() => {
            fields.insert("pack", "unit_not_countable");
        }
        Some(_) => {}
    }

    let category_deleted: Option<bool> = sqlx::query_scalar("SELECT is_deleted FROM categories WHERE id = $1")
        .bind(new.category_id)
        .fetch_optional(&mut *conn)
        .await?;
    match category_deleted {
        Some(false) => {}
        Some(true) => {
            fields.insert("category_id", "deleted");
        }
        None => {
            fields.insert("category_id", "not_found");
        }
    }
    if !fields.is_empty() {
        return Err(CatalogError::Validation(fields));
    }
    let price = price.expect("проверено выше");

    let inserted = sqlx::query(
        "INSERT INTO products
            (id, category_id, name, description, unit_code, price, pack_size, pack_price, is_published)
         VALUES ($1, $2, $3, $4, $5, $6::numeric, $7, $8::numeric, $9)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(new.id)
    .bind(new.category_id)
    .bind(&name)
    .bind(&description)
    .bind(&new.unit_code)
    .bind(&price)
    .bind(pack.as_ref().map(|(size, _)| *size))
    .bind(pack.as_ref().map(|(_, pack_price)| pack_price.as_str()))
    .bind(new.is_published)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(false);
    }

    audit::record(
        &mut *conn,
        Entry::new(actor, "product.create").entity("product", new.id).details(json!({
            "name": name,
            "category_id": new.category_id,
            "unit_code": new.unit_code,
            "price": price,
            "pack_size": pack.as_ref().map(|(size, _)| *size),
            "pack_price": pack.as_ref().map(|(_, pack_price)| pack_price.clone()),
        })),
    )
    .await?;
    Ok(true)
}

async fn exists(conn: &mut PgConnection, table: &'static str, id: Uuid) -> sqlx::Result<bool> {
    // Имя таблицы — константа из кода, не из запроса.
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)")))
        .bind(id)
        .fetch_one(conn)
        .await
}

/// Проверяет денежную сумму и приводит к виду «185.00».
/// Допускается до 10 цифр до точки и до 2 после; запятая принимается как точка.
pub fn parse_money(raw: &str) -> Option<String> {
    let normalized = raw.trim().replace(',', ".");
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let digits_only = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || whole.len() > 10 || fraction.len() > 2 || !digits_only(whole) || !digits_only(fraction) {
        return None;
    }
    if normalized.ends_with('.') {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    Some(format!("{whole}.{fraction:0<2}"))
}

/// Одна строка: без управляющих символов, пробелы по краям и повторы убраны.
fn clean_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Многострочный текст: переводы строк остаются, прочие управляющие символы убираются.
fn clean_text(text: &str) -> String {
    text.replace("\r\n", "\n").chars().filter(|c| *c == '\n' || !c.is_control()).collect::<String>().trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_is_normalized_without_floats() {
        for (raw, expected) in [
            ("96", "96.00"),
            ("96.5", "96.50"),
            ("0.45", "0.45"),
            ("0,45", "0.45"),
            (" 9800.00 ", "9800.00"),
            ("007", "7.00"),
            ("0", "0.00"),
            ("9999999999.99", "9999999999.99"),
        ] {
            assert_eq!(parse_money(raw).as_deref(), Some(expected), "{raw}");
        }
        for bad in ["", "-1", "1.234", "1e3", "abc", "1.", ".5", "1 000", "12345678901", "NaN", "+5"] {
            assert_eq!(parse_money(bad), None, "{bad}");
        }
    }

    #[test]
    fn text_cleaning() {
        assert_eq!(clean_line("  Дошка \t обрізна\n25×100\u{0} "), "Дошка обрізна 25×100");
        assert_eq!(clean_text(" перший\r\nдругий\u{0}\u{7} "), "перший\nдругий");
    }
}
