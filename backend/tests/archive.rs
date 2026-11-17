//! Тесты архивации.
//!
//! Архивация берёт «сегодня» параметром; команда сервера передаёт настоящую дату. Тесты
//! передают дату на год вперёд, чтобы только что созданные данные стали «старыми».

mod common;

use axum::http::StatusCode;
use budmaster_server::archive::{self, add_months, first_of_month};
use budmaster_server::catalog::import;
use common::{Session, TestApp};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

const CATALOG: &str = r#"{
  "categories": [ { "key": "cement", "name": "Цемент" }, { "key": "screws", "name": "Шурупи" } ],
  "products": [
    { "key": "cement", "category": "cement", "name": "Цемент ПЦ-400", "unit": "bag", "price": "185.00" },
    { "key": "screw", "category": "screws", "name": "Шуруп", "unit": "pcs", "price": "0.45", "pack": { "size": 100, "price": "40.00" } },
    { "key": "nails", "category": "screws", "name": "Цвяхи", "unit": "kg", "price": "68.00" }
  ]
}"#;

fn now() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

async fn push(app: &TestApp, session: &Session, op_type: &str, payload: Value) {
    let body = json!({ "device_time": now(), "ops": [{ "op_id": Uuid::new_v4(), "type": op_type, "occurred_at": now(), "payload": payload }] });
    let r = app.post("/api/v1/sync/push", &session.access, body).await;
    assert_eq!(r.body["results"][0]["status"], "applied", "{op_type}: {}", r.body);
}

async fn kyiv_today(app: &TestApp) -> Date {
    sqlx::query_scalar("SELECT (now() AT TIME ZONE 'Europe/Kyiv')::date").fetch_one(&app.admin).await.unwrap()
}

async fn count(app: &TestApp, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}"))).fetch_one(&app.admin).await.unwrap()
}

async fn balances(app: &TestApp) -> Vec<(Uuid, Uuid, String, String, String)> {
    sqlx::query_as(
        "SELECT product_id, warehouse_id, qty_units::text, qty_packs::text, reserved_units::text
         FROM stock_balances ORDER BY 1, 2",
    )
    .fetch_all(&app.admin)
    .await
    .unwrap()
}

