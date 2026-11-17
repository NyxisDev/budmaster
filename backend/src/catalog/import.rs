//! Импорт каталога из файла JSON — способ наполнить каталог, пока нет приложения.
//!
//! Формат:
//! ```json
//! {
//!   "categories": [{ "key": "lumber", "parent": null, "name": "Пиломатеріали" }],
//!   "products": [{
//!     "key": "board-25", "category": "lumber", "name": "Дошка обрізна 25×100×4000 мм",
//!     "description": "…", "unit": "pcs", "price": "96.00",
//!     "pack": { "size": 1000, "price": "380.00" }
//!   }]
//! }
//! ```
//! `key` — любое постоянное имя записи в файле. По нему вычисляется id, поэтому повторный
//! импорт того же файла ничего не дублирует: уже существующие записи пропускаются
//! (и не изменяются — правка товаров появится вместе с приложением).

use std::collections::HashMap;

use anyhow::{Context, bail};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::{CatalogError, NewCategory, NewProduct, create_category, create_product};
use crate::audit::Actor;

/// Пространство имён для id, вычисляемых из ключей файла (UUID v5).
const KEY_NAMESPACE: Uuid = Uuid::from_u128(0x6275_646d_6173_7465_725f_6361_7461_6c6f);

const MAX_ITEMS: usize = 20_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFile {
    #[serde(default)]
    categories: Vec<CategoryItem>,
    #[serde(default)]
    products: Vec<ProductItem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryItem {
    key: String,
    #[serde(default)]
    parent: Option<String>,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductItem {
    key: String,
    category: String,
    name: String,
    #[serde(default)]
    description: String,
    unit: String,
    price: String,
    #[serde(default)]
    pack: Option<PackItem>,
    #[serde(default = "yes")]
    published: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackItem {
    size: i32,
    price: String,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub categories_created: usize,
    pub categories_skipped: usize,
    pub products_created: usize,
    pub products_skipped: usize,
}

pub fn parse(json: &str) -> anyhow::Result<CatalogFile> {
    let file: CatalogFile = serde_json::from_str(json).context("файл каталога: неверный JSON или лишние поля")?;
    if file.categories.len() + file.products.len() > MAX_ITEMS {
        bail!("файл каталога слишком большой: больше {MAX_ITEMS} записей");
    }
    Ok(file)
}

/// Импортирует файл одной транзакцией: при ошибке в любой записи не сохраняется ничего.
pub async fn run(db: &PgPool, file: CatalogFile) -> anyhow::Result<ImportSummary> {
    let actor = Actor::server_cli();
    let mut summary = ImportSummary::default();
    let mut tx = db.begin().await?;

    let mut category_ids: HashMap<&str, Uuid> = HashMap::new();
    for (index, item) in file.categories.iter().enumerate() {
        if category_ids.insert(&item.key, key_id("category", &item.key)).is_some() {
            bail!("категория «{}»: ключ повторяется", item.key);
        }
        // Родитель должен стоять в файле раньше — так дерево не может замкнуться в кольцо.
        let parent_id = match &item.parent {
            None => None,
            Some(parent) if parent == &item.key => bail!("категория «{}» указана родителем самой себе", item.key),
            Some(parent) => Some(*category_ids.get(parent.as_str()).with_context(|| {
                format!("категория «{}»: родитель «{parent}» не найден выше по файлу", item.key)
            })?),
        };
        let new = NewCategory {
            id: category_ids[item.key.as_str()],
            parent_id,
            name: item.name.clone(),
            sort_order: i32::try_from(index + 1).unwrap_or(i32::MAX),
        };
        match create_category(&mut tx, &actor, new).await {
            Ok(true) => summary.categories_created += 1,
            Ok(false) => summary.categories_skipped += 1,
            Err(error) => return Err(describe(error, "категория", &item.key)),
        }
    }

    let mut seen_products = std::collections::HashSet::new();
    for item in &file.products {
        if !seen_products.insert(item.key.as_str()) {
            bail!("товар «{}»: ключ повторяется", item.key);
        }
        let category_id = *category_ids
            .get(item.category.as_str())
            .with_context(|| format!("товар «{}»: категория «{}» не найдена в файле", item.key, item.category))?;
        let new = NewProduct {
            id: key_id("product", &item.key),
            category_id,
            name: item.name.clone(),
            description: item.description.clone(),
            unit_code: item.unit.clone(),
            price: item.price.clone(),
            pack: item.pack.as_ref().map(|pack| (pack.size, pack.price.clone())),
            is_published: item.published,
        };
        match create_product(&mut tx, &actor, new).await {
            Ok(true) => summary.products_created += 1,
            Ok(false) => summary.products_skipped += 1,
            Err(error) => return Err(describe(error, "товар", &item.key)),
        }
    }

    tx.commit().await?;
    Ok(summary)
}

fn key_id(kind: &str, key: &str) -> Uuid {
    Uuid::new_v5(&KEY_NAMESPACE, format!("{kind}:{key}").as_bytes())
}

fn describe(error: CatalogError, kind: &str, key: &str) -> anyhow::Error {
    match error {
        CatalogError::Validation(fields) => {
            let list: Vec<String> = fields.iter().map(|(field, code)| format!("{field} — {code}")).collect();
            anyhow::anyhow!("{kind} «{key}»: ошибка в полях: {}", list.join(", "))
        }
        CatalogError::Db(error) => anyhow::Error::new(error).context(format!("{kind} «{key}»")),
    }
}
