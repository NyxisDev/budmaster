//! Тесты каталога: импорт, публичное API, фото.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use budmaster_server::catalog::import;
use common::{TestApp, WORKER_PASSWORD};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

const CATALOG: &str = r#"{
  "categories": [
    { "key": "lumber", "name": "Пиломатеріали" },
    { "key": "boards", "parent": "lumber", "name": "Дошка" },
    { "key": "beams", "parent": "lumber", "name": "Брус" },
    { "key": "fasteners", "name": "Кріплення" },
    { "key": "screws", "parent": "fasteners", "name": "Шурупи" },
    { "key": "paint", "name": "Лакофарбові матеріали" }
  ],
  "products": [
    { "key": "board-25", "category": "boards", "name": "Дошка обрізна 25×100×4000 мм", "description": "Соснова дошка.", "unit": "pcs", "price": "96" },
    { "key": "board-m3", "category": "boards", "name": "Дошка обрізна, сосна", "unit": "m3", "price": "9800.00" },
    { "key": "beam-100", "category": "beams", "name": "Брус 100×100×4000 мм", "unit": "pcs", "price": "620.5" },
    { "key": "screw-35", "category": "screws", "name": "Шуруп по дереву 3,5×35 мм", "description": "Знижка 100% не діє, артикул A_1.", "unit": "pcs", "price": "0.45", "pack": { "size": 1000, "price": "380.00" } },
    { "key": "paint-white", "category": "paint", "name": "Фарба інтер’єрна матова біла, 10 л", "unit": "pcs", "price": "890.00" },
    { "key": "paint-hidden", "category": "paint", "name": "Фарба прихована", "unit": "pcs", "price": "1.00", "published": false }
  ]
}"#;

async fn import_catalog(app: &TestApp) -> import::ImportSummary {
    import::run(&app.app_db, import::parse(CATALOG).unwrap()).await.unwrap()
}

async fn public(app: &TestApp, path: &str) -> common::Response {
    app.request("GET", &format!("/api/v1/public{path}"), None, None).await
}

fn names(response: &common::Response) -> Vec<&str> {
    response.body["items"].as_array().unwrap().iter().map(|item| item["name"].as_str().unwrap()).collect()
}

async fn id_of(app: &TestApp, table: &str, name: &str) -> Uuid {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT id FROM {table} WHERE name = $1")))
        .bind(name)
        .fetch_one(&app.admin)
        .await
        .unwrap()
}

// --- Публичное API ----------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn public_catalog_matches_the_documented_format(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let summary = import_catalog(&app).await;
    assert_eq!((summary.categories_created, summary.products_created), (6, 6));

    let categories = public(&app, "/categories").await;
    assert_eq!(categories.status, StatusCode::OK);
    let items = categories.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    let lumber = items.iter().find(|c| c["name"] == "Пиломатеріали").unwrap();
    assert_eq!(lumber["slug"], "pylomaterialy");
    assert!(lumber["parent_id"].is_null());
    let boards = items.iter().find(|c| c["name"] == "Дошка").unwrap();
    assert_eq!(boards["parent_id"], lumber["id"]);
    assert_eq!(keys(lumber), ["id", "name", "parent_id", "slug", "sort_order"]);

    let list = public(&app, "/products").await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.body["total"], 5, "скрытый товар не считается");
    assert_eq!((list.body["page"].as_i64(), list.body["per_page"].as_i64()), (Some(1), Some(12)));
    assert_eq!(keys(&list.body), ["items", "page", "per_page", "total"]);

    let items = list.body["items"].as_array().unwrap();
    let screw = items.iter().find(|p| p["name"] == "Шуруп по дереву 3,5×35 мм").unwrap();
    // Ровно те поля, что описаны в docs/design.md 12.1: ни себестоимости, ни остатков, ни служебных.
    assert_eq!(
        keys(screw),
        ["availability", "category_id", "description", "id", "images", "name", "pack", "price", "unit"]
    );
    assert_eq!(screw["price"], "0.45", "цена — строка с двумя знаками");
    assert_eq!(screw["pack"], json!({ "size": 1000, "price": "380.00" }));
    assert_eq!(screw["unit"], json!({ "code": "pcs", "short_name": "шт", "allow_fraction": false }));
    assert_eq!(screw["images"], json!([]));
    assert_eq!(screw["availability"], "out_of_stock");

    let board = items.iter().find(|p| p["name"] == "Дошка обрізна 25×100×4000 мм").unwrap();
    assert_eq!(board["price"], "96.00");
    assert!(board["pack"].is_null());
    let beam = items.iter().find(|p| p["name"] == "Брус 100×100×4000 мм").unwrap();
    assert_eq!(beam["price"], "620.50");
    let cubic = items.iter().find(|p| p["name"] == "Дошка обрізна, сосна").unwrap();
    assert_eq!(cubic["unit"]["allow_fraction"], true);

    let one = public(&app, &format!("/products/{}", screw["id"].as_str().unwrap())).await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(&one.body, screw, "карточка и строка списка совпадают");
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

