//! Архивация (docs/design.md, раздел 11).
//!
//! Раз в месяц команда `archive` выгружает старые данные в сжатые файлы и убирает их из базы:
//! - журнал действий и реестр синхронизации — старше 3 полных месяцев;
//! - данные учёта (продажи, заказы, поставки, перемещения, возвраты, движения) — старше 12.
//!
//! Порядок для каждого месяца, в одной транзакции под общей блокировкой изменений:
//! выгрузка → запись файла на диск → чтение файла обратно и проверка числа строк и
//! контрольной суммы → запись в реестр → удаление из базы. Сбой на любом шаге откатывает всё,
//! кроме файла; повторный запуск перезапишет файл и сделает месяц заново. Месяц, который уже
//! есть в реестре, повторно не обрабатывается.
//!
//! Перед удалением данных учёта сохраняются остаток на начало (`stock_opening_balances`)
//! и итоги месяца (`report_monthly_totals`) — остатки и отчёты после архивации не меняются.
//!
//! Последние 2 месяца (текущий и прошлый) в архив не попадают никогда, что бы ни передали.
//!
//! Команда работает под ролью-владельцем базы: только она может удалять из неизменяемых
//! таблиц (и только с включённым `budmaster.archiving`, см. миграцию 0005).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use time::{Date, Month};
use uuid::Uuid;

use crate::sales::reports::TIMEZONE;

const LOG_MONTHS: i32 = 3;
const OPERATIONS_MONTHS: i32 = 12;
/// Реестр синхронизации хранится не меньше этого: операции старше 90 дней сервер не принимает,
/// и до этого срока повтор должен находить запись в реестре.
const SYNC_REGISTRY_MIN_DAYS: i64 = 100;
/// Сжатие идёт, пока держится общая блокировка изменений (синхронизация ждёт), поэтому
/// уровень умеренный: быстро, а файл всё равно в разы меньше исходного текста.
const ZSTD_LEVEL: i32 = 3;

/// Что сделано за запуск — для вывода в терминал.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
}

pub fn first_of_month(date: Date) -> Date {
    date.replace_day(1).expect("1-е число есть в каждом месяце")
}

pub fn add_months(month: Date, months: i32) -> Date {
    let total = month.year() * 12 + i32::from(u8::from(month.month())) - 1 + months;
    let (year, month_index) = (total.div_euclid(12), total.rem_euclid(12));
    let month = Month::try_from(u8::try_from(month_index + 1).expect("1..=12")).expect("1..=12");
    Date::from_calendar_date(year, month, 1).expect("1-е число")
}

/// Защита от архивации свежих данных: месяц должен закончиться не позже начала прошлого месяца.
fn ensure_old_enough(month: Date, today: Date) -> anyhow::Result<()> {
    let previous_month = add_months(first_of_month(today), -1);
    ensure!(
        add_months(month, 1) <= previous_month,
        "месяц {month} слишком свежий: последние 2 месяца в архив не попадают"
    );
    Ok(())
}

/// Запуск архивации. `today` — сегодняшняя дата по киевскому времени (команда берёт её из базы).
pub async fn run(owner_db: &PgPool, dir: &Path, today: Date) -> anyhow::Result<Report> {
    tokio::fs::create_dir_all(dir).await.context("нет доступа к папке архива")?;
    let mut report = Report::default();
    let this_month = first_of_month(today);

    archive_audit_log(owner_db, dir, today, add_months(this_month, -LOG_MONTHS), &mut report).await?;
    archive_sync_registry(owner_db, dir, today, add_months(this_month, -LOG_MONTHS), &mut report).await?;
    archive_operations(owner_db, dir, today, add_months(this_month, -OPERATIONS_MONTHS), &mut report).await?;
    merge_years(owner_db, dir, &mut report).await?;

    if report.lines.is_empty() {
        report.lines.push("Архивировать нечего.".to_owned());
    }
    Ok(report)
}