/// Набор документов за текущий месяц: приход, касса, возврат, выданный и отменённый заказы,
/// списание, перемещение, корректировка, вскрытие пачки; плюс незавершённые — открытый заказ
/// и запланированная поставка, которые архивироваться не должны.
async fn activity(app: &TestApp, owner: &Session) -> (Uuid, Uuid) {
    let id = |name: &'static str| {
        let admin = app.admin.clone();
        async move { sqlx::query_scalar::<_, Uuid>("SELECT id FROM products WHERE name = $1").bind(name).fetch_one(&admin).await.unwrap() }
    };
    let (cement, screw, nails) = (id("Цемент ПЦ-400").await, id("Шуруп").await, id("Цвяхи").await);
    let w = app.warehouse_id;
    let w2 = Uuid::new_v4();
    let supplier = Uuid::new_v4();
    push(app, owner, "warehouse.create", json!({ "id": w2, "name": "Склад 2" })).await;
    push(app, owner, "supplier.create", json!({ "id": supplier, "name": "Постачальник" })).await;
    push(app, owner, "supply.receive", json!({ "supply_id": Uuid::new_v4(), "supplier_id": supplier, "warehouse_id": w, "items": [
        { "id": Uuid::new_v4(), "product_id": cement, "stock_unit": "unit", "received_qty": "20", "unit_cost": "150" },
        { "id": Uuid::new_v4(), "product_id": screw, "stock_unit": "pack", "received_qty": "5", "unit_cost": "30" }
    ] })).await;
    let sale = Uuid::new_v4();
    let line = Uuid::new_v4();
    push(app, owner, "sale.create", json!({ "id": sale, "warehouse_id": w, "payment_type": "cash", "items": [
        { "id": line, "product_id": cement, "stock_unit": "unit", "qty": "3", "price": "185" },
        { "id": Uuid::new_v4(), "product_id": nails, "stock_unit": "unit", "qty": "1.5", "price": "68" }
    ] })).await;
    push(app, owner, "return.create", json!({ "id": Uuid::new_v4(), "sale_id": sale, "warehouse_id": w, "reason": "x", "payment_type": "cash",
        "items": [{ "id": Uuid::new_v4(), "sale_item_id": line, "product_id": cement, "stock_unit": "unit", "qty": "1" }] })).await;
    push(app, owner, "stock.write_off", json!({ "product_id": cement, "warehouse_id": w, "stock_unit": "unit", "qty": "1", "reason": "Бій" })).await;
    // Дробное количество с себестоимостью в 4 знака: 0,3 × 0,4166 = 0,12498 — здесь раньше
    // двойное округление давало копейку расхождения между отчётом до и после архивации.
    push(app, owner, "supply.receive", json!({ "supply_id": Uuid::new_v4(), "supplier_id": supplier, "warehouse_id": w, "items": [
        { "id": Uuid::new_v4(), "product_id": nails, "stock_unit": "unit", "received_qty": "10", "unit_cost": "0.4166" }
    ] })).await;
    push(app, owner, "sale.create", json!({ "id": Uuid::new_v4(), "warehouse_id": w, "payment_type": "cash", "items": [
        { "id": Uuid::new_v4(), "product_id": nails, "stock_unit": "unit", "qty": "0.3", "price": "68" }
    ] })).await;
    push(app, owner, "stock.adjust", json!({ "product_id": nails, "warehouse_id": w, "stock_unit": "unit", "delta": "+10", "reason": "Залишок" })).await;
    push(app, owner, "stock.pack_open", json!({ "product_id": screw, "warehouse_id": w, "packs": "1" })).await;
    push(app, owner, "transfer.create", json!({ "id": Uuid::new_v4(), "from_warehouse_id": w, "to_warehouse_id": w2,
        "items": [{ "id": Uuid::new_v4(), "product_id": cement, "stock_unit": "unit", "qty": "2" }] })).await;

    let order = |qty: i64| {
        let id = Uuid::new_v4();
        (id, json!({ "id": id, "customer_name": "Петро", "customer_phone": "0501234567", "delivery_type": "pickup",
                     "items": [{ "id": Uuid::new_v4(), "product_id": cement, "stock_unit": "unit", "qty": qty.to_string() }] }))
    };
    let (done, payload) = order(2);
    push(app, owner, "order.create", payload).await;
    for (status, extra) in [("confirmed", json!({ "warehouse_id": w })), ("assembled", json!({})),
                            ("completed", json!({ "payment_type": "terminal", "sale_id": Uuid::new_v4() }))] {
        let mut p = json!({ "id": done, "status": status });
        for (k, v) in extra.as_object().unwrap() { p[k] = v.clone(); }
        push(app, owner, "order.set_status", p).await;
    }
    let (cancelled, payload) = order(1);
    push(app, owner, "order.create", payload).await;
    push(app, owner, "order.set_status", json!({ "id": cancelled, "status": "cancelled" })).await;

    // Незавершённое — остаётся в базе при любой давности.
    let (open, payload) = order(4);
    push(app, owner, "order.create", payload).await;
    push(app, owner, "order.set_status", json!({ "id": open, "status": "confirmed", "warehouse_id": w })).await;
    let planned = Uuid::new_v4();
    push(app, owner, "supply.create", json!({ "id": planned, "supplier_id": supplier, "warehouse_id": w,
        "items": [{ "id": Uuid::new_v4(), "product_id": nails, "stock_unit": "unit", "expected_qty": "5" }] })).await;
    (open, planned)
}

