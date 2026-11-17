//! Тесты заказов, кассы, возвратов и отчётов.

mod common;

use axum::http::StatusCode;
use budmaster_server::catalog::import;
use common::{Session, TestApp};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

const CATALOG: &str = r#"{
  "categories": [
    { "key": "fasteners", "name": "Кріплення" },
    { "key": "screws", "parent": "fasteners", "name": "Шурупи" },
    { "key": "cement", "name": "Цемент" }
  ],
  "products": [
    { "key": "screw", "category": "screws", "name": "Шуруп 3,5×35", "unit": "pcs", "price": "0.45", "pack": { "size": 100, "price": "40.00" } },
    { "key": "nails", "category": "fasteners", "name": "Цвяхи 80 мм", "unit": "kg", "price": "68.00" },
    { "key": "cement", "category": "cement", "name": "Цемент ПЦ-400", "unit": "bag", "price": "185.00" },
    { "key": "hidden", "category": "cement", "name": "Прихований", "unit": "bag", "price": "1.00", "published": false }
  ]
}"#;

struct F {
    app: TestApp,
    owner: Session,
    screw: Uuid,
    nails: Uuid,
    cement: Uuid,
    hidden: Uuid,
    w: Uuid,
    supplier: Uuid,
}

async fn setup(app: TestApp) -> F {
    import::run(&app.app_db, import::parse(CATALOG).unwrap()).await.unwrap();
    let owner = app.owner().await;
    let id = |name: &'static str| {
        let admin = app.admin.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM products WHERE name = $1").bind(name).fetch_one(&admin).await.unwrap()
        }
    };
    let (screw, nails, cement, hidden) =
        (id("Шуруп 3,5×35").await, id("Цвяхи 80 мм").await, id("Цемент ПЦ-400").await, id("Прихований").await);
    let supplier = Uuid::new_v4();
    let w = app.warehouse_id;
    let f = F { app, owner, screw, nails, cement, hidden, w, supplier };
    let r = push(&f, &f.owner, vec![op("supplier.create", json!({ "id": supplier, "name": "Постачальник" }))]).await;
    assert_eq!(statuses(&r), ["applied"]);
    f
}

async fn fixture(pool: PgPoolOptions, conn: PgConnectOptions) -> F {
    setup(TestApp::new(pool, conn).await).await
}

fn now() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

fn op(op_type: &str, payload: Value) -> Value {
    json!({ "op_id": Uuid::new_v4(), "type": op_type, "occurred_at": now(), "payload": payload })
}

async fn push(f: &F, session: &Session, ops: Vec<Value>) -> Value {
    let r = f.app.post("/api/v1/sync/push", &session.access, json!({ "device_time": now(), "ops": ops })).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body
}

async fn push1(f: &F, session: &Session, op_type: &str, payload: Value) -> (String, String) {
    let r = push(f, session, vec![op(op_type, payload)]).await;
    (r["results"][0]["status"].as_str().unwrap().to_owned(), r["results"][0]["code"].as_str().unwrap_or("").to_owned())
}

fn statuses(r: &Value) -> Vec<String> {
    r["results"].as_array().unwrap().iter().map(|x| x["status"].as_str().unwrap().to_owned()).collect()
}

async fn receive(f: &F, product: Uuid, unit: &str, qty: &str, cost: &str) {
    let r = push1(
        f,
        &f.owner,
        "supply.receive",
        json!({ "supply_id": Uuid::new_v4(), "supplier_id": f.supplier, "warehouse_id": f.w,
                "items": [{ "id": Uuid::new_v4(), "product_id": product, "stock_unit": unit, "received_qty": qty, "unit_cost": cost }] }),
    )
    .await;
    assert_eq!(r.0, "applied", "{r:?}");
}

/// (остаток штук, резерв штук) на складе.
async fn stock(f: &F, product: Uuid) -> (String, String) {
    sqlx::query_as::<_, (String, String)>(
        "SELECT qty_units::numeric(14,3)::text, reserved_units::numeric(14,3)::text FROM stock_balances
         WHERE product_id = $1 AND warehouse_id = $2",
    )
    .bind(product)
    .bind(f.w)
    .fetch_optional(&f.app.admin)
    .await
    .unwrap()
    .unwrap_or(("0.000".into(), "0.000".into()))
}