async fn already_archived(conn: &mut PgConnection, dataset: &str, month: Date) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM audit_archives WHERE dataset = $1 AND (
                (period_kind = 'month' AND period_start = $2)
                OR (period_kind = 'year' AND period_start = make_date(extract(year FROM $2::date)::int, 1, 1))))",
    )
    .bind(dataset)
    .bind(month)
    .fetch_one(conn)
    .await
}

/// Начинает транзакцию архивации: общая блокировка изменений и разрешение удалять.
async fn begin(db: &PgPool) -> anyhow::Result<sqlx::Transaction<'static, sqlx::Postgres>> {
    let mut tx = db.begin().await?;
    crate::sync::lock_changes(&mut tx).await?;
    sqlx::query("SELECT set_config('budmaster.archiving', 'on', true)").execute(&mut *tx).await?;
    Ok(tx)
}

async fn register(
    conn: &mut PgConnection,
    dataset: &str,
    kind: &str,
    start: Date,
    file: &ArchiveFile,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO audit_archives (id, dataset, period_kind, period_start, file_name, row_count, sha256)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::new_v4())
    .bind(dataset)
    .bind(kind)
    .bind(start)
    .bind(&file.name)
    .bind(i64::try_from(file.rows).unwrap_or(i64::MAX))
    .bind(&file.sha256)
    .execute(conn)
    .await?;
    Ok(())
}

fn month_label(month: Date) -> String {
    format!("{}-{:02}", month.year(), u8::from(month.month()))
}

// --- Журнал действий -------------------------------------------------------------

async fn archive_audit_log(
    db: &PgPool,
    dir: &Path,
    today: Date,
    before: Date,
    report: &mut Report,
) -> anyhow::Result<()> {
    // Секции журнала — по календарным месяцам (UTC): audit_log_2026_08.
    let partitions: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i
         JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent
         WHERE p.relname = 'audit_log' ORDER BY 1",
    )
    .fetch_all(db)
    .await?;

    for partition in partitions {
        let Some(month) = parse_partition(&partition) else { continue };
        if month >= before {
            continue;
        }
        ensure_old_enough(month, today)?;
        let mut tx = begin(db).await?;
        if already_archived(&mut tx, "audit_log", month).await? {
            continue;
        }
        // Имя секции — из каталога базы и проверено разбором выше: только audit_log_ГГГГ_ММ.
        let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT jsonb_build_object('table', 'audit_log', 'row', to_jsonb(t))::text
             FROM {partition} t ORDER BY synced_at, id"
        )))
        .fetch_all(&mut *tx)
        .await?;
        let file = write_file(dir, &format!("audit_log-{}.jsonl.zst", month_label(month)), &lines).await?;
        register(&mut tx, "audit_log", "month", month, &file).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {partition}"))).execute(&mut *tx).await?;
        tx.commit().await?;
        report.lines.push(format!("Журнал за {}: {} записей → {}", month_label(month), file.rows, file.name));
    }
    Ok(())
}

fn parse_partition(name: &str) -> Option<Date> {
    let rest = name.strip_prefix("audit_log_")?;
    let (year, month) = rest.split_once('_')?;
    let (year, month): (i32, u8) = (year.parse().ok()?, month.parse().ok()?);
    if year.to_string().len() != 4 || month == 0 || month > 12 {
        return None;
    }
    Date::from_calendar_date(year, Month::try_from(month).ok()?, 1).ok()
}

// --- Реестр синхронизации ----------------------------------------------------------

