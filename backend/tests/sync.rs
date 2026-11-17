//! Тесты синхронизации и склада.

mod common;

use axum::http::StatusCode;
use budmaster_server::catalog::import;
use common::{Session, TestApp, WORKER_PASSWORD};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const CATALOG: &str = r#"{
  "categories": [
    { "key": "fasteners", "name": "Кріплення" },
    { "key": "cement", "name": "Цемент" }
  ],
  "products": [
    { "key": "screw", "category": "fasteners", "name": "Шуруп 3,5×35", "unit": "pcs", "price": "0.45", "pack": { "size": 100, "price": "40.00" } },
    { "key": "nails", "category": "fasteners", "name": "Цвяхи 80 мм", "unit": "kg", "price": "68.00" },
    { "key": "cement", "category": "cement", "name": "Цемент ПЦ-400", "unit": "bag", "price": "185.00" }
  ]
}"#;

struct Fixture {
    app: TestApp,
    owner: Session,
    screw: Uuid,
    nails: Uuid,
    cement: Uuid,
    warehouse: Uuid,
    second_warehouse: Uuid,
    supplier: Uuid,
}

async fn fixture(pool: PgPoolOptions, conn: PgConnectOptions) -> Fixture {
    let app = TestApp::new(pool, conn).await;
    import::run(&app.app_db, import::parse(CATALOG).unwrap()).await.unwrap();
    let owner = app.owner().await;
    let product = |name: &'static str| {
        let admin = app.admin.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM products WHERE name = $1")
                .bind(name)
                .fetch_one(&admin)
                .await
                .unwrap()
        }
    };
    let (screw, nails, cement) = (product("Шуруп 3,5×35").await, product("Цвяхи 80 мм").await, product("Цемент ПЦ-400").await);

    let second_warehouse = Uuid::new_v4();
    let supplier = Uuid::new_v4();
    let results = push(
        &app,
        &owner,
        vec![
            op("warehouse.create", json!({ "id": second_warehouse, "name": "Склад 2", "address": "вул. Нова, 1" })),
            op("supplier.create", json!({ "id": supplier, "name": "ТОВ Будпостач", "phone": "+380501112233" })),
        ],
    )
    .await;
    assert_statuses(&results, &["applied", "applied"]);
    let warehouse = app.warehouse_id;
    Fixture { app, owner, screw, nails, cement, warehouse, second_warehouse, supplier }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn ts(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap()
}

/// Операция «сейчас».
fn op(op_type: &str, payload: Value) -> Value {
    op_at(op_type, payload, now())
}

fn op_at(op_type: &str, payload: Value, at: OffsetDateTime) -> Value {
    json!({ "op_id": Uuid::new_v4(), "type": op_type, "occurred_at": ts(at), "payload": payload })
}

async fn push(app: &TestApp, session: &Session, ops: Vec<Value>) -> Value {
    push_raw(app, session, ops, now(), true).await
}

async fn push_raw(app: &TestApp, session: &Session, ops: Vec<Value>, device_time: OffsetDateTime, drained: bool) -> Value {
    let response = app
        .post(
            "/api/v1/sync/push",
            &session.access,
            json!({ "device_time": ts(device_time), "queue_drained": drained, "ops": ops }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body
}

fn statuses(results: &Value) -> Vec<String> {
    results["results"].as_array().unwrap().iter().map(|r| r["status"].as_str().unwrap().to_owned()).collect()
}

fn assert_statuses(results: &Value, expected: &[&str]) {
    assert_eq!(statuses(results), expected, "{results}");
}

fn code(results: &Value, index: usize) -> String {
    results["results"][index]["code"].as_str().unwrap_or_default().to_owned()
}

/// Делает вид, что сотрудник вошёл на это устройство час назад: операции на устройстве
/// не могут быть раньше первого входа, а тестам нужны операции «из прошлого».
async fn logged_in_an_hour_ago(app: &TestApp, session: &Session) {
    sqlx::query("UPDATE refresh_tokens SET created_at = now() - interval '1 hour' WHERE device_id = $1")
        .bind(session.device_id)
        .execute(&app.admin)
        .await
        .unwrap();
}

/// Остаток (штуки, пачки) как строки из базы.
async fn balance(f: &Fixture, product: Uuid, warehouse: Uuid) -> (String, String) {
    sqlx::query_as::<_, (String, String)>(
        "SELECT qty_units::numeric(14,3)::text, qty_packs::numeric(14,3)::text FROM stock_balances
         WHERE product_id = $1 AND warehouse_id = $2",
    )
    .bind(product)
    .bind(warehouse)
    .fetch_optional(&f.app.admin)
    .await
    .unwrap()
    .unwrap_or(("0.000".into(), "0.000".into()))
}

async fn receive(f: &Fixture, session: &Session, items: Value) -> Value {
    push(
        &f.app,
        session,
        vec![op(
            "supply.receive",
            json!({ "supply_id": Uuid::new_v4(), "supplier_id": f.supplier, "warehouse_id": f.warehouse, "items": items }),
        )],
    )
    .await
}

fn line(product: Uuid, unit: &str, qty: &str, cost: &str) -> Value {
    json!({ "id": Uuid::new_v4(), "product_id": product, "stock_unit": unit, "received_qty": qty, "unit_cost": cost })
}

// --- Склад ------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stock_operations_move_balances(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;

    let received = receive(
        &f,
        &f.owner,
        json!([
            line(f.screw, "pack", "10", "35.00"),
            line(f.screw, "unit", "50", "0.40"),
            line(f.nails, "unit", "12.5", "50"),
        ]),
    )
    .await;
    assert_statuses(&received, &["applied"]);
    assert_eq!(balance(&f, f.screw, f.warehouse).await, ("50.000".into(), "10.000".into()));
    assert_eq!(balance(&f, f.nails, f.warehouse).await.0, "12.500");

    let results = push(
        &f.app,
        &f.owner,
        vec![
            // Вскрыли 2 пачки: пачек −2, штук +200.
            op("stock.pack_open", json!({ "product_id": f.screw, "warehouse_id": f.warehouse, "packs": "2" })),
            op(
                "stock.write_off",
                json!({ "product_id": f.nails, "warehouse_id": f.warehouse, "stock_unit": "unit", "qty": "0.5", "reason": "Іржа" }),
            ),
            op(
                "stock.adjust",
                json!({ "product_id": f.screw, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "-3", "reason": "Перерахунок" }),
            ),
            op(
                "transfer.create",
                json!({
                    "id": Uuid::new_v4(), "from_warehouse_id": f.warehouse, "to_warehouse_id": f.second_warehouse,
                    "items": [{ "id": Uuid::new_v4(), "product_id": f.screw, "stock_unit": "pack", "qty": "3" }]
                }),
            ),
            // Продали больше, чем есть: остаток уходит в минус, операция не отклоняется.
            op(
                "stock.write_off",
                json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "qty": "4", "reason": "Пошкоджено" }),
            ),
        ],
    )
    .await;
    assert_statuses(&results, &["applied"; 5]);

    assert_eq!(balance(&f, f.screw, f.warehouse).await, ("247.000".into(), "5.000".into()));
    assert_eq!(balance(&f, f.screw, f.second_warehouse).await, ("0.000".into(), "3.000".into()));
    assert_eq!(balance(&f, f.nails, f.warehouse).await.0, "12.000");
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "-4.000");

    // Кэш остатков совпадает с суммой движений.
    assert_eq!(budmaster_server::sync::stock::rebuild_balances(&f.app.app_db).await.unwrap(), 0);
    let mismatches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM stock_balances b LEFT JOIN (
            SELECT product_id, warehouse_id,
                   COALESCE(sum(qty) FILTER (WHERE stock_unit = 'unit'), 0) AS units,
                   COALESCE(sum(qty) FILTER (WHERE stock_unit = 'pack'), 0) AS packs
            FROM stock_movements GROUP BY product_id, warehouse_id
         ) m ON m.product_id = b.product_id AND m.warehouse_id = b.warehouse_id
         WHERE b.qty_units <> COALESCE(m.units, 0) OR b.qty_packs <> COALESCE(m.packs, 0)",
    )
    .fetch_one(&f.app.admin)
    .await
    .unwrap();
    assert_eq!(mismatches, 0);

    // Испорченный кэш команда восстанавливает.
    sqlx::query("UPDATE stock_balances SET qty_units = 999").execute(&f.app.admin).await.unwrap();
    assert!(budmaster_server::sync::stock::rebuild_balances(&f.app.app_db).await.unwrap() > 0);
    assert_eq!(balance(&f, f.screw, f.warehouse).await, ("247.000".into(), "5.000".into()));
}