fn site_order(id: Uuid, items: Value) -> Value {
    json!({
        "id": id, "customer_name": "Петро Покупець", "customer_phone": "+38 (050) 123-45-67",
        "comment": "Передзвоніть після обіду", "delivery_type": "delivery", "delivery_address": "м. Чугуїв, вул. Миру, 5",
        "items": items, "website": "", "elapsed_ms": 15000
    })
}

async fn post_order(f: &F, body: Value) -> common::Response {
    f.app.request("POST", "/api/v1/public/orders", None, Some(body)).await
}

async fn availability(f: &F, product: Uuid) -> String {
    let card = f.app.request("GET", &format!("/api/v1/public/products/{product}"), None, None).await;
    card.body["availability"].as_str().unwrap().to_owned()
}

// --- Заявка с сайта ---------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn site_order_is_created_with_server_prices(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let id = Uuid::new_v4();
    let body = site_order(
        id,
        json!([
            { "product_id": f.cement, "stock_unit": "unit", "qty": 2 },
            { "product_id": f.screw, "stock_unit": "pack", "qty": "3" },
            { "product_id": f.nails, "stock_unit": "unit", "qty": 1.333 }
        ]),
    );
    let created = post_order(&f, body.clone()).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    // 2 × 185 + 3 × 40 + round(1.333 × 68, 2) = 370 + 120 + 90.64
    assert_eq!(created.body["total"], "580.64");
    let number = created.body["number"].as_i64().unwrap();
    assert!(number >= 1001);

    // Повтор той же заявки — тот же заказ, второго нет.
    let again = post_order(&f, body.clone()).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["number"], number);
    // Чужой телефон с тем же id ничего не узнаёт.
    let mut other = body.clone();
    other["customer_phone"] = json!("0671112233");
    assert_eq!(post_order(&f, other).await.body["error"]["fields"]["id"], "conflict");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orders").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(count, 1);

    // Телефон нормализован; телефона, имени и адреса нет в неизменяемом журнале.
    let phone: String = sqlx::query_scalar("SELECT customer_phone FROM orders").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(phone, "+380501234567");
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE details::text ~ '501234567|Петро|Миру'",
    )
    .fetch_one(&f.app.admin)
    .await
    .unwrap();
    assert_eq!(leaked, 0);

    // Заказ приходит на устройство.
    let pulled = f.app.get("/api/v1/sync/pull?cursor=0&limit=2000", &f.owner.access).await;
    let order = pulled.body["changes"].as_array().unwrap().iter().find(|c| c["type"] == "order").unwrap();
    assert_eq!(order["data"]["status"], "new");
    assert_eq!(order["data"]["items"].as_array().unwrap().len(), 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn site_order_is_validated(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let good = json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 1 }]);
    let cases: Vec<(Value, &str)> = vec![
        (json!({ "website": "http://spam" }), "website"),
        (json!({ "elapsed_ms": 500 }), "elapsed_ms"),
        (json!({ "elapsed_ms": null }), "elapsed_ms"),
        (json!({ "customer_phone": "12345" }), "customer_phone"),
        (json!({ "customer_name": "П" }), "customer_name"),
        (json!({ "delivery_address": "   " }), "delivery_address"),
        (json!({ "items": [] }), "items"),
        (json!({ "items": [{ "product_id": f.hidden, "stock_unit": "unit", "qty": 1 }] }), "items"),
        (json!({ "items": [{ "product_id": f.cement, "stock_unit": "unit", "qty": 1.5 }] }), "items"),
        (json!({ "items": [{ "product_id": f.cement, "stock_unit": "pack", "qty": 1 }] }), "items"),
        (json!({ "items": [{ "product_id": f.nails, "stock_unit": "unit", "qty": "1.2345" }] }), "items"),
        (json!({ "items": [
            { "product_id": f.cement, "stock_unit": "unit", "qty": 1 },
            { "product_id": f.cement, "stock_unit": "unit", "qty": 2 }] }), "items"),
    ];
    for (patch, field) in cases {
        let mut body = site_order(Uuid::new_v4(), good.clone());
        for (key, value) in patch.as_object().unwrap() {
            body[key] = value.clone();
        }
        let r = post_order(&f, body).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{patch}: {}", r.body);
        assert!(r.body["error"]["fields"].get(field).is_some(), "{patch}: {}", r.body);
    }
    // Цену с сайта сервер не принимает вовсе.
    let mut priced = site_order(Uuid::new_v4(), json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 1, "price": "0.01" }]));
    priced["elapsed_ms"] = json!(5000);
    assert_eq!(post_order(&f, priced).await.status, StatusCode::BAD_REQUEST);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orders").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn site_orders_are_rate_limited(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = setup(TestApp::with_order_limits(pool, conn, (3, 2)).await).await;
    let line = json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 1 }]);
    let with_phone = |phone: &str| {
        let mut body = site_order(Uuid::new_v4(), line.clone());
        body["customer_phone"] = json!(phone);
        body
    };
    assert_eq!(post_order(&f, with_phone("0501111111")).await.status, StatusCode::CREATED);
    assert_eq!(post_order(&f, with_phone("0501111111")).await.status, StatusCode::CREATED);
    let per_phone = post_order(&f, with_phone("0501111111")).await;
    assert_eq!(per_phone.status, StatusCode::TOO_MANY_REQUESTS, "не больше 2 на номер");
    assert!(per_phone.retry_after.is_some());
    assert_eq!(post_order(&f, with_phone("0502222222")).await.status, StatusCode::CREATED);
    assert_eq!(post_order(&f, with_phone("0503333333")).await.status, StatusCode::TOO_MANY_REQUESTS, "не больше 3 с адреса");
}