async fn archive_sync_registry(
    db: &PgPool,
    dir: &Path,
    today: Date,
    before: Date,
    report: &mut Report,
) -> anyhow::Result<()> {
    let months: Vec<Date> = sqlx::query_scalar(
        "SELECT DISTINCT date_trunc('month', received_at AT TIME ZONE 'UTC')::date FROM sync_operations
         WHERE received_at < ($1::date::timestamp AT TIME ZONE 'UTC') ORDER BY 1",
    )
    .bind(before)
    .fetch_all(db)
    .await?;

    for month in months {
        ensure_old_enough(month, today)?;
        let end = add_months(month, 1);
        // Дополнительно к границе месяца — не моложе 100 дней: пока операция может прийти
        // повторно, запись о ней должна оставаться в реестре. Такой месяц уйдёт в следующий раз.
        if end > today - time::Duration::days(SYNC_REGISTRY_MIN_DAYS) {
            continue;
        }
        let mut tx = begin(db).await?;
        if already_archived(&mut tx, "sync_operations", month).await? {
            continue;
        }
        const RANGE: &str = "received_at >= ($1::date::timestamp AT TIME ZONE 'UTC')
                             AND received_at < ($2::date::timestamp AT TIME ZONE 'UTC')";
        let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT jsonb_build_object('table', 'sync_operations', 'row', to_jsonb(t))::text
             FROM sync_operations t WHERE {RANGE} ORDER BY received_at, op_id"
        )))
        .bind(month)
        .bind(end)
        .fetch_all(&mut *tx)
        .await?;
        let file = write_file(dir, &format!("sync_operations-{}.jsonl.zst", month_label(month)), &lines).await?;
        register(&mut tx, "sync_operations", "month", month, &file).await?;
        let deleted = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM sync_operations WHERE {RANGE}")))
            .bind(month)
            .bind(end)
            .execute(&mut *tx)
            .await?;
        ensure!(deleted.rows_affected() == file.rows, "реестр синхронизации: удалено не столько, сколько выгружено");
        tx.commit().await?;
        report.lines.push(format!("Реестр синхронизации за {}: {} записей", month_label(month), file.rows));
    }
    Ok(())
}

// --- Данные учёта --------------------------------------------------------------------

/// Таблицы документов в порядке выгрузки и (в обратном порядке) удаления.
/// Временная таблица `arch_<таблица>` содержит id строк, уходящих в архив.
const OPERATION_TABLES: [&str; 12] = [
    "sales",
    "sale_items",
    "orders",
    "order_items",
    "order_status_history",
    "returns",
    "return_items",
    "supplies",
    "supply_items",
    "transfers",
    "transfer_items",
    "stock_movements",
];
/// Порядок удаления: сначала то, что ссылается, потом то, на что ссылаются.
const DELETE_ORDER: [&str; 12] = [
    "stock_movements",
    "return_items",
    "returns",
    "sale_items",
    "sales",
    "order_status_history",
    "order_items",
    "orders",
    "supply_items",
    "supplies",
    "transfer_items",
    "transfers",
];