#[sqlx::test(migrations = "./migrations")]
async fn stock_operations_are_validated(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let w = f.warehouse;
    let cases = [
        // дробное количество для штук
        ("stock.write_off", json!({ "product_id": f.cement, "warehouse_id": w, "stock_unit": "unit", "qty": "1.5", "reason": "x" }), "invalid_qty"),
        // пачки у товара без фасовки
        ("stock.write_off", json!({ "product_id": f.cement, "warehouse_id": w, "stock_unit": "pack", "qty": "1", "reason": "x" }), "no_pack"),
        // без причины
        ("stock.write_off", json!({ "product_id": f.cement, "warehouse_id": w, "stock_unit": "unit", "qty": "1", "reason": "  " }), "reason_required"),
        ("stock.adjust", json!({ "product_id": f.cement, "warehouse_id": w, "stock_unit": "unit", "delta": "0", "reason": "x" }), "invalid_qty"),
        ("stock.pack_open", json!({ "product_id": f.nails, "warehouse_id": w, "packs": "1" }), "no_pack"),
        ("stock.pack_open", json!({ "product_id": f.screw, "warehouse_id": w, "packs": "0.5" }), "invalid_qty"),
        ("stock.pack_open", json!({ "product_id": Uuid::new_v4(), "warehouse_id": w, "packs": "1" }), "product_not_found"),
        ("stock.pack_open", json!({ "product_id": f.screw, "warehouse_id": Uuid::new_v4(), "packs": "1" }), "warehouse_not_found"),
        ("stock.pack_open", json!({ "product_id": f.screw, "warehouse_id": w, "packs": "1", "extra": 1 }), "invalid_payload"),
        ("transfer.create", json!({ "id": Uuid::new_v4(), "from_warehouse_id": w, "to_warehouse_id": w, "items": [] }), "same_warehouse"),
        ("unknown.op", json!({}), "unknown_type"),
        ("sale.create", json!({}), "invalid_payload"),
    ];
    for (op_type, payload, expected) in cases {
        let results = push(&f.app, &f.owner, vec![op(op_type, payload.clone())]).await;
        assert_eq!((statuses(&results)[0].as_str(), code(&results, 0).as_str()), ("rejected", expected), "{op_type} {payload}");
    }
    // Отказы не оставили ни одного движения, но остались в журнале.
    let movements: i64 = sqlx::query_scalar("SELECT count(*) FROM stock_movements").fetch_one(&f.app.admin).await.unwrap();
    assert_eq!(movements, 0);
    assert_eq!(f.app.audit_count("sync.rejected").await, 12);
}