#[sqlx::test(migrations = "./migrations")]
async fn archiving_a_month_keeps_balances_and_reports(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import::run(&app.app_db, import::parse(CATALOG).unwrap()).await.unwrap();
    let owner = app.owner().await;
    let (open_order, planned_supply) = activity(&app, &owner).await;

    let today = kyiv_today(&app).await;
    let month = first_of_month(today);
    let last_day = add_months(month, 1).previous_day().unwrap();
    let report = |group: &'static str| {
        let path = format!("/api/v1/reports/summary?from={month}&to={last_day}&group_by={group}");
        let app = &app;
        let token = owner.access.clone();
        async move { app.get(&path, &token).await.body }
    };
    let products_path = format!("/api/v1/reports/products?from={month}&to={last_day}");
    let before = (report("channel").await, report("warehouse").await, report("category").await);
    let products_before = app.get(&products_path, &owner.access).await.body["items"].clone();
    let balances_before = balances(&app).await;
    let site_before = app.request("GET", "/api/v1/public/products", None, None).await.body["items"].clone();
    assert_eq!(before.0["totals"]["turnover"], "1047.40", "проверка набора: 3×185 + 1,5×68 + 2×185 + 0,3×68 — {}", before.0);

    // Год спустя.
    let dir = app.media_dir.join("archive");
    let later = add_months(month, 13).replace_day(5).unwrap();
    let result = archive::run(&app.admin, &dir, later).await.unwrap();
    assert!(result.lines.iter().any(|l| l.starts_with("Данные учёта")), "{:?}", result.lines);
    budmaster_server::audit::ensure_partitions(&app.app_db).await.unwrap();

    // Законченное ушло в архив, незавершённое осталось.
    for table in ["sales", "sale_items", "returns", "transfers", "order_status_history"] {
        assert!(count(&app, table).await <= 2, "{table}");
    }
    assert_eq!(count(&app, "sales").await, 0);
    assert_eq!(count(&app, "returns").await, 0);
    let remaining_orders: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM orders").fetch_all(&app.admin).await.unwrap();
    assert_eq!(remaining_orders, vec![open_order]);
    let remaining_supplies: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM supplies").fetch_all(&app.admin).await.unwrap();
    assert_eq!(remaining_supplies, vec![planned_supply]);
    assert_eq!(count(&app, "stock_movements").await, 0, "все движения месяца — в архиве");

    // Остатки, резервы и сайт — как были; пересчёт из «остаток на начало + движения» сходится.
    assert_eq!(balances(&app).await, balances_before);
    assert_eq!(budmaster_server::sync::stock::rebuild_balances(&app.app_db).await.unwrap(), 0);
    assert_eq!(app.request("GET", "/api/v1/public/products", None, None).await.body["items"], site_before);

    // Отчёт за архивный месяц — из сохранённых итогов, те же цифры.
    let after = (report("channel").await, report("warehouse").await, report("category").await);
    assert_eq!(after.0["totals"], before.0["totals"]);
    assert_eq!(after.0["groups"], before.0["groups"]);
    assert_eq!(after.1["groups"], before.1["groups"]);
    assert_eq!(after.2["groups"], before.2["groups"]);
    assert_eq!(app.get(&products_path, &owner.access).await.body["items"], products_before);
    // Кусок архивного месяца посчитать нельзя — только целиком.
    let partial = app.get(&format!("/api/v1/reports/summary?from={month}&to={month}"), &owner.access).await;
    assert_eq!(partial.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(partial.body["error"]["fields"]["from"], "archived_month_partial");

    // Архив в реестре, файл читается владельцем через API.
    let list = app.get("/api/v1/audit-log/archives", &owner.access).await;
    let operations = list.body["items"].as_array().unwrap().iter().find(|a| a["dataset"] == "operations").unwrap().clone();
    let read = app.get(&format!("/api/v1/audit-log/archives/{}?per_page=200", operations["id"].as_str().unwrap()), &owner.access).await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["total"], operations["row_count"]);
    assert!(read.body["items"].as_array().unwrap().iter().any(|row| row["table"] == "sales"));
    let worker = app.active_worker(&owner, "ivan").await;
    assert_eq!(app.get("/api/v1/audit-log/archives", &worker.access).await.status, StatusCode::FORBIDDEN);

    // Повторный запуск ничего не делает и ничего не ломает.
    let again = archive::run(&app.admin, &dir, later).await.unwrap();
    assert!(again.lines.iter().all(|l| !l.starts_with("Данные учёта")), "{:?}", again.lines);
    assert_eq!(balances(&app).await, balances_before);
}