async fn archive_operations(
    db: &PgPool,
    dir: &Path,
    today: Date,
    before: Date,
    report: &mut Report,
) -> anyhow::Result<()> {
    // Месяцы по киевскому времени, в которых есть законченные документы или движения.
    let months: Vec<Date> = sqlx::query_scalar(
        "SELECT DISTINCT date_trunc('month', at AT TIME ZONE $2)::date AS month FROM (
            SELECT occurred_at AS at FROM stock_movements
            UNION ALL SELECT occurred_at FROM sales
            UNION ALL SELECT occurred_at FROM returns
            UNION ALL SELECT occurred_at FROM transfers
            UNION ALL SELECT COALESCE(received_at, created_at) FROM supplies WHERE status <> 'planned'
            UNION ALL SELECT h.occurred_at FROM order_status_history h JOIN orders o ON o.id = h.order_id
                      WHERE o.status IN ('completed', 'cancelled')
         ) t
         WHERE at < ($1::date::timestamp AT TIME ZONE $2)
         ORDER BY 1",
    )
    .bind(before)
    .bind(TIMEZONE)
    .fetch_all(db)
    .await?;

    for month in months {
        ensure_old_enough(month, today)?;
        let mut tx = begin(db).await?;
        if already_archived(&mut tx, "operations", month).await? {
            continue;
        }
        let end = add_months(month, 1);
        select_documents(&mut tx, month, end).await?;
        save_month_totals(&mut tx, month).await?;
        save_opening_balances(&mut tx, end).await?;

        let mut lines = Vec::new();
        for table in OPERATION_TABLES {
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT jsonb_build_object('table', '{table}', 'row', to_jsonb(t))::text
                 FROM {table} t WHERE t.id IN (SELECT id FROM arch_{table}) ORDER BY t.id"
            )))
            .fetch_all(&mut *tx)
            .await?;
            lines.extend(rows);
        }
        if lines.is_empty() {
            tx.rollback().await?;
            continue;
        }
        let file = write_file(dir, &format!("operations-{}.jsonl.zst", month_label(month)), &lines).await?;
        register(&mut tx, "operations", "month", month, &file).await?;

        let mut deleted = 0;
        for table in DELETE_ORDER {
            deleted += sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE id IN (SELECT id FROM arch_{table})"
            )))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        ensure!(deleted == file.rows, "данные учёта за {month}: удалено {deleted}, выгружено {}", file.rows);
        tx.commit().await?;
        report.lines.push(format!("Данные учёта за {}: {} записей → {}", month_label(month), file.rows, file.name));
    }
    Ok(())
}

/// Отбирает документы месяца во временные таблицы `arch_*`.
///
/// Связанные документы уходят вместе: заказ — вместе со своей продажей (по дате выдачи),
/// отменённый заказ — по дате отмены, поставка — по дате приёмки или создания (отменённая),
/// движения — вместе со своими документами. Незавершённые заказы и запланированные поставки
/// не архивируются никогда.
async fn select_documents(conn: &mut PgConnection, month: Date, end: Date) -> sqlx::Result<()> {
    const IN_MONTH: &str = ">= ($1::date::timestamp AT TIME ZONE $3) AND {col} < ($2::date::timestamp AT TIME ZONE $3)";
    let range = |col: &str| format!("{col} {}", IN_MONTH.replace("{col}", col));
    let statements = [
        format!("CREATE TEMP TABLE arch_sales ON COMMIT DROP AS SELECT id FROM sales WHERE {}", range("occurred_at")),
        "CREATE TEMP TABLE arch_sale_items ON COMMIT DROP AS
            SELECT id FROM sale_items WHERE sale_id IN (SELECT id FROM arch_sales)"
            .to_owned(),
        format!(
            "CREATE TEMP TABLE arch_orders ON COMMIT DROP AS
                SELECT order_id AS id FROM sales WHERE id IN (SELECT id FROM arch_sales) AND order_id IS NOT NULL
              UNION
                SELECT o.id FROM orders o WHERE o.status = 'cancelled' AND (
                    SELECT max(h.occurred_at) FROM order_status_history h WHERE h.order_id = o.id) {}",
            IN_MONTH.replace("{col}", "(SELECT max(h.occurred_at) FROM order_status_history h WHERE h.order_id = o.id)")
        ),
        "CREATE TEMP TABLE arch_order_items ON COMMIT DROP AS
            SELECT id FROM order_items WHERE order_id IN (SELECT id FROM arch_orders)"
            .to_owned(),
        "CREATE TEMP TABLE arch_order_status_history ON COMMIT DROP AS
            SELECT id FROM order_status_history WHERE order_id IN (SELECT id FROM arch_orders)"
            .to_owned(),
        format!("CREATE TEMP TABLE arch_returns ON COMMIT DROP AS SELECT id FROM returns WHERE {}", range("occurred_at")),
        "CREATE TEMP TABLE arch_return_items ON COMMIT DROP AS
            SELECT id FROM return_items WHERE return_id IN (SELECT id FROM arch_returns)"
            .to_owned(),
        format!(
            // Отменённая поставка не знает даты отмены: она уходит вместе с первым месяцем,
            // который архивируется после её создания (а не только с месяцем создания —
            // иначе поставку, отменённую уже после архивации того месяца, не забрал бы никто).
            "CREATE TEMP TABLE arch_supplies ON COMMIT DROP AS SELECT id FROM supplies
             WHERE (status = 'received' AND {}) OR (status = 'cancelled' AND created_at < ($2::date::timestamp AT TIME ZONE $3))",
            range("received_at")
        ),
        "CREATE TEMP TABLE arch_supply_items ON COMMIT DROP AS
            SELECT id FROM supply_items WHERE supply_id IN (SELECT id FROM arch_supplies)"
            .to_owned(),
        format!("CREATE TEMP TABLE arch_transfers ON COMMIT DROP AS SELECT id FROM transfers WHERE {}", range("occurred_at")),
        "CREATE TEMP TABLE arch_transfer_items ON COMMIT DROP AS
            SELECT id FROM transfer_items WHERE transfer_id IN (SELECT id FROM arch_transfers)"
            .to_owned(),
        format!(
            "CREATE TEMP TABLE arch_stock_movements ON COMMIT DROP AS SELECT id FROM stock_movements m WHERE
                m.sale_item_id IN (SELECT id FROM arch_sale_items)
                OR m.return_item_id IN (SELECT id FROM arch_return_items)
                OR m.supply_item_id IN (SELECT id FROM arch_supply_items)
                OR m.transfer_item_id IN (SELECT id FROM arch_transfer_items)
                OR (m.kind IN ('write_off', 'adjustment', 'pack_open') AND {})",
            range("m.occurred_at")
        ),
    ];
    for statement in statements {
        // Текст запросов собран из констант этой функции; даты и пояс — параметрами
        // (только там, где они есть в запросе).
        let uses_period = statement.contains("$1");
        let query = sqlx::query(sqlx::AssertSqlSafe(statement));
        let query = if uses_period { query.bind(month).bind(end).bind(TIMEZONE) } else { query };
        query.execute(&mut *conn).await?;
    }
    Ok(())
}