#[sqlx::test(migrations = "./migrations")]
async fn average_cost_follows_receipts(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let avg = || async {
        sqlx::query_scalar::<_, String>("SELECT avg_cost::text FROM product_costs WHERE product_id = $1")
            .bind(f.screw)
            .fetch_one(&f.app.admin)
            .await
            .unwrap()
    };

    // 10 пачек по 35 ₴ = 1000 шт по 0,35.
    receive(&f, &f.owner, json!([line(f.screw, "pack", "10", "35")])).await;
    assert_eq!(avg().await, "0.3500");
    // +1000 шт по 0,45: (1000×0,35 + 1000×0,45) / 2000 = 0,40.
    receive(&f, &f.owner, json!([line(f.screw, "unit", "1000", "0.45")])).await;
    assert_eq!(avg().await, "0.4000");

    // Списание фиксирует себестоимость: 2 пачки × 100 × 0,40 = 40 за пачку.
    push(
        &f.app,
        &f.owner,
        vec![op(
            "stock.write_off",
            json!({ "product_id": f.screw, "warehouse_id": f.warehouse, "stock_unit": "pack", "qty": "2", "reason": "Брак" }),
        )],
    )
    .await;
    let cost: String = sqlx::query_scalar("SELECT unit_cost::text FROM stock_movements WHERE kind = 'write_off'")
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(cost, "40.0000");

    // Остаток ушёл в ноль и ниже — новая средняя равна цене новой поставки.
    push(
        &f.app,
        &f.owner,
        vec![op(
            "stock.adjust",
            json!({ "product_id": f.screw, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "-5000", "reason": "Ревізія" }),
        )],
    )
    .await;
    receive(&f, &f.owner, json!([line(f.screw, "unit", "10", "0.5")])).await;
    assert_eq!(avg().await, "0.5000");
}

#[sqlx::test(migrations = "./migrations")]
async fn planned_supply_is_received_once_with_discrepancies(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let supply = Uuid::new_v4();
    let planned = push(
        &f.app,
        &f.owner,
        vec![op(
            "supply.create",
            json!({
                "id": supply, "supplier_id": f.supplier, "warehouse_id": f.warehouse, "expected_on": "2026-12-01",
                "items": [
                    { "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "expected_qty": "40" },
                    { "id": Uuid::new_v4(), "product_id": f.nails, "stock_unit": "unit", "expected_qty": "5" }
                ]
            }),
        )],
    )
    .await;
    assert_statuses(&planned, &["applied"]);

    // На сайте ожидаемый товар — «скоро з'явиться».
    let card = f.app.request("GET", &format!("/api/v1/public/products/{}", f.cement), None, None).await;
    assert_eq!(card.body["availability"], "coming_soon");

    // Две приёмки одной поставки с двух устройств.
    let second_device = f.app.login("owner", common::OWNER_PASSWORD).await;
    let receive_op = |qty: &str| {
        op(
            "supply.receive",
            json!({ "supply_id": supply, "items": [
                { "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "received_qty": qty, "unit_cost": "150" },
                { "id": Uuid::new_v4(), "product_id": f.screw, "stock_unit": "pack", "received_qty": "2", "unit_cost": "30" }
            ]}),
        )
    };
    let (first, second) =
        tokio::join!(push(&f.app, &f.owner, vec![receive_op("38")]), push(&f.app, &second_device, vec![receive_op("40")]));
    let mut outcome = [statuses(&first)[0].clone(), statuses(&second)[0].clone()];
    outcome.sort();
    assert_eq!(outcome, ["applied", "rejected"]);
    let rejected = if statuses(&first)[0] == "rejected" { &first } else { &second };
    assert_eq!(code(rejected, 0), "already_received");

    // Приход проведён ровно один раз.
    let cement = balance(&f, f.cement, f.warehouse).await.0;
    assert!(cement == "38.000" || cement == "40.000", "{cement}");
    assert_eq!(balance(&f, f.screw, f.warehouse).await.1, "2.000", "строка не из плана добавлена");
    let card = f.app.request("GET", &format!("/api/v1/public/products/{}", f.cement), None, None).await;
    assert_eq!(card.body["availability"], "in_stock");

    // Расхождения: цвяхи не пришли вовсе; цемент — если пришло 38 из 40.
    let lines: Value = sqlx::query_scalar("SELECT details->'lines' FROM audit_log WHERE action = 'supply.discrepancy'")
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    let lines = lines.as_array().unwrap();
    assert!(lines.iter().any(|l| l["product_id"] == f.nails.to_string() && l["received"] == "0"));
    // + строка сверх плана (шурупы пачками) — тоже расхождение.
    assert_eq!(lines.len(), if cement == "38.000" { 3 } else { 2 });

    // Отменить принятую нельзя.
    let cancel = push(&f.app, &f.owner, vec![op("supply.cancel", json!({ "supply_id": supply }))]).await;
    assert_eq!(code(&cancel, 0), "already_received");
}

// --- Повторы ----------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn the_same_operation_is_applied_once(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let write_off = op(
        "stock.write_off",
        json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "qty": "1", "reason": "Бій" }),
    );

    assert_statuses(&push(&f.app, &f.owner, vec![write_off.clone()]).await, &["applied"]);
    // Связь оборвалась, устройство отправило ту же операцию ещё раз.
    assert_statuses(&push(&f.app, &f.owner, vec![write_off.clone()]).await, &["duplicate"]);
    // И ещё дважды одновременно.
    let (a, b) = tokio::join!(push(&f.app, &f.owner, vec![write_off.clone()]), push(&f.app, &f.owner, vec![write_off.clone()]));
    assert_statuses(&a, &["duplicate"]);
    assert_statuses(&b, &["duplicate"]);
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "-1.000");

    // Отклонённая операция при повторе остаётся отклонённой с тем же кодом.
    let bad = op("stock.pack_open", json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "packs": "1" }));
    assert_eq!(code(&push(&f.app, &f.owner, vec![bad.clone()]).await, 0), "no_pack");
    let again = push(&f.app, &f.owner, vec![bad]).await;
    assert_eq!((statuses(&again)[0].as_str(), code(&again, 0).as_str()), ("rejected", "no_pack"));

    // Чужой сотрудник с тем же op_id не выполняет и не узнаёт ничего чужого.
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let stolen = push(&f.app, &worker, vec![write_off]).await;
    assert_eq!(code(&stolen, 0), "op_id_conflict");
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "-1.000");
}