#[sqlx::test(migrations = "./migrations")]
async fn fresh_data_is_never_archived(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import::run(&app.app_db, import::parse(CATALOG).unwrap()).await.unwrap();
    let owner = app.owner().await;
    activity(&app, &owner).await;
    let sales = count(&app, "sales").await;
    let audit = count(&app, "audit_log").await;

    let today = kyiv_today(&app).await;
    let dir = app.media_dir.join("archive");
    // Через месяц: данные учёта хранятся год, журнал — 3 месяца.
    for months in [0, 1, 2] {
        archive::run(&app.admin, &dir, add_months(first_of_month(today), months)).await.unwrap();
        assert_eq!(count(&app, "sales").await, sales, "+{months} мес.");
        assert_eq!(count(&app, "audit_log").await, audit, "+{months} мес.");
    }
    assert_eq!(count(&app, "audit_archives").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn audit_log_months_are_archived_and_merged_into_a_year(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    // Секции журнала за весь 2025 год, в каждой — записи.
    for month in 1..=12u8 {
        let start = format!("2025-{month:02}-01");
        let end = if month == 12 { "2026-01-01".to_owned() } else { format!("2025-{:02}-01", month + 1) };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE audit_log_2025_{month:02} PARTITION OF audit_log FOR VALUES FROM ('{start}') TO ('{end}')"
        )))
        .execute(&app.admin)
        .await
        .unwrap();
        for _ in 0..u32::from(month) {
            sqlx::query(
                "INSERT INTO audit_log (id, user_full_name, action, occurred_at, synced_at)
                 VALUES ($1, 'тест', 'old.action', $2::date + interval '3 days', $2::date + interval '3 days')",
            )
            .bind(Uuid::new_v4())
            .bind(&start)
            .execute(&app.admin)
            .await
            .unwrap();
        }
    }
    let current_rows = count(&app, "audit_log").await - 78;

    let dir = app.media_dir.join("archive");
    let today = kyiv_today(&app).await;
    let result = archive::run(&app.admin, &dir, today).await.unwrap();
    assert!(result.lines.iter().any(|l| l.contains("объединены")), "{:?}", result.lines);

    // В базе остались только свежие записи; старых секций нет.
    assert_eq!(count(&app, "audit_log").await, current_rows);
    let old_partitions: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_class WHERE relname LIKE 'audit_log_2025_%'")
        .fetch_one(&app.admin)
        .await
        .unwrap();
    assert_eq!(old_partitions, 0);
    // Один годовой файл на 1 + 2 + … + 12 = 78 записей; месячных файлов и записей в реестре нет.
    let archives: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT dataset, period_kind, row_count FROM audit_archives").fetch_all(&app.admin).await.unwrap();
    assert_eq!(archives, vec![("audit_log".to_owned(), "year".to_owned(), 78)]);
    let files: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(files, vec!["audit_log-2025.jsonl.zst".to_owned()]);

    // Повреждённый файл не читается молча.
    let owner = app.owner().await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM audit_archives").fetch_one(&app.admin).await.unwrap();
    let ok = app.get(&format!("/api/v1/audit-log/archives/{id}?per_page=5"), &owner.access).await;
    assert_eq!((ok.body["total"].as_i64(), ok.body["items"].as_array().unwrap().len()), (Some(78), 5));
    std::fs::write(dir.join("audit_log-2025.jsonl.zst"), b"broken").unwrap();
    assert_eq!(app.get(&format!("/api/v1/audit-log/archives/{id}"), &owner.access).await.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[sqlx::test(migrations = "./migrations")]
async fn only_the_archiver_may_delete_protected_rows(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    import::run(&app.app_db, import::parse(CATALOG).unwrap()).await.unwrap();
    let owner = app.owner().await;
    activity(&app, &owner).await;

    // Роль сервера, даже включив разрешение архивации, удалять не может.
    let mut app_conn = app.app_db.acquire().await.unwrap();
    sqlx::query("SELECT set_config('budmaster.archiving', 'on', false)").execute(&mut *app_conn).await.unwrap();
    for table in ["stock_movements", "order_status_history", "sales", "audit_log"] {
        let error = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}"))).execute(&mut *app_conn).await.unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{table}: {error}");
    }
    // Владелец базы без разрешения архивации — тоже нет.
    for table in ["stock_movements", "order_status_history"] {
        let error = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}"))).execute(&app.admin).await.unwrap_err();
        assert!(error.to_string().contains("нельзя изменять"), "{table}: {error}");
    }
    // Изменять нельзя никому и никогда, даже архиватору.
    let mut tx = app.admin.begin().await.unwrap();
    sqlx::query("SELECT set_config('budmaster.archiving', 'on', true)").execute(&mut *tx).await.unwrap();
    let error = sqlx::query("UPDATE stock_movements SET qty = 1").execute(&mut *tx).await.unwrap_err();
    assert!(error.to_string().contains("нельзя изменять"), "{error}");
}