/// Итоги месяца по каналу, складу и товару — из тех же строк, что считает отчёт.
async fn save_month_totals(conn: &mut PgConnection, month: Date) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO report_monthly_totals
            (month, channel, warehouse_id, product_id, turnover, returns, turnover_unknown_cost, returns_unknown_cost,
             sale_cost, return_cost, losses, unknown_cost_lines, unknown_loss_lines)
         SELECT $1, channel, warehouse_id, product_id, sum(turnover), sum(returns),
                sum(CASE WHEN unknown_cost = 1 THEN turnover ELSE 0 END),
                sum(CASE WHEN unknown_cost = 1 THEN returns ELSE 0 END),
                sum(sale_cost), sum(return_cost), sum(losses), sum(unknown_cost), sum(unknown_loss)
         FROM (
            SELECT s.channel, s.warehouse_id, i.product_id, round(i.qty * i.price, 2) AS turnover, 0 AS returns,
                   COALESCE(i.qty * i.unit_cost, 0) AS sale_cost, 0 AS return_cost, 0 AS losses,
                   (i.unit_cost IS NULL)::int AS unknown_cost, 0 AS unknown_loss
            FROM sales s JOIN sale_items i ON i.sale_id = s.id WHERE s.id IN (SELECT id FROM arch_sales)
          UNION ALL
            SELECT r.channel, r.warehouse_id, i.product_id, 0, round(i.qty * i.price, 2),
                   0, COALESCE(i.qty * i.unit_cost, 0), 0, (i.unit_cost IS NULL)::int, 0
            FROM returns r JOIN return_items i ON i.return_id = r.id WHERE r.id IN (SELECT id FROM arch_returns)
          UNION ALL
            SELECT NULL, m.warehouse_id, m.product_id, 0, 0, 0, 0, COALESCE(-m.qty * m.unit_cost, 0),
                   0, (m.unit_cost IS NULL)::int
            FROM stock_movements m WHERE m.kind = 'write_off' AND m.id IN (SELECT id FROM arch_stock_movements)
         ) t
         GROUP BY channel, warehouse_id, product_id",
    )
    .bind(month)
    .execute(conn)
    .await?;
    Ok(())
}