#[sqlx::test(migrations = "./migrations")]
async fn simultaneous_different_operations_all_count(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let one = |qty: &str| {
        op(
            "stock.adjust",
            json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": qty, "reason": "Прихід" }),
        )
    };
    // Два работника офлайн меняют остаток одного товара — конфликта нет, всё складывается.
    let (a, b) = tokio::join!(
        push(&f.app, &f.owner, (0..10).map(|_| one("+1")).collect()),
        push(&f.app, &worker, (0..10).map(|_| one("+2")).collect()),
    );
    assert!(statuses(&a).iter().chain(statuses(&b).iter()).all(|s| s == "applied"));
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "30.000");
}

// --- Время и блокировка -----------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn device_clock_is_corrected_and_bounded(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let effective = |op_id: &str| {
        let admin = f.app.admin.clone();
        let op_id: Uuid = op_id.parse().unwrap();
        async move {
            sqlx::query_scalar::<_, OffsetDateTime>("SELECT effective_at FROM sync_operations WHERE op_id = $1")
                .bind(op_id)
                .fetch_one(&admin)
                .await
                .unwrap()
        }
    };
    let switch = |at| op_at("session.warehouse_switch", json!({ "warehouse_id": f.warehouse }), at);
    // Новое устройство: нижней границы времени у него ещё нет.
    let device = f.app.login("owner", common::OWNER_PASSWORD).await;
    logged_in_an_hour_ago(&f.app, &device).await;

    // Часы устройства отстают на 2 часа: время операции сдвигается вперёд.
    let behind = now() - Duration::hours(2);
    let delayed = switch(behind - Duration::minutes(10));
    push_raw(&f.app, &device, vec![delayed.clone()], behind, false).await;
    let at = effective(delayed["op_id"].as_str().unwrap()).await;
    assert!((at - (now() - Duration::minutes(10))).abs() < Duration::minutes(1), "{at}");
    assert_eq!(f.app.audit_count("sync.clock_skew").await, 1);

    // Время из будущего — не позже момента приёма.
    let future = switch(now() + Duration::days(3));
    push_raw(&f.app, &device, vec![future.clone()], now(), false).await;
    assert!(effective(future["op_id"].as_str().unwrap()).await <= now());

    // Очередь отправлена целиком: дальше операции не могут оказаться раньше этого момента.
    let drained_at = now();
    push(&f.app, &f.owner, vec![], ).await;
    let backdated = switch(now() - Duration::days(30));
    push(&f.app, &f.owner, vec![backdated.clone()]).await;
    assert!(effective(backdated["op_id"].as_str().unwrap()).await >= drained_at - Duration::seconds(1));

    // Операция старше 90 дней не принимается — и на новом устройстве, и на устройстве с нижней
    // границей (граница не «омолаживает» старую операцию).
    let fresh = f.app.login("owner", common::OWNER_PASSWORD).await;
    let ancient = push_raw(&f.app, &fresh, vec![switch(now() - Duration::days(120))], now(), false).await;
    assert_eq!(code(&ancient, 0), "too_old");
    let bounded = push_raw(&f.app, &f.owner, vec![switch(now() - Duration::days(120))], now(), false).await;
    assert_eq!(code(&bounded, 0), "too_old");

    // Новый вход на новом устройстве: раньше этого входа операций быть не может.
    let new_device = f.app.login("owner", common::OWNER_PASSWORD).await;
    let before_login = switch(now() - Duration::days(2));
    push_raw(&f.app, &new_device, vec![before_login.clone()], now(), false).await;
    assert!(effective(before_login["op_id"].as_str().unwrap()).await >= now() - Duration::minutes(1));
    assert!(f.app.audit_count("sync.time_adjusted").await >= 1, "подтягивание времени видно в журнале");
}

#[sqlx::test(migrations = "./migrations")]
async fn operations_made_while_blocked_are_rejected_even_after_unblocking(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let path = format!("/api/v1/users/{}", worker.user_id);
    let adjust = |at| {
        op_at(
            "stock.adjust",
            json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+1", "reason": "x" }),
            at,
        )
    };

    let before_block = now() - Duration::seconds(30);
    f.app.post(&format!("{path}/block"), &f.owner.access, json!({})).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let during_block = now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    f.app.post(&format!("{path}/unblock"), &f.owner.access, json!({})).await;

    let again = f.app.try_login("ivan", "worker-password-2", worker.device_id).await;
    let relogin = Session {
        access: again.body["access_token"].as_str().unwrap().to_owned(),
        refresh: again.body["refresh_token"].as_str().unwrap().to_owned(),
        device_id: worker.device_id,
        user_id: worker.user_id,
    };
    let results = push(&f.app, &relogin, vec![adjust(before_block), adjust(during_block)]).await;
    assert_statuses(&results, &["applied", "rejected"]);
    assert_eq!(code(&results, 1), "user_blocked");
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "1.000");
}

#[sqlx::test(migrations = "./migrations")]
async fn blocked_device_sends_its_queue_once(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let adjust = |at| {
        op_at(
            "stock.adjust",
            json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+5", "reason": "Прихід" }),
            at,
        )
    };
    let before = now() - Duration::minutes(5);
    f.app.post(&format!("/api/v1/users/{}/block", worker.user_id), &f.owner.access, json!({})).await;
    let after = now() + Duration::seconds(1);

    // Обычная синхронизация уже закрыта.
    let closed = f
        .app
        .post("/api/v1/sync/push", &worker.access, json!({ "device_time": ts(now()), "ops": [adjust(before)] }))
        .await;
    assert_eq!(closed.status, StatusCode::UNAUTHORIZED);

    let drain = |ops: Vec<Value>, device: Uuid| {
        f.app.request(
            "POST",
            "/api/v1/sync/drain",
            None,
            Some(json!({
                "refresh_token": worker.refresh, "device_id": device, "device_time": ts(now()),
                "queue_drained": true, "ops": ops
            })),
        )
    };
    // Чужое устройство с тем же токеном — нет.
    assert_eq!(drain(vec![adjust(before)], Uuid::new_v4()).await.status, StatusCode::UNAUTHORIZED);

    let drained = drain(vec![adjust(before), adjust(after)], worker.device_id).await;
    assert_eq!(drained.status, StatusCode::OK, "{}", drained.body);
    assert_statuses(&drained.body, &["applied", "rejected"]);
    assert_eq!(code(&drained.body, 1), "user_blocked");
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "5.000");
    assert_eq!(f.app.audit_count("sync.drain").await, 1);

    // Второй раз — нельзя: устройство отключено.
    assert_eq!(drain(vec![adjust(before)], worker.device_id).await.status, StatusCode::UNAUTHORIZED);

    // Незаблокированный сотрудник досылкой пользоваться не может.
    let active = f.app.active_worker(&f.owner, "petro").await;
    let not_blocked = f
        .app
        .request(
            "POST",
            "/api/v1/sync/drain",
            None,
            Some(json!({ "refresh_token": active.refresh, "device_id": active.device_id, "device_time": ts(now()), "ops": [] })),
        )
        .await;
    assert_eq!(not_blocked.status, StatusCode::UNAUTHORIZED);
}