#[sqlx::test(migrations = "./migrations")]
async fn hidden_and_deleted_items_never_reach_the_site(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;

    let hidden = id_of(&app, "products", "Фарба прихована").await;
    let deleted = id_of(&app, "products", "Фарба інтер’єрна матова біла, 10 л").await;
    sqlx::query("UPDATE products SET is_deleted = true WHERE id = $1").bind(deleted).execute(&app.admin).await.unwrap();
    // Удалена категория верхнего уровня: пропадают она, её подкатегории и все их товары.
    sqlx::query("UPDATE categories SET is_deleted = true WHERE name = 'Пиломатеріали'")
        .execute(&app.admin)
        .await
        .unwrap();
    let in_deleted_branch = id_of(&app, "products", "Брус 100×100×4000 мм").await;

    let list = public(&app, "/products").await;
    assert_eq!(names(&list), ["Шуруп по дереву 3,5×35 мм"]);
    assert_eq!(list.body["total"], 1);
    for id in [hidden, deleted, in_deleted_branch] {
        assert_eq!(public(&app, &format!("/products/{id}")).await.status, StatusCode::NOT_FOUND);
    }

    let categories = public(&app, "/categories").await;
    let mut visible: Vec<&str> =
        categories.body["items"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    visible.sort_unstable();
    assert_eq!(visible, ["Кріплення", "Лакофарбові матеріали", "Шурупи"]);

    // Фильтр по удалённой категории не открывает её товары.
    let lumber = id_of(&app, "categories", "Пиломатеріали").await;
    assert_eq!(public(&app, &format!("/products?category_id={lumber}")).await.body["total"], 0);

    // Поиск тоже не находит скрытое.
    assert_eq!(public(&app, "/products?q=%D0%BF%D1%80%D0%B8%D1%85%D0%BE%D0%B2%D0%B0%D0%BD%D0%B0").await.body["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn category_filter_search_and_paging(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;

    // Категория включает подкатегории.
    let lumber = id_of(&app, "categories", "Пиломатеріали").await;
    let in_lumber = public(&app, &format!("/products?category_id={lumber}")).await;
    assert_eq!(
        names(&in_lumber),
        ["Брус 100×100×4000 мм", "Дошка обрізна 25×100×4000 мм", "Дошка обрізна, сосна"],
        "по алфавиту"
    );
    let boards = id_of(&app, "categories", "Дошка").await;
    assert_eq!(public(&app, &format!("/products?category_id={boards}")).await.body["total"], 2);
    assert_eq!(public(&app, &format!("/products?category_id={}", Uuid::new_v4())).await.body["total"], 0);

    // Поиск без учёта регистра, по названию и описанию, вместе с категорией.
    let search = |q: &str| format!("/products?q={}", urlencode(q));
    assert_eq!(public(&app, &search("ДОШКА")).await.body["total"], 2);
    assert_eq!(public(&app, &search("соснова")).await.body["total"], 1, "по описанию");
    assert_eq!(public(&app, &format!("{}&category_id={boards}", search("брус"))).await.body["total"], 0);
    assert_eq!(public(&app, &search("   ")).await.body["total"], 5, "пустой запрос — весь каталог");

    // Знаки % и _ ищутся буквально, а не как «что угодно».
    assert_eq!(public(&app, &search("%")).await.body["total"], 1);
    assert_eq!(public(&app, &search("_")).await.body["total"], 1);
    assert_eq!(public(&app, &search("100%")).await.body["total"], 1);
    assert_eq!(public(&app, &search("a_1")).await.body["total"], 1);
    assert_eq!(public(&app, &search("ax1")).await.body["total"], 0);
    assert_eq!(public(&app, &search("'; DROP TABLE products; --")).await.body["total"], 0);
    assert_eq!(public(&app, "/products").await.body["total"], 5, "таблица на месте");

    // Страницы не пересекаются и вместе дают весь список.
    let first = public(&app, "/products?per_page=2&page=1").await;
    let second = public(&app, "/products?per_page=2&page=2").await;
    let third = public(&app, "/products?per_page=2&page=3").await;
    let mut all = names(&first);
    all.extend(names(&second));
    all.extend(names(&third));
    assert_eq!(all.len(), 5);
    let mut unique = all.clone();
    unique.dedup();
    assert_eq!(unique.len(), 5);
    assert_eq!(names(&public(&app, "/products?page=99").await).len(), 0);

    for bad in ["per_page=49", "per_page=0", "page=0", "page=-1", &format!("q={}", "a".repeat(101))] {
        assert_eq!(public(&app, &format!("/products?{bad}")).await.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    assert_eq!(public(&app, "/products?category_id=abc").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(public(&app, "/products/not-a-uuid").await.status, StatusCode::NOT_FOUND);
}

fn urlencode(text: &str) -> String {
    text.bytes().map(|byte| format!("%{byte:02X}")).collect()
}

async fn media_cache(app: &TestApp, url: &str) -> Option<String> {
    use tower::ServiceExt;
    let path = url.strip_prefix("http://api.test").unwrap_or(url);
    let response = app.router().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
    response.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

/// PNG, заявляющий заданные размеры, с правильной контрольной суммой заголовка.
fn png_claiming(width: u32, height: u32) -> Vec<u8> {
    let mut file = png(1, 1, false);
    file[16..20].copy_from_slice(&width.to_be_bytes());
    file[20..24].copy_from_slice(&height.to_be_bytes());
    let mut crc = !0u32;
    for byte in &file[12..29] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    file[29..33].copy_from_slice(&(!crc).to_be_bytes());
    file
}

// --- Импорт -----------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn import_is_repeatable_and_all_or_nothing(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;

    // Повторный импорт того же файла ничего не дублирует.
    let again = import_catalog(&app).await;
    assert_eq!((again.categories_created, again.products_created), (0, 0));
    assert_eq!((again.categories_skipped, again.products_skipped), (6, 6));
    assert_eq!(app.audit_count("product.create").await, 6);
    assert_eq!(app.audit_count("category.create").await, 6);

    let broken_files = [
        // неизвестная категория у товара
        r#"{"categories":[{"key":"a","name":"Нова"}],"products":[{"key":"p","category":"zzz","name":"X","unit":"pcs","price":"1"}]}"#,
        // цена не число
        r#"{"categories":[{"key":"a","name":"Нова"}],"products":[{"key":"p","category":"a","name":"X","unit":"pcs","price":"дорого"}]}"#,
        // отрицательная цена
        r#"{"categories":[{"key":"a","name":"Нова"}],"products":[{"key":"p","category":"a","name":"X","unit":"pcs","price":"-5"}]}"#,
        // пачка у товара, который продаётся на вес
        r#"{"categories":[{"key":"a","name":"Нова"}],"products":[{"key":"p","category":"a","name":"X","unit":"kg","price":"1","pack":{"size":10,"price":"9"}}]}"#,
        // неизвестная единица
        r#"{"categories":[{"key":"a","name":"Нова"}],"products":[{"key":"p","category":"a","name":"X","unit":"парсек","price":"1"}]}"#,
        // родитель стоит ниже по файлу
        r#"{"categories":[{"key":"a","parent":"b","name":"Нова"},{"key":"b","name":"Б"}]}"#,
        // повтор ключа
        r#"{"categories":[{"key":"a","name":"Нова"},{"key":"a","name":"Ще"}]}"#,
        // пустое название
        r#"{"categories":[{"key":"a","name":"   "}]}"#,
    ];
    for json in broken_files {
        let result = import::run(&app.app_db, import::parse(json).unwrap()).await;
        assert!(result.is_err(), "{json}");
    }
    // Лишнее поле и битый JSON отклоняются ещё при чтении файла.
    assert!(import::parse(r#"{"categories":[{"key":"a","name":"A","cost":5}]}"#).is_err());
    assert!(import::parse("{ не json").is_err());

    // Ни один из сломанных файлов не оставил следов — даже первая, годная категория из них.
    let categories: i64 = sqlx::query_scalar("SELECT count(*) FROM categories").fetch_one(&app.admin).await.unwrap();
    let products: i64 = sqlx::query_scalar("SELECT count(*) FROM products").fetch_one(&app.admin).await.unwrap();
    assert_eq!((categories, products), (6, 6));
}

#[sqlx::test(migrations = "./migrations")]
async fn repeated_import_survives_deleted_records(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;
    sqlx::query("UPDATE categories SET is_deleted = true WHERE name = 'Пиломатеріали'")
        .execute(&app.admin)
        .await
        .unwrap();
    sqlx::query("UPDATE products SET is_deleted = true WHERE name = 'Брус 100×100×4000 мм'")
        .execute(&app.admin)
        .await
        .unwrap();

    // Удалённое не возвращается и не мешает: повторный импорт просто пропускает всё, что уже было.
    let again = import_catalog(&app).await;
    assert_eq!((again.categories_created, again.products_created), (0, 0));
    let deleted: i64 = sqlx::query_scalar("SELECT count(*) FROM categories WHERE is_deleted")
        .fetch_one(&app.admin)
        .await
        .unwrap();
    assert_eq!(deleted, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn categories_with_the_same_name_get_different_addresses(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let json = r#"{"categories":[
        {"key":"a","name":"Цегла"},
        {"key":"b","name":"Інше"},
        {"key":"c","parent":"b","name":"Цегла"},
        {"key":"d","parent":"b","name":"ЦЕГЛА!"}
    ]}"#;
    import::run(&app.app_db, import::parse(json).unwrap()).await.unwrap();
    let mut slugs: Vec<String> = sqlx::query_scalar("SELECT slug FROM categories WHERE name ILIKE 'цегла%'")
        .fetch_all(&app.admin)
        .await
        .unwrap();
    slugs.sort();
    assert_eq!(slugs, ["tsehla", "tsehla-2", "tsehla-3"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn server_role_cannot_delete_catalog_rows(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;
    for table in ["products", "categories", "product_images", "units"] {
        let error = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&app.app_db)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{table}: {error}");
    }
    let error = sqlx::query("UPDATE units SET allow_fraction = true").execute(&app.app_db).await.unwrap_err();
    assert!(error.to_string().contains("permission denied"), "справочник единиц только для чтения: {error}");

    // База сама не принимает бессмысленные товары.
    let category = id_of(&app, "categories", "Дошка").await;
    for (price, pack_size, pack_price) in [("-1", None, None), ("1", Some(10), None), ("1", Some(1), Some("5"))] {
        let result = sqlx::query(
            "INSERT INTO products (id, category_id, name, unit_code, price, pack_size, pack_price)
             VALUES ($1, $2, 'x', 'pcs', $3::numeric, $4, $5::numeric)",
        )
        .bind(Uuid::new_v4())
        .bind(category)
        .bind(price)
        .bind(pack_size)
        .bind(pack_price)
        .execute(&app.admin)
        .await;
        assert!(result.is_err(), "{price} {pack_size:?} {pack_price:?}");
    }
}

// --- Фото -------------------------------------------------------------------

/// PNG заданного размера. `noisy` — с шумом, чтобы файл плохо сжимался и был большим.
fn png(width: u32, height: u32, noisy: bool) -> Vec<u8> {
    use image::ImageEncoder;
    let mut state = 12345u32;
    let mut pixels = Vec::with_capacity((width * height * 3) as usize);
    for _ in 0..width * height * 3 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        pixels.push(if noisy { (state >> 24) as u8 } else { 180 });
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, width, height, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

async fn upload(app: &TestApp, token: Option<&str>, product_id: Uuid, bytes: Vec<u8>) -> common::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/products/{product_id}/images"))
        .header(header::CONTENT_TYPE, "image/png");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    app.raw(builder.body(Body::from(bytes)).unwrap()).await
}

async fn fetch_media(app: &TestApp, url: &str) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let path = url.strip_prefix("http://api.test").unwrap_or(url);
    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
    let response = app.router().oneshot(request).await.unwrap();
    let header_text = |name| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let (content_type, nosniff) = (header_text(header::CONTENT_TYPE), header_text(header::X_CONTENT_TYPE_OPTIONS));
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, content_type, nosniff, body)
}

#[sqlx::test(migrations = "./migrations")]
async fn photo_upload_end_to_end(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;
    let owner = app.owner().await;
    let product = id_of(&app, "products", "Брус 100×100×4000 мм").await;

    // Файл больше общего лимита запросов (64 КБ), но меньше лимита фото — принимается.
    let big = png(700, 500, true);
    assert!(big.len() > 64 * 1024 && big.len() < 10 * 1024 * 1024, "{} байт", big.len());
    let uploaded = upload(&app, Some(&owner.access), product, big.clone()).await;
    assert_eq!(uploaded.status, StatusCode::CREATED, "{}", uploaded.body);
    assert_eq!((uploaded.body["width"].as_i64(), uploaded.body["height"].as_i64()), (Some(700), Some(500)));
    let url = uploaded.body["url"].as_str().unwrap().to_owned();
    let thumb_url = uploaded.body["thumb_url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("http://api.test/media/products/") && url.ends_with(".jpg"), "{url}");

    // Сайт видит фото в карточке товара с полным адресом.
    let card = public(&app, &format!("/products/{product}")).await;
    assert_eq!(card.body["images"], json!([{ "url": url, "thumb_url": thumb_url }]));

    // Файл отдаётся как JPEG — не тем, что было загружено.
    let (status, content_type, nosniff, body) = fetch_media(&app, &url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media_cache(&app, &url).await.as_deref(), Some("public, max-age=86400"));
    assert_eq!(content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(nosniff.as_deref(), Some("nosniff"));
    assert_eq!(&body[..3], &[0xFF, 0xD8, 0xFF]);
    assert_ne!(body, big);
    let (thumb_status, ..) = fetch_media(&app, &thumb_url).await;
    assert_eq!(thumb_status, StatusCode::OK);

    // Из папки с фото нельзя выйти наружу.
    for path in ["/media/products/../../etc/passwd", "/media/products/..%2F..%2Fetc%2Fpasswd", "/media/", "/media/products/"] {
        let (status, ..) = fetch_media(&app, path).await;
        assert!(status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST, "{path}: {status}");
    }

    // Порядок и удаление.
    let second = upload(&app, Some(&owner.access), product, png(40, 40, false)).await;
    assert_eq!(second.body["sort_order"], 1);
    let first_id = uploaded.body["id"].as_str().unwrap();
    let reorder = app
        .request(
            "PATCH",
            &format!("/api/v1/products/{product}/images/{first_id}"),
            Some(&owner.access),
            Some(json!({ "sort_order": 5 })),
        )
        .await;
    assert_eq!(reorder.status, StatusCode::OK);
    let card = public(&app, &format!("/products/{product}")).await;
    assert_eq!(card.body["images"][0]["url"], second.body["url"], "первое фото уехало в конец");

    let removed = app
        .request("DELETE", &format!("/api/v1/products/{product}/images/{first_id}"), Some(&owner.access), None)
        .await;
    assert_eq!(removed.status, StatusCode::OK);
    assert_eq!(public(&app, &format!("/products/{product}")).await.body["images"].as_array().unwrap().len(), 1);
    assert_eq!(fetch_media(&app, &url).await.0, StatusCode::NOT_FOUND, "файл стёрт с диска");
    // Отказ /media — в общем формате ошибок и не кэшируется.
    let (_, missing_type, _, missing_body) = fetch_media(&app, &url).await;
    assert_eq!(missing_type.as_deref(), Some("application/json"));
    assert_eq!(serde_json::from_slice::<Value>(&missing_body).unwrap()["error"]["code"], "not_found");
    assert_eq!(media_cache(&app, &url).await.as_deref(), Some("no-store"));
    let again = app
        .request("DELETE", &format!("/api/v1/products/{product}/images/{first_id}"), Some(&owner.access), None)
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    // Запись о фото осталась в базе помеченной, а не исчезла.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM product_images WHERE is_deleted")
        .fetch_one(&app.admin)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(app.audit_count("product.image_add").await, 2);
    assert_eq!(app.audit_count("product.image_remove").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn photo_upload_is_validated_and_limited(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;
    let owner = app.owner().await;
    let product = id_of(&app, "products", "Брус 100×100×4000 мм").await;
    let token = Some(owner.access.as_str());

    let bad = [
        (b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>".to_vec(), "unsupported_format"),
        (b"<html><script>alert(1)</script></html>".to_vec(), "unsupported_format"),
        (b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(), "unsupported_format"),
        (png(20, 20, false)[..40].to_vec(), "corrupted"),
        (Vec::new(), "empty"),
    ];
    for (bytes, code) in bad {
        let response = upload(&app, token, product, bytes).await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{code}: {}", response.body);
        assert_eq!(response.body["error"]["fields"]["image"], code);
    }

    let too_big = upload(&app, token, product, vec![0u8; 10 * 1024 * 1024 + 1]).await;
    assert_eq!(too_big.status, StatusCode::PAYLOAD_TOO_LARGE);

    assert_eq!(upload(&app, token, Uuid::new_v4(), png(20, 20, false)).await.status, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE products SET is_deleted = true WHERE name = 'Фарба прихована'").execute(&app.admin).await.unwrap();
    let deleted = id_of(&app, "products", "Фарба прихована").await;
    assert_eq!(upload(&app, token, deleted, png(20, 20, false)).await.status, StatusCode::NOT_FOUND);

    // Не больше пяти фото на товар.
    for n in 0..5 {
        assert_eq!(upload(&app, token, product, png(30, 30, false)).await.status, StatusCode::CREATED, "фото {n}");
    }
    let sixth = upload(&app, token, product, png(30, 30, false)).await;
    assert_eq!(sixth.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(sixth.body["error"]["fields"]["image"], "limit_reached");

    // Файл, заявляющий огромные размеры, отклоняется до раскодирования.
    let other = id_of(&app, "products", "Дошка обрізна, сосна").await;
    let huge = upload(&app, token, other, png_claiming(60_000, 60_000)).await;
    assert_eq!(huge.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(huge.body["error"]["fields"]["image"], "dimensions_too_large");

    // Ни один отклонённый файл не остался на диске: там ровно 5 фото и 5 миниатюр.
    let files = std::fs::read_dir(app.media_dir.join("products")).unwrap().count();
    assert_eq!(files, 10);
}

#[sqlx::test(migrations = "./migrations")]
async fn photo_endpoints_require_the_products_permission(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;
    let owner = app.owner().await;
    let product = id_of(&app, "products", "Брус 100×100×4000 мм").await;
    let existing = upload(&app, Some(&owner.access), product, png(30, 30, false)).await;
    let image_path = format!("/api/v1/products/{product}/images/{}", existing.body["id"].as_str().unwrap());

    // Без входа.
    assert_eq!(upload(&app, None, product, png(30, 30, false)).await.status, StatusCode::UNAUTHORIZED);

    // Работник без права «товары» (даже со всеми остальными правами).
    let plain = change_password(&app, &owner, "plain", 30).await;
    assert_eq!(upload(&app, Some(&plain), product, png(30, 30, false)).await.code(), "permission_required");
    assert_eq!(app.request("DELETE", &image_path, Some(&plain), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.request("PATCH", &image_path, Some(&plain), Some(json!({ "sort_order": 3 }))).await.status,
        StatusCode::FORBIDDEN
    );

    // Работник с правом «товары».
    let editor = change_password(&app, &owner, "editor", 1).await;
    assert_eq!(upload(&app, Some(&editor), product, png(30, 30, false)).await.status, StatusCode::CREATED);
    assert_eq!(app.request("DELETE", &image_path, Some(&editor), None).await.status, StatusCode::OK);

    // Работник с временным паролем не может ничего, даже с правом.
    app.create_worker(&owner, "newbie", 1).await;
    let newbie = app.login("newbie", WORKER_PASSWORD).await;
    assert_eq!(upload(&app, Some(&newbie.access), product, png(30, 30, false)).await.code(), "password_change_required");
}

#[sqlx::test(migrations = "./migrations")]
async fn simultaneous_uploads_cannot_exceed_the_photo_limit(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import_catalog(&app).await;
    let owner = app.owner().await;
    let product = id_of(&app, "products", "Брус 100×100×4000 мм").await;

    let responses = join_all((0..8).map(|_| upload(&app, Some(&owner.access), product, png(30, 30, false)))).await;
    let created = responses.iter().filter(|r| r.status == StatusCode::CREATED).count();
    let refused = responses
        .iter()
        .filter(|r| r.status == StatusCode::UNPROCESSABLE_ENTITY && r.body["error"]["fields"]["image"] == "limit_reached")
        .count();
    assert_eq!((created, refused), (5, 3));

    let in_db: i64 = sqlx::query_scalar("SELECT count(*) FROM product_images WHERE product_id = $1")
        .bind(product)
        .fetch_one(&app.admin)
        .await
        .unwrap();
    assert_eq!(in_db, 5);
    // Файлы отказанных загрузок, записанные до проверки под блокировкой, удалены.
    let names: Vec<String> = std::fs::read_dir(app.media_dir.join("products"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 10, "{names:?}");
    assert!(names.iter().all(|name| !name.ends_with(".tmp")), "{names:?}");
}

/// Запускает все запросы сразу и ждёт, пока завершатся все.
async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut pending: Vec<_> = futures.into_iter().map(|future| Some(Box::pin(future))).collect();
    let mut outputs: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (slot, output) in pending.iter_mut().zip(outputs.iter_mut()) {
            if let Some(future) = slot {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => {
                        *output = Some(value);
                        *slot = None;
                    }
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
    outputs.into_iter().map(|output| output.expect("все завершились")).collect()
}

/// Создаёт работника с маской прав, меняет временный пароль и возвращает рабочий токен.
async fn change_password(app: &TestApp, owner: &common::Session, login: &str, permissions: i64) -> String {
    app.create_worker(owner, login, permissions).await;
    let first = app.login(login, WORKER_PASSWORD).await;
    let changed = app
        .post(
            "/api/v1/auth/change-password",
            &first.access,
            json!({ "current_password": WORKER_PASSWORD, "new_password": "worker-password-2" }),
        )
        .await;
    changed.body["access_token"].as_str().unwrap().to_owned()
}