// --- Статусы, резерв, выдача -----------------------------------------------------

async fn new_site_order(f: &F, qty: i64) -> Uuid {
    let id = Uuid::new_v4();
    let r = post_order(f, site_order(id, json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": qty }]))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn order_lifecycle_reserves_and_sells(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, f.cement, "unit", "10", "150").await;
    let order = new_site_order(&f, 4).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let status = |s: &str, extra: Value| {
        let mut payload = json!({ "id": order, "status": s });
        for (k, v) in extra.as_object().unwrap() {
            payload[k] = v.clone();
        }
        payload
    };

    // Без склада подтвердить нельзя; без подтверждения выдать нельзя.
    assert_eq!(push1(&f, &worker, "order.set_status", status("confirmed", json!({}))).await.1, "warehouse_required");
    assert_eq!(push1(&f, &worker, "order.set_status", status("completed", json!({}))).await.1, "invalid_transition");

    assert_eq!(push1(&f, &worker, "order.set_status", status("confirmed", json!({ "warehouse_id": f.w }))).await.0, "applied");
    assert_eq!(stock(&f, f.cement).await, ("10.000".into(), "4.000".into()));
    // Повтор того же статуса с другого устройства — не ошибка.
    assert_eq!(push1(&f, &f.owner, "order.set_status", status("confirmed", json!({}))).await.0, "applied");
    assert_eq!(stock(&f, f.cement).await.1, "4.000", "резерв не удвоился");

    // Подсказка «есть X из Y»: свой резерв не вычитается.
    let detail = f.app.get(&format!("/api/v1/orders/{order}"), &worker.access).await;
    assert_eq!(detail.body["items"][0]["available"][0]["qty"], "10.000");
    assert_eq!(detail.body["history"].as_array().unwrap().len(), 2, "создан и подтверждён; повтор не пишется");

    assert_eq!(push1(&f, &worker, "order.set_status", status("assembled", json!({}))).await.0, "applied");
    assert_eq!(push1(&f, &worker, "order.set_status", status("completed", json!({ "sale_id": Uuid::new_v4() }))).await.1, "payment_type_required");

    let sale_id = Uuid::new_v4();
    let done = push1(&f, &worker, "order.set_status", status("completed", json!({ "payment_type": "cash", "sale_id": sale_id }))).await;
    assert_eq!(done.0, "applied", "{done:?}");
    assert_eq!(stock(&f, f.cement).await, ("6.000".into(), "0.000".into()), "списано, резерв снят");

    let (channel, total, order_id): (String, String, Uuid) =
        sqlx::query_as("SELECT channel::text, total::text, order_id FROM sales WHERE id = $1")
            .bind(sale_id)
            .fetch_one(&f.app.admin)
            .await
            .unwrap();
    assert_eq!((channel.as_str(), total.as_str(), order_id), ("site", "740.00", order));
    let cost: String = sqlx::query_scalar("SELECT unit_cost::text FROM sale_items WHERE sale_id = $1")
        .bind(sale_id)
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(cost, "150.0000");

    // Второе устройство тоже пытается выдать — «уже выдан»; отменить выданный нельзя.
    let second = push1(&f, &f.owner, "order.set_status", status("completed", json!({ "payment_type": "cash", "sale_id": Uuid::new_v4() }))).await;
    assert_eq!(second.1, "already_completed");
    assert_eq!(push1(&f, &f.owner, "order.set_status", status("cancelled", json!({}))).await.1, "already_completed");
    let sales: i64 = sqlx::query_scalar("SELECT count(*) FROM sales").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(sales, 1);

    // Кэш остатков и резервов совпадает с пересчётом.
    assert_eq!(budmaster_server::sync::stock::rebuild_balances(&f.app.app_db).await.unwrap(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn confirm_and_cancel_in_either_order_leave_no_reserve(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, f.cement, "unit", "5", "150").await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;

    // Подтверждение, потом отмена.
    let a = new_site_order(&f, 3).await;
    push1(&f, &worker, "order.set_status", json!({ "id": a, "status": "confirmed", "warehouse_id": f.w })).await;
    assert_eq!(availability(&f, f.cement).await, "in_stock");
    let r = push1(&f, &f.owner, "order.set_status", json!({ "id": a, "status": "cancelled", "cancel_reason": "Клієнт передумав" })).await;
    assert_eq!(r.0, "applied");
    assert_eq!(stock(&f, f.cement).await.1, "0.000");

    // Отмена пришла раньше подтверждения: подтверждение отклоняется.
    let b = new_site_order(&f, 3).await;
    push1(&f, &f.owner, "order.set_status", json!({ "id": b, "status": "cancelled" })).await;
    let late = push1(&f, &worker, "order.set_status", json!({ "id": b, "status": "confirmed", "warehouse_id": f.w })).await;
    assert_eq!(late.1, "order_cancelled");
    assert_eq!(stock(&f, f.cement).await.1, "0.000");

    // Весь остаток зарезервирован — на сайте товара «немає».
    let c = new_site_order(&f, 5).await;
    push1(&f, &worker, "order.set_status", json!({ "id": c, "status": "confirmed", "warehouse_id": f.w })).await;
    assert_eq!(availability(&f, f.cement).await, "out_of_stock");
    assert_eq!(budmaster_server::sync::stock::rebuild_balances(&f.app.app_db).await.unwrap(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn manual_orders_and_edits(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, f.cement, "unit", "10", "150").await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let order = Uuid::new_v4();
    let created = push1(
        &f,
        &worker,
        "order.create",
        json!({ "id": order, "customer_name": "Оксана", "customer_phone": "0671234567", "delivery_type": "pickup",
                "items": [{ "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "qty": "2" }] }),
    )
    .await;
    assert_eq!(created.0, "applied", "{created:?}");
    push1(&f, &worker, "order.set_status", json!({ "id": order, "status": "confirmed", "warehouse_id": f.w })).await;

    // Цена товара выросла — у строки в заказе цена прежняя.
    push1(&f, &f.owner, "product.update", json!({ "id": f.cement, "fields": { "price": "200.00" } })).await;
    let edit = |items: Value| json!({ "id": order, "items": items });
    let r = push1(
        &f,
        &worker,
        "order.update",
        edit(json!([
            { "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "qty": "3" },
            { "id": Uuid::new_v4(), "product_id": f.nails, "stock_unit": "unit", "qty": "0.5" }
        ])),
    )
    .await;
    assert_eq!(r.0, "applied", "{r:?}");
    let total: String = sqlx::query_scalar("SELECT total::text FROM orders WHERE id = $1").bind(order).fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(total, "589.00", "3 × 185 (прежняя цена) + 0,5 × 68 (текущая)");
    assert_eq!(stock(&f, f.cement).await.1, "3.000", "резерв пересчитан под новый состав");

    // Цену строки меняет только владелец.
    let priced = edit(json!([{ "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "qty": "3", "price": "150" }]));
    assert_eq!(push1(&f, &worker, "order.update", priced.clone()).await.1, "permission_required");
    assert_eq!(push1(&f, &f.owner, "order.update", priced).await.0, "applied");
    let total: String = sqlx::query_scalar("SELECT total::text FROM orders WHERE id = $1").bind(order).fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(total, "450.00");

    // Неверный телефон и доставка без адреса.
    assert_eq!(push1(&f, &worker, "order.update", json!({ "id": order, "customer_phone": "123" })).await.1, "invalid_phone");
    assert_eq!(push1(&f, &worker, "order.update", json!({ "id": order, "delivery_type": "delivery" })).await.1, "address_required");

    // Источник «вручную» — канал «вручную» (в отчёте это касса).
    push1(&f, &worker, "order.set_status", json!({ "id": order, "status": "assembled" })).await;
    push1(&f, &worker, "order.set_status", json!({ "id": order, "status": "completed", "payment_type": "terminal", "sale_id": Uuid::new_v4() })).await;
    let channel: String = sqlx::query_scalar("SELECT channel::text FROM sales").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(channel, "manual");
    assert_eq!(push1(&f, &f.owner, "order.update", json!({ "id": order, "comment": "пізно" })).await.1, "order_closed");
}

// --- Касса и возвраты --------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn pos_sales_and_returns(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, f.cement, "unit", "10", "150").await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let sale = Uuid::new_v4();
    let line = Uuid::new_v4();
    // Продали офлайн по старой цене 180 — продажа принимается как есть.
    let r = push1(
        &f,
        &worker,
        "sale.create",
        json!({ "id": sale, "warehouse_id": f.w, "payment_type": "cash",
                "items": [
                    { "id": line, "product_id": f.cement, "stock_unit": "unit", "qty": "3", "price": "180.00" },
                    { "id": Uuid::new_v4(), "product_id": f.nails, "stock_unit": "unit", "qty": "1.5", "price": "68" }
                ] }),
    )
    .await;
    assert_eq!(r.0, "applied", "{r:?}");
    assert_eq!(stock(&f, f.cement).await.0, "7.000");
    let differs: Value = sqlx::query_scalar("SELECT details->'price_differs' FROM audit_log WHERE action = 'sale.create'")
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(differs.as_array().unwrap().len(), 1);
    let (total, unknown): (String, i64) = sqlx::query_as(
        "SELECT s.total::text, (SELECT count(*) FROM sale_items WHERE sale_id = s.id AND unit_cost IS NULL) FROM sales s",
    )
    .fetch_one(&f.app.admin)
    .await
    .unwrap();
    assert_eq!((total.as_str(), unknown), ("642.00", 1), "540 + 102; себестоимость цвяхів неизвестна");

    // Возврат 2 из 3 — товар вернулся на склад, цена и себестоимость из продажи.
    let ret = |qty: &str| {
        json!({ "id": Uuid::new_v4(), "sale_id": sale, "warehouse_id": f.w, "reason": "Не підійшов", "payment_type": "cash",
                "items": [{ "id": Uuid::new_v4(), "sale_item_id": line, "product_id": f.cement, "stock_unit": "unit", "qty": qty }] })
    };
    assert_eq!(push1(&f, &worker, "return.create", ret("2")).await.0, "applied");
    assert_eq!(stock(&f, f.cement).await.0, "9.000");
    // Больше, чем осталось невозвращённым, — нельзя.
    assert_eq!(push1(&f, &worker, "return.create", ret("2")).await.1, "return_exceeds_sale");
    assert_eq!(push1(&f, &worker, "return.create", ret("1")).await.0, "applied");
    let (refund, channel): (String, String) =
        sqlx::query_as("SELECT sum(refund_total)::text, min(channel::text) FROM returns").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!((refund.as_str(), channel.as_str()), ("540.00", "pos"));

    // Без привязки к продаже — цену указывает работник.
    let unlinked = |price: Option<&str>| {
        let mut item = json!({ "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "qty": "1" });
        if let Some(price) = price {
            item["price"] = json!(price);
        }
        json!({ "id": Uuid::new_v4(), "warehouse_id": f.w, "reason": "Чек втрачено", "payment_type": "cash", "items": [item] })
    };
    assert_eq!(push1(&f, &worker, "return.create", unlinked(None)).await.1, "price_required");
    assert_eq!(push1(&f, &worker, "return.create", unlinked(Some("185"))).await.0, "applied");

    // Себестоимость продажи — только владельцу; на устройство она не уходит.
    let as_worker = f.app.get(&format!("/api/v1/sales/{sale}"), &worker.access).await;
    assert!(as_worker.body["items"][0].get("unit_cost").is_none());
    assert_eq!(as_worker.body["items"].as_array().unwrap().iter().find(|i| i["id"] == line.to_string()).unwrap()["returned_qty"], "3.000");
    let as_owner = f.app.get(&format!("/api/v1/sales/{sale}"), &f.owner.access).await;
    assert!(as_owner.body["items"].as_array().unwrap().iter().any(|i| i["unit_cost"] == "150.0000"));
    let pulled = f.app.get("/api/v1/sync/pull?cursor=0&limit=2000", &worker.access).await;
    let pulled_sales: Vec<&Value> = pulled.body["changes"].as_array().unwrap().iter().filter(|c| c["type"] == "sale").collect();
    assert_eq!(pulled_sales.len(), 1);
    // Себестоимость продаж на устройство не уходит (закупочные цены поставок — уходят, см. P6).
    assert!(!pulled_sales[0].to_string().contains("unit_cost"));
}

// --- Отчёты ------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reports_add_up(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, f.cement, "unit", "10", "150").await;
    receive(&f, f.screw, "pack", "5", "30").await; // 0,30 за штуку
    let worker = f.app.active_worker(&f.owner, "ivan").await;

    // Касса: 2 цементу × 185 і 1 пачка шурупів × 40.
    let sale = Uuid::new_v4();
    let cement_line = Uuid::new_v4();
    push1(&f, &worker, "sale.create", json!({ "id": sale, "warehouse_id": f.w, "payment_type": "cash", "items": [
        { "id": cement_line, "product_id": f.cement, "stock_unit": "unit", "qty": "2", "price": "185" },
        { "id": Uuid::new_v4(), "product_id": f.screw, "stock_unit": "pack", "qty": "1", "price": "40" }
    ] })).await;
    // Сайт: 1 цемент.
    let order = new_site_order(&f, 1).await;
    for (status, extra) in [
        ("confirmed", json!({ "warehouse_id": f.w })),
        ("assembled", json!({})),
        ("completed", json!({ "payment_type": "terminal", "sale_id": Uuid::new_v4() })),
    ] {
        let mut payload = json!({ "id": order, "status": status });
        for (k, v) in extra.as_object().unwrap() {
            payload[k] = v.clone();
        }
        assert_eq!(push1(&f, &worker, "order.set_status", payload).await.0, "applied");
    }
    // Повернули 1 цемент з каси; списали 1 цемент; продали цвяхи без відомої собівартості.
    push1(&f, &worker, "return.create", json!({ "id": Uuid::new_v4(), "sale_id": sale, "warehouse_id": f.w,
        "reason": "x", "payment_type": "cash",
        "items": [{ "id": Uuid::new_v4(), "sale_item_id": cement_line, "product_id": f.cement, "stock_unit": "unit", "qty": "1" }] })).await;
    push1(&f, &worker, "stock.write_off", json!({ "product_id": f.cement, "warehouse_id": f.w, "stock_unit": "unit", "qty": "1", "reason": "Бій" })).await;
    push1(&f, &worker, "sale.create", json!({ "id": Uuid::new_v4(), "warehouse_id": f.w, "payment_type": "cash", "items": [
        { "id": Uuid::new_v4(), "product_id": f.nails, "stock_unit": "unit", "qty": "1", "price": "68" }
    ] })).await;

    // «Сегодня» — по киевскому времени, как считает отчёт.
    let today: time::Date = sqlx::query_scalar("SELECT (now() AT TIME ZONE 'Europe/Kyiv')::date")
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    let today = today.to_string();
    let path = |group: &str| format!("/api/v1/reports/summary?from=2026-01-01&to={today}&group_by={group}");
    let by_channel = f.app.get(&path("channel"), &f.owner.access).await;
    assert_eq!(by_channel.status, StatusCode::OK, "{}", by_channel.body);
    let totals = &by_channel.body["totals"];
    // Оборот: 370 + 40 + 185 + 68 = 663; возвраты 185; выручка 478.
    // Себестоимость: 2×150 + 100×0,30 + 150 − 150 (повернули) = 330.
    // Цвяхи (68 ₴) без відомої собівартості в прибуток не входять зовсім: 478 − 68 − 330 = 80.
    assert_eq!(totals["turnover"], "663.00");
    assert_eq!(totals["returns"], "185.00");
    assert_eq!(totals["revenue"], "478.00");
    assert_eq!(totals["cost_of_sales"], "330.00");
    assert_eq!(totals["revenue_unknown_cost"], "68.00");
    assert_eq!(totals["profit"], "80.00");
    assert_eq!(totals["losses"], "150.00");
    assert_eq!(totals["unknown_cost_lines"], 1);

    let groups = by_channel.body["groups"].as_array().unwrap();
    let site = groups.iter().find(|g| g["key"] == "site").unwrap();
    let store = groups.iter().find(|g| g["key"] == "store").unwrap();
    assert_eq!((site["turnover"].as_str(), site["name"].as_str()), (Some("185.00"), Some("Сайт")));
    assert_eq!((store["turnover"].as_str(), store["returns"].as_str()), (Some("478.00"), Some("185.00")));

    // По категориям — с вложенными: шурупи входять у «Кріплення».
    let by_category = f.app.get(&path("category"), &f.owner.access).await;
    let fasteners = by_category.body["groups"].as_array().unwrap().iter().find(|g| g["name"] == "Кріплення").unwrap();
    assert_eq!(fasteners["turnover"], "108.00", "40 шурупи + 68 цвяхи");
    let by_warehouse = f.app.get(&path("warehouse"), &f.owner.access).await;
    assert_eq!(by_warehouse.body["groups"][0]["losses"], "150.00");

    let products_path = format!("/api/v1/reports/products?from=2026-01-01&to={today}");
    let products = f.app.get(&products_path, &f.owner.access).await;
    assert_eq!(products.body["items"][0]["product_name"], "Цемент ПЦ-400");
    assert_eq!(products.body["items"][0]["revenue"], "370.00");
    let nails = products.body["items"].as_array().unwrap().iter().find(|p| p["product_name"] == "Цвяхи 80 мм").unwrap();
    assert_eq!((nails["profit"].as_str(), nails["revenue_unknown_cost"].as_str()), (Some("0.00"), Some("68.00")));
    assert_eq!(f.app.get(&products_path, &worker.access).await.status, StatusCode::FORBIDDEN);

    // Пустой период — нули; работнику — 403; неверные даты — 422.
    let empty = f.app.get("/api/v1/reports/summary?from=2020-01-01&to=2020-01-31", &f.owner.access).await;
    assert_eq!(empty.body["totals"]["turnover"], "0.00");
    assert_eq!(f.app.get(&path("channel"), &worker.access).await.status, StatusCode::FORBIDDEN);
    for bad in ["from=2026-02-01&to=2026-01-01", "from=bad&to=2026-01-01", "from=2026-01-01&to=2026-01-31&group_by=x"] {
        let r = f.app.get(&format!("/api/v1/reports/summary?{bad}"), &f.owner.access).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
}

// --- Дополнительные случаи -----------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rejected_orders_do_not_use_up_the_limit(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = setup(TestApp::with_order_limits(pool, conn, (100, 1)).await).await;
    // Негодная заявка (скрытый товар, огромное количество) — отказ, попытка не тратится.
    let bad = site_order(Uuid::new_v4(), json!([{ "product_id": f.hidden, "stock_unit": "unit", "qty": 1 }]));
    assert_eq!(post_order(&f, bad).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let huge = site_order(Uuid::new_v4(), json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 99999999 }]));
    let r = post_order(&f, huge).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert_eq!(r.body["error"]["fields"]["items"], "invalid");
    // Единственная попытка на номер — всё ещё доступна.
    let good = site_order(Uuid::new_v4(), json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 99999 }]));
    assert_eq!(post_order(&f, good).await.status, StatusCode::CREATED);
    let next = site_order(Uuid::new_v4(), json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 1 }]));
    assert_eq!(post_order(&f, next).await.status, StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test(migrations = "./migrations")]
async fn warehouse_change_and_cancel_from_assembled_keep_reserves_consistent(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, f.cement, "unit", "10", "150").await;
    let w2 = Uuid::new_v4();
    push1(&f, &f.owner, "warehouse.create", json!({ "id": w2, "name": "Склад 2" })).await;
    let order = new_site_order(&f, 3).await;
    push1(&f, &f.owner, "order.set_status", json!({ "id": order, "status": "confirmed", "warehouse_id": f.w })).await;
    assert_eq!(stock(&f, f.cement).await.1, "3.000");

    // Заказ перенесли на другой склад — резерв переехал.
    assert_eq!(push1(&f, &f.owner, "order.update", json!({ "id": order, "warehouse_id": w2 })).await.0, "applied");
    assert_eq!(stock(&f, f.cement).await.1, "0.000");
    let reserved_w2: String = sqlx::query_scalar(
        "SELECT reserved_units::numeric(14,3)::text FROM stock_balances WHERE product_id = $1 AND warehouse_id = $2",
    )
    .bind(f.cement)
    .bind(w2)
    .fetch_one(&f.app.admin)
    .await
    .unwrap();
    assert_eq!(reserved_w2, "3.000");
    assert_eq!(budmaster_server::sync::stock::rebuild_balances(&f.app.app_db).await.unwrap(), 0);

    // Выключенный склад выбрать нельзя.
    push1(&f, &f.owner, "warehouse.deactivate", json!({ "id": w2 })).await;
    assert_eq!(push1(&f, &f.owner, "order.update", json!({ "id": order, "warehouse_id": w2 })).await.1, "warehouse_inactive");

    // Отмена из «собран» снимает резерв.
    push1(&f, &f.owner, "order.set_status", json!({ "id": order, "status": "assembled" })).await;
    assert_eq!(push1(&f, &f.owner, "order.set_status", json!({ "id": order, "status": "cancelled" })).await.0, "applied");
    assert_eq!(budmaster_server::sync::stock::rebuild_balances(&f.app.app_db).await.unwrap(), 0);
    let total_reserved: String = sqlx::query_scalar("SELECT sum(reserved_units)::numeric(14,3)::text FROM stock_balances")
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(total_reserved, "0.000");
}

#[sqlx::test(migrations = "./migrations")]
async fn order_and_sale_lists(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let a = new_site_order(&f, 1).await;
    let mut other = site_order(Uuid::new_v4(), json!([{ "product_id": f.cement, "stock_unit": "unit", "qty": 1 }]));
    other["customer_phone"] = json!("0679998877");
    post_order(&f, other).await;
    push1(&f, &f.owner, "order.set_status", json!({ "id": a, "status": "cancelled" })).await;
    push1(&f, &f.owner, "sale.create", json!({ "id": Uuid::new_v4(), "warehouse_id": f.w, "payment_type": "cash",
        "items": [{ "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "qty": "1", "price": "185" }] })).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;

    let all = f.app.get("/api/v1/orders", &worker.access).await;
    assert_eq!(all.body["total"], 2);
    let cancelled = f.app.get("/api/v1/orders?status=cancelled", &worker.access).await;
    assert_eq!(cancelled.body["items"][0]["id"], a.to_string());
    let by_phone = f.app.get("/api/v1/orders?q=998877", &worker.access).await;
    assert_eq!(by_phone.body["total"], 1);
    let number = all.body["items"][0]["number"].as_i64().unwrap();
    assert_eq!(f.app.get(&format!("/api/v1/orders?q={number}"), &worker.access).await.body["total"], 1);
    assert_eq!(f.app.get("/api/v1/orders?status=lost", &worker.access).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(f.app.get(&format!("/api/v1/orders/{}", Uuid::new_v4()), &worker.access).await.status, StatusCode::NOT_FOUND);

    let sales = f.app.get("/api/v1/sales", &worker.access).await;
    assert_eq!(sales.body["items"].as_array().unwrap().len(), 1);
    assert!(!sales.body.to_string().contains("unit_cost"));
}