// --- Справочники ------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn last_edit_wins_per_field_in_any_arrival_order(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let product = |name: &str, price: &str| {
        let admin = f.app.admin.clone();
        let (name, price) = (name.to_owned(), price.to_owned());
        async move {
            let row: (String, String) = sqlx::query_as("SELECT name, price::text FROM products WHERE id = $1")
                .bind(f.screw)
                .fetch_one(&admin)
                .await
                .unwrap();
            assert_eq!(row, (name, price));
        }
    };
    let t1 = now() - Duration::minutes(10);
    let t2 = now() - Duration::minutes(5);
    let edit = |fields: Value, at| op_at("product.update", json!({ "id": f.screw, "fields": fields }), at);
    // Правки приходят с устройств, которые ещё не отправили всю очередь: их время не «подтягивается».
    let device = f.app.login("owner", common::OWNER_PASSWORD).await;
    logged_in_an_hour_ago(&f.app, &device).await;
    let push = |_device_name: &'static str, ops: Vec<Value>| {
        let app = &f.app;
        let device = &device;
        async move { push_raw(app, device, ops, now(), false).await }
    };

    // Позднюю правку прислали первой, раннюю — второй: ранняя проигрывает.
    push("device", vec![edit(json!({ "name": "Шуруп новий" }), t2)]).await;
    push("device", vec![edit(json!({ "name": "Шуруп старий" }), t1)]).await;
    product("Шуруп новий", "0.45").await;

    // Обычный порядок: поздняя побеждает.
    push("device", vec![edit(json!({ "price": "0.50" }), t1)]).await;
    push("device", vec![edit(json!({ "price": "0.55" }), t2)]).await;
    product("Шуруп новий", "0.55").await;

    // Разные поля не мешают друг другу: старая правка цены проиграла, а старая правка
    // описания (его никто не трогал) применилась.
    push("device", vec![edit(json!({ "price": "0.10", "description": "Опис" }), t1)]).await;
    product("Шуруп новий", "0.55").await;
    let description: String = sqlx::query_scalar("SELECT description FROM products WHERE id = $1")
        .bind(f.screw)
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(description, "Опис");

    // В журнале обе правки, в том числе проигравшая.
    let ignored: Vec<Value> = sqlx::query_scalar(
        "SELECT details->'ignored_older_than_saved' FROM audit_log WHERE action = 'product.update' ORDER BY synced_at",
    )
    .fetch_all(&f.app.admin)
    .await
    .unwrap();
    assert_eq!(ignored.len(), 5);
    assert_eq!(ignored[1], json!(["name"]));
    assert_eq!(ignored[4], json!(["price"]));
}

#[sqlx::test(migrations = "./migrations")]
async fn reference_edits_check_permissions_and_rules(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    // Работник с правом «товары», но без права «цены».
    f.app.create_worker(&f.owner, "editor", 1).await;
    let first = f.app.login("editor", WORKER_PASSWORD).await;
    let changed = f
        .app
        .post(
            "/api/v1/auth/change-password",
            &first.access,
            json!({ "current_password": WORKER_PASSWORD, "new_password": "worker-password-2" }),
        )
        .await;
    let editor = Session {
        access: changed.body["access_token"].as_str().unwrap().to_owned(),
        refresh: changed.body["refresh_token"].as_str().unwrap().to_owned(),
        device_id: first.device_id,
        user_id: first.user_id,
    };

    let results = push(
        &f.app,
        &editor,
        vec![
            op("product.update", json!({ "id": f.screw, "fields": { "name": "Можна" } })),
            op("product.update", json!({ "id": f.screw, "fields": { "name": "Ціна разом", "price": "1.00" } })),
            op("category.create", json!({ "id": Uuid::new_v4(), "name": "Без права" })),
            op("supplier.create", json!({ "id": Uuid::new_v4(), "name": "Без права" })),
            op("warehouse.create", json!({ "id": Uuid::new_v4(), "name": "Без права" })),
        ],
    )
    .await;
    assert_statuses(&results, &["applied", "rejected", "rejected", "rejected", "rejected"]);
    assert!((1..5).all(|i| code(&results, i) == "permission_required"));

    // Единицу и фасовку нельзя менять, когда по товару уже есть движения.
    receive(&f, &f.owner, json!([line(f.screw, "unit", "1", "0.4")])).await;
    let frozen = push(&f.app, &f.owner, vec![op("product.update", json!({ "id": f.screw, "fields": { "pack_size": 50 } }))]).await;
    assert_eq!(code(&frozen, 0), "unit_frozen");
    // Товар без движений — можно, но фасовка должна остаться целой.
    let half = push(&f.app, &f.owner, vec![op("product.update", json!({ "id": f.cement, "fields": { "pack_size": 10 } }))]).await;
    assert_eq!(code(&half, 0), "pack_incomplete");
    let whole = push(
        &f.app,
        &f.owner,
        vec![op("product.update", json!({ "id": f.cement, "fields": { "pack_size": 10, "pack_price": "1800" } }))],
    )
    .await;
    assert_statuses(&whole, &["applied"]);

    // Дерево категорий не замыкается в кольцо.
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let tree = push(
        &f.app,
        &f.owner,
        vec![
            op("category.create", json!({ "id": parent, "name": "Батьківська" })),
            op("category.create", json!({ "id": child, "parent_id": parent, "name": "Дочірня" })),
            op("category.update", json!({ "id": parent, "fields": { "parent_id": child } })),
            op("category.update", json!({ "id": parent, "fields": { "parent_id": parent } })),
        ],
    )
    .await;
    assert_statuses(&tree, &["applied", "applied", "rejected", "rejected"]);
    assert_eq!(code(&tree, 2), "category_cycle");

    // Склад с тем же названием — нет; неизвестное поле — нет.
    let dup = push(&f.app, &f.owner, vec![op("warehouse.create", json!({ "id": Uuid::new_v4(), "name": "Склад 2" }))]).await;
    assert_eq!(code(&dup, 0), "name_taken");
    let unknown = push(&f.app, &f.owner, vec![op("product.update", json!({ "id": f.screw, "fields": { "change_seq": 1 } }))]).await;
    assert_eq!(code(&unknown, 0), "unknown_field");

    // Удалённый товар пропадает с сайта.
    push(&f.app, &f.owner, vec![op("product.delete", json!({ "id": f.nails }))]).await;
    let card = f.app.request("GET", &format!("/api/v1/public/products/{}", f.nails), None, None).await;
    assert_eq!(card.status, StatusCode::NOT_FOUND);
}