/// Движения месяца уходят в остаток на начало: текущие остатки от архивации не меняются.
async fn save_opening_balances(conn: &mut PgConnection, end: Date) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO stock_opening_balances (product_id, warehouse_id, qty_units, qty_packs, as_of)
         SELECT product_id, warehouse_id,
                COALESCE(sum(qty) FILTER (WHERE stock_unit = 'unit'), 0),
                COALESCE(sum(qty) FILTER (WHERE stock_unit = 'pack'), 0), $1
         FROM stock_movements WHERE id IN (SELECT id FROM arch_stock_movements)
         GROUP BY product_id, warehouse_id
         ON CONFLICT (product_id, warehouse_id) DO UPDATE SET
            qty_units = stock_opening_balances.qty_units + EXCLUDED.qty_units,
            qty_packs = stock_opening_balances.qty_packs + EXCLUDED.qty_packs,
            as_of = GREATEST(stock_opening_balances.as_of, EXCLUDED.as_of)",
    )
    .bind(end)
    .execute(conn)
    .await?;
    Ok(())
}

// --- Объединение в годовые файлы ------------------------------------------------------

/// Когда за год заархивированы все 12 месяцев набора, их файлы объединяются в один годовой.
async fn merge_years(db: &PgPool, dir: &Path, report: &mut Report) -> anyhow::Result<()> {
    let complete: Vec<(String, i32)> = sqlx::query_as(
        "SELECT dataset, extract(year FROM period_start)::int FROM audit_archives
         WHERE period_kind = 'month' GROUP BY 1, 2 HAVING count(*) = 12",
    )
    .fetch_all(db)
    .await?;

    for (dataset, year) in complete {
        // Объединение меняет только реестр архивов — общая блокировка изменений не нужна,
        // синхронизация во время него не ждёт.
        let mut tx = db.begin().await?;
        let months: Vec<(Uuid, String, i64, Vec<u8>)> = sqlx::query_as(
            "SELECT id, file_name, row_count, sha256 FROM audit_archives
             WHERE dataset = $1 AND period_kind = 'month' AND extract(year FROM period_start) = $2
             ORDER BY period_start FOR UPDATE",
        )
        .bind(&dataset)
        .bind(year)
        .fetch_all(&mut *tx)
        .await?;
        if months.len() != 12 {
            continue;
        }
        let mut lines = Vec::new();
        for (_, name, rows, sha) in &months {
            let month_lines = read_file(dir, name, sha).await?;
            ensure!(i64::try_from(month_lines.len())? == *rows, "{name}: число строк не совпадает с реестром");
            lines.extend(month_lines);
        }
        let file = write_file(dir, &format!("{dataset}-{year}.jsonl.zst"), &lines).await?;
        let start = Date::from_calendar_date(year, Month::January, 1)?;
        register(&mut tx, &dataset, "year", start, &file).await?;
        sqlx::query("DELETE FROM audit_archives WHERE id = ANY($1)")
            .bind(months.iter().map(|m| m.0).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        for (_, name, _, _) in &months {
            let _ = tokio::fs::remove_file(dir.join(name)).await;
        }
        report.lines.push(format!("{dataset} за {year}: 12 месяцев объединены в {}", file.name));
    }
    Ok(())
}

// --- Файлы ---------------------------------------------------------------------------

pub struct ArchiveFile {
    pub name: String,
    pub rows: u64,
    pub sha256: Vec<u8>,
}

/// Пишет строки в сжатый файл, сбрасывает его на диск и читает обратно для проверки.
async fn write_file(dir: &Path, name: &str, lines: &[String]) -> anyhow::Result<ArchiveFile> {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
    for line in lines {
        encoder.write_all(line.as_bytes())?;
        encoder.write_all(b"\n")?;
    }
    let bytes = encoder.finish()?;
    let sha256 = Sha256::digest(&bytes).to_vec();

    crate::catalog::images::store(dir, name, &bytes).await.with_context(|| format!("не удалось записать {name}"))?;

    // Проверка: файл на диске читается, контрольная сумма и число строк совпадают.
    let back = read_file(dir, name, &sha256).await?;
    ensure!(back.len() == lines.len(), "{name}: после записи прочитано {} строк вместо {}", back.len(), lines.len());
    Ok(ArchiveFile { name: name.to_owned(), rows: u64::try_from(lines.len())?, sha256 })
}

/// Читает архивный файл, проверяя контрольную сумму. Возвращает строки JSON.
pub async fn read_file(dir: &Path, name: &str, expected_sha256: &[u8]) -> anyhow::Result<Vec<String>> {
    let bytes = tokio::fs::read(archive_path(dir, name)?).await.with_context(|| format!("не удалось прочитать {name}"))?;
    if Sha256::digest(&bytes).as_slice() != expected_sha256 {
        bail!("{name}: контрольная сумма не совпадает — файл повреждён");
    }
    // Распаковка тяжёлая — вне асинхронных потоков.
    tokio::task::spawn_blocking(move || {
        let mut text = String::new();
        zstd::stream::Decoder::new(bytes.as_slice())?.read_to_string(&mut text)?;
        Ok(text.lines().map(str::to_owned).collect())
    })
    .await?
}

/// Путь к файлу архива. Имя приходит из реестра; всё равно проверяем, что это просто имя.
fn archive_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let simple = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
        && !name.contains("..");
    ensure!(simple, "недопустимое имя файла архива");
    Ok(dir.join(name))
}