// --- Загрузка изменений -----------------------------------------------------------

async fn pull(app: &TestApp, session: &Session, cursor: i64, limit: i64) -> Value {
    let response = app.get(&format!("/api/v1/sync/pull?cursor={cursor}&limit={limit}"), &session.access).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body
}

#[sqlx::test(migrations = "./migrations")]
async fn pull_returns_everything_in_order_with_tombstones(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, &f.owner, json!([line(f.screw, "pack", "2", "35")])).await;
    push(&f.app, &f.owner, vec![op("product.delete", json!({ "id": f.nails }))]).await;

    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let first = pull(&f.app, &worker, 0, 2000).await;
    assert_eq!(first["has_more"], false);
    assert!(first["units"].as_array().unwrap().iter().any(|u| u["code"] == "m3"));
    let changes = first["changes"].as_array().unwrap();
    let seqs: Vec<i64> = changes.iter().map(|c| c["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "по возрастанию без повторов");
    assert_eq!(first["cursor"].as_i64().unwrap(), *seqs.last().unwrap());

    let kinds: std::collections::HashSet<&str> = changes.iter().map(|c| c["type"].as_str().unwrap()).collect();
    for kind in ["warehouse", "category", "product", "supplier", "stock_balance", "supply"] {
        assert!(kinds.contains(kind), "{kind}");
    }
    let nails = changes.iter().find(|c| c["type"] == "product" && c["data"]["id"] == f.nails.to_string()).unwrap();
    assert_eq!(nails["data"]["is_deleted"], true, "удалённое приходит надгробием");
    let screw = changes.iter().find(|c| c["type"] == "product" && c["data"]["id"] == f.screw.to_string()).unwrap();
    assert_eq!(screw["data"]["price"], "0.45", "деньги строкой");
    let stock = changes.iter().find(|c| c["type"] == "stock_balance").unwrap();
    assert_eq!(stock["data"]["qty_packs"], "2.000");
    let supply = changes.iter().find(|c| c["type"] == "supply").unwrap();
    assert_eq!(supply["data"]["items"][0]["unit_cost"], "35.0000", "закупочную цену видит и работник");

    // Ни себестоимости, ни сотрудников, ни служебных полей.
    let text = first.to_string();
    for forbidden in ["avg_cost", "password", "login", "field_updated_at", "permissions"] {
        assert!(!text.contains(forbidden), "{forbidden}");
    }

    // Следующая загрузка с курсором — только новое.
    let empty = pull(&f.app, &worker, first["cursor"].as_i64().unwrap(), 500).await;
    assert_eq!(empty["changes"].as_array().unwrap().len(), 0);
    assert!(empty.get("units").is_none());
    push(&f.app, &f.owner, vec![op("product.update", json!({ "id": f.screw, "fields": { "name": "Шуруп оновлений" } }))]).await;
    let next = pull(&f.app, &worker, first["cursor"].as_i64().unwrap(), 500).await;
    let names: Vec<&str> = next["changes"].as_array().unwrap().iter().filter_map(|c| c["data"]["name"].as_str()).collect();
    assert_eq!(names, ["Шуруп оновлений"]);

    for bad in ["cursor=-1", "limit=0", "limit=5000"] {
        let response = f.app.get(&format!("/api/v1/sync/pull?{bad}"), &worker.access).await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn paged_pull_during_concurrent_writes_misses_nothing(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;

    // Пока устройство качает изменения маленькими страницами, другое пишет.
    let writer = async {
        for i in 0..30 {
            let delta = format!("+{}", i + 1);
            push(
                &f.app,
                &f.owner,
                vec![op(
                    "stock.adjust",
                    json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": delta, "reason": "x" }),
                )],
            )
            .await;
        }
    };
    let reader = async {
        let mut cursor = 0;
        let mut seen = Vec::new();
        for _ in 0..200 {
            let page = pull(&f.app, &worker, cursor, 3).await;
            for change in page["changes"].as_array().unwrap() {
                seen.push(change["seq"].as_i64().unwrap());
            }
            cursor = page["cursor"].as_i64().unwrap();
            tokio::task::yield_now().await;
        }
        (cursor, seen)
    };
    let ((), (mut cursor, mut seen)) = tokio::join!(writer, reader);
    // Дочитываем остаток после окончания записи.
    loop {
        let page = pull(&f.app, &worker, cursor, 500).await;
        for change in page["changes"].as_array().unwrap() {
            seen.push(change["seq"].as_i64().unwrap());
        }
        cursor = page["cursor"].as_i64().unwrap();
        if page["has_more"] == false {
            break;
        }
    }

    // Последняя версия остатка, которую видело устройство, совпадает с базой.
    let all: Vec<i64> = sqlx::query_scalar(
        "SELECT change_seq FROM (
            SELECT change_seq FROM warehouses UNION ALL SELECT change_seq FROM categories
            UNION ALL SELECT change_seq FROM products UNION ALL SELECT change_seq FROM suppliers
            UNION ALL SELECT change_seq FROM stock_balances UNION ALL SELECT change_seq FROM supplies
            UNION ALL SELECT change_seq FROM product_images) t ORDER BY 1",
    )
    .fetch_all(&f.app.admin)
    .await
    .unwrap();
    seen.sort_unstable();
    seen.dedup();
    for seq in &all {
        assert!(seen.contains(seq), "пропущено изменение {seq}");
    }
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "465.000");
}

// --- Видимость себестоимости --------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn cost_is_visible_only_to_owners(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, &f.owner, json!([line(f.cement, "unit", "10", "150")])).await;
    push(
        &f.app,
        &f.owner,
        vec![op(
            "stock.write_off",
            json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "qty": "1", "reason": "Бій" }),
        )],
    )
    .await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let path = format!("/api/v1/stock/movements?product_id={}", f.cement);

    let as_owner = f.app.get(&path, &f.owner.access).await;
    let owner_items = as_owner.body["items"].as_array().unwrap();
    assert_eq!(owner_items.len(), 2);
    assert_eq!(owner_items[0]["kind"], "write_off", "новые сверху");
    assert_eq!(owner_items[0]["unit_cost"], "150.0000");

    let as_worker = f.app.get(&path, &worker.access).await;
    let worker_items = as_worker.body["items"].as_array().unwrap();
    assert!(worker_items[0].get("unit_cost").is_none(), "себестоимость списания скрыта");
    assert_eq!(worker_items[1]["unit_cost"], "150.0000", "закупочная цена прихода видна");
    assert_eq!(worker_items[0]["user_full_name"], "Олена Власник");
}

#[sqlx::test(migrations = "./migrations")]
async fn movements_cannot_be_changed_by_the_server_role(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    receive(&f, &f.owner, json!([line(f.cement, "unit", "10", "150")])).await;
    for statement in ["UPDATE stock_movements SET qty = 1000", "DELETE FROM stock_movements", "DELETE FROM supplies"] {
        let error = sqlx::query(statement).execute(&f.app.app_db).await.unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{statement}: {error}");
    }
    let error = sqlx::query("UPDATE stock_movements SET qty = 1000").execute(&f.app.admin).await.unwrap_err();
    assert!(error.to_string().contains("нельзя изменять"), "{error}");
}

#[sqlx::test(migrations = "./migrations")]
async fn push_requires_a_working_session_and_limits_batches(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let body = json!({ "device_time": ts(now()), "ops": [] });
    assert_eq!(f.app.request("POST", "/api/v1/sync/push", None, Some(body.clone())).await.status, StatusCode::UNAUTHORIZED);

    f.app.create_worker(&f.owner, "newbie", 0).await;
    let newbie = f.app.login("newbie", WORKER_PASSWORD).await;
    assert_eq!(f.app.post("/api/v1/sync/push", &newbie.access, body).await.code(), "password_change_required");

    let too_many: Vec<Value> =
        (0..201).map(|_| op("session.warehouse_switch", json!({ "warehouse_id": f.warehouse }))).collect();
    let response = f
        .app
        .post("/api/v1/sync/push", &f.owner.access, json!({ "device_time": ts(now()), "ops": too_many }))
        .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

// --- Исправления по итогам проверки ---------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_broken_operation_does_not_stop_the_queue(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let adjust = op(
        "stock.adjust",
        json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+1", "reason": "x" }),
    );
    let broken = json!({ "op_id": Uuid::new_v4(), "type": "stock.adjust\u{0}", "occurred_at": ts(now()), "payload": {} });
    let results = push(&f.app, &f.owner, vec![broken, adjust]).await;
    assert_statuses(&results, &["rejected", "applied"]);
    assert_eq!(code(&results, 0), "unknown_type");
    // Текст с нулевым байтом в данных операции тоже не держит очередь.
    let nul = op(
        "stock.write_off",
        json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "qty": "1", "reason": "a\u{0}b" }),
    );
    let after = op("session.warehouse_switch", json!({ "warehouse_id": f.warehouse }));
    let results = push(&f.app, &f.owner, vec![nul, after]).await;
    assert_eq!(statuses(&results)[1], "applied", "{results}");
    assert_ne!(statuses(&results)[0], "retry", "{results}");
}

#[sqlx::test(migrations = "./migrations")]
async fn first_send_of_one_operation_from_two_connections_applies_once(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let adjust = op(
        "stock.adjust",
        json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+7", "reason": "x" }),
    );
    let (a, b) = tokio::join!(push(&f.app, &f.owner, vec![adjust.clone()]), push(&f.app, &f.owner, vec![adjust.clone()]));
    let mut both = [statuses(&a)[0].clone(), statuses(&b)[0].clone()];
    both.sort();
    assert_eq!(both, ["applied", "duplicate"]);
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "7.000");
}

#[sqlx::test(migrations = "./migrations")]
async fn staff_changes_and_device_sync_do_not_deadlock(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    let path = format!("/api/v1/users/{}", worker.user_id);

    // Владелец правит работника, пока устройство работника (и самого владельца) синхронизируется.
    let edits = async {
        let mut codes = Vec::new();
        for i in 0..15 {
            let body = json!({ "last_name": format!("Прізвище{i}"), "permissions": i % 2 });
            codes.push(f.app.request("PATCH", &path, Some(&f.owner.access), Some(body)).await.status);
        }
        codes
    };
    let syncs = async {
        let mut all = Vec::new();
        for _ in 0..15 {
            let ops = (0..3)
                .map(|_| {
                    op(
                        "stock.adjust",
                        json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+1", "reason": "x" }),
                    )
                })
                .collect();
            all.extend(statuses(&push(&f.app, &worker, ops).await));
        }
        all
    };
    let (edit_codes, sync_statuses) = tokio::join!(edits, syncs);
    assert!(edit_codes.iter().all(|code| *code == StatusCode::OK), "{edit_codes:?}");
    assert!(sync_statuses.iter().all(|status| status == "applied"), "{sync_statuses:?}");
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "45.000");
}