#[cfg(test)]
mod tests {
    use time::macros::date;

    use super::*;

    #[test]
    fn months() {
        assert_eq!(add_months(date!(2026 - 11 - 01), -3), date!(2026 - 08 - 01));
        assert_eq!(add_months(date!(2026 - 01 - 01), -1), date!(2025 - 12 - 01));
        assert_eq!(add_months(date!(2026 - 01 - 01), -12), date!(2025 - 01 - 01));
        assert_eq!(add_months(date!(2026 - 12 - 01), 1), date!(2027 - 01 - 01));
        assert_eq!(first_of_month(date!(2026 - 11 - 17)), date!(2026 - 11 - 01));
    }

    #[test]
    fn the_last_two_months_are_never_archived() {
        let today = date!(2026 - 12 - 01);
        assert!(ensure_old_enough(date!(2026 - 12 - 01), today).is_err(), "текущий");
        assert!(ensure_old_enough(date!(2026 - 11 - 01), today).is_err(), "прошлый");
        assert!(ensure_old_enough(date!(2026 - 10 - 01), today).is_ok());
    }

    #[test]
    fn partition_names() {
        assert_eq!(parse_partition("audit_log_2026_08"), Some(date!(2026 - 08 - 01)));
        assert_eq!(parse_partition("audit_log_2026_13"), None);
        assert_eq!(parse_partition("audit_log_x"), None);
        assert_eq!(parse_partition("audit_log_2026_08; DROP"), None);
    }

    #[test]
    fn archive_paths_cannot_escape() {
        let dir = Path::new("/archive");
        assert!(archive_path(dir, "audit_log-2026-08.jsonl.zst").is_ok());
        assert!(archive_path(dir, "../etc/passwd").is_err());
        assert!(archive_path(dir, "/etc/passwd").is_err());
        assert!(archive_path(dir, "").is_err());
    }
}