#[sqlx::test(migrations = "./migrations")]
async fn drain_accepts_only_the_last_token_and_one_caller(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    let worker = f.app.active_worker(&f.owner, "ivan").await;
    // Старый токен цепочки: после обновления он уже не последний.
    let rotated = f
        .app
        .request(
            "POST",
            "/api/v1/auth/refresh",
            None,
            Some(json!({ "refresh_token": worker.refresh, "device_id": worker.device_id })),
        )
        .await;
    let latest = rotated.body["refresh_token"].as_str().unwrap().to_owned();
    let before_block = now() - Duration::seconds(1);
    f.app.post(&format!("/api/v1/users/{}/block", worker.user_id), &f.owner.access, json!({})).await;

    // Операции сделаны до блокировки — их досылка и принимается.
    let adjust = || {
        op_at(
            "stock.adjust",
            json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+1", "reason": "x" }),
            before_block,
        )
    };
    let drain = |token: &str, ops: Vec<Value>, drained: bool| {
        f.app.request(
            "POST",
            "/api/v1/sync/drain",
            None,
            Some(json!({
                "refresh_token": token, "device_id": worker.device_id, "device_time": ts(now()),
                "queue_drained": drained, "ops": ops
            })),
        )
    };

    assert_eq!(drain(&worker.refresh, vec![adjust()], true).await.status, StatusCode::UNAUTHORIZED, "старый токен");

    // Длинная очередь — несколькими запросами: пока не пришёл последний, устройство открыто.
    let part = drain(&latest, vec![adjust()], false).await;
    assert_eq!(part.status, StatusCode::OK, "{}", part.body);

    // Две досылки одновременно: проходит одна.
    let (a, b) = tokio::join!(drain(&latest, vec![adjust()], true), drain(&latest, vec![adjust()], true));
    let mut codes = [a.status, b.status];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::UNAUTHORIZED]);
    assert_eq!(drain(&latest, vec![adjust()], true).await.status, StatusCode::UNAUTHORIZED, "после последней — закрыто");
    assert_eq!(balance(&f, f.cement, f.warehouse).await.0, "2.000");
}

#[sqlx::test(migrations = "./migrations")]
async fn cost_and_discrepancies_edge_cases(pool: PgPoolOptions, conn: PgConnectOptions) {
    let f = fixture(pool, conn).await;
    // Начальный остаток внесли корректировкой — его цена неизвестна.
    push(
        &f.app,
        &f.owner,
        vec![op(
            "stock.adjust",
            json!({ "product_id": f.cement, "warehouse_id": f.warehouse, "stock_unit": "unit", "delta": "+1000", "reason": "Початковий залишок" }),
        )],
    )
    .await;
    receive(&f, &f.owner, json!([line(f.cement, "unit", "10", "100")])).await;
    let avg: String = sqlx::query_scalar("SELECT avg_cost::text FROM product_costs WHERE product_id = $1")
        .bind(f.cement)
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(avg, "100.0000", "не смесь с нулём");

    // План в пачках, а пришли штуки: и «пачки: 0», и лишняя строка в штуках — расхождения.
    let supply = Uuid::new_v4();
    push(
        &f.app,
        &f.owner,
        vec![op(
            "supply.create",
            json!({ "id": supply, "supplier_id": f.supplier, "warehouse_id": f.warehouse,
                    "items": [{ "id": Uuid::new_v4(), "product_id": f.screw, "stock_unit": "pack", "expected_qty": "5" }] }),
        )],
    )
    .await;
    let received = push(
        &f.app,
        &f.owner,
        vec![op("supply.receive", json!({ "supply_id": supply, "items": [line(f.screw, "unit", "500", "0.4")] }))],
    )
    .await;
    assert_statuses(&received, &["applied"]);
    let lines: Value = sqlx::query_scalar("SELECT details->'lines' FROM audit_log WHERE action = 'supply.discrepancy'")
        .fetch_one(&f.app.admin)
        .await
        .unwrap();
    assert_eq!(lines.as_array().unwrap().len(), 2, "{lines}");

    // Чужой склад при приёмке запланированной поставки — отказ; удалённый поставщик — тоже.
    let other = Uuid::new_v4();
    push(
        &f.app,
        &f.owner,
        vec![op(
            "supply.create",
            json!({ "id": other, "supplier_id": f.supplier, "warehouse_id": f.warehouse,
                    "items": [{ "id": Uuid::new_v4(), "product_id": f.cement, "stock_unit": "unit", "expected_qty": "1" }] }),
        )],
    )
    .await;
    let mismatch = push(
        &f.app,
        &f.owner,
        vec![op(
            "supply.receive",
            json!({ "supply_id": other, "warehouse_id": f.second_warehouse, "items": [line(f.cement, "unit", "1", "1")] }),
        )],
    )
    .await;
    assert_eq!(code(&mismatch, 0), "supply_mismatch");
    push(&f.app, &f.owner, vec![op("supplier.delete", json!({ "id": f.supplier }))]).await;
    let deleted = receive(&f, &f.owner, json!([line(f.cement, "unit", "1", "1")])).await;
    assert_eq!(code(&deleted, 0), "supplier_deleted");

    // Повторный id склада — already_exists, как у категорий и товаров.
    let dup = push(&f.app, &f.owner, vec![op("warehouse.create", json!({ "id": f.second_warehouse, "name": "Інша назва" }))]).await;
    assert_eq!(code(&dup, 0), "already_exists");
}
