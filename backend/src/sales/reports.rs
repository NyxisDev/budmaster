//! Отчёты владельца (docs/design.md, раздел 6).
//!
//! Период — даты `from` и `to` включительно, по киевскому времени.
//! - Оборот — сумма продаж; возвраты — сумма возвратов; выручка = оборот − возвраты.
//! - Себестоимость продаж = себестоимость проданного − себестоимость возвращённого.
//! - Прибыль = выручка − себестоимость продаж.
//! - Потери — списания по себестоимости на момент списания.
//! - Строки, у которых себестоимость неизвестна (товар не принимали поставкой), в прибыль
//!   не попадают вовсе — ни их выручка, ни себестоимость. Их выручка показана отдельно
//!   (`revenue_unknown_cost`), чтобы прибыль не выглядела больше, чем есть.
//!
//! Каналы: «site» — заказы с сайта; «store» — касса и заказы, внесённые вручную (P5).
//!
//! Месяцы, ушедшие в архив, берутся из сохранённых итогов (`report_monthly_totals`).
//! По ним отчёт возможен только целыми месяцами: период, который режет архивный месяц
//! посередине, отклоняется (`archived_month_partial`).

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::Date;
use time::macros::format_description;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Owner;
use crate::auth::extract::ApiQuery;
use crate::error::{ApiError, ApiResult, FieldErrors};

pub const TIMEZONE: &str = "Europe/Kyiv";
const MAX_DAYS: i64 = 3660;

#[derive(Deserialize)]
pub struct SummaryQuery {
    from: String,
    to: String,
    group_by: Option<String>,
}

fn parse_period(from: &str, to: &str) -> ApiResult<(Date, Date)> {
    let format = format_description!("[year]-[month]-[day]");
    let mut fields = FieldErrors::new();
    let from = Date::parse(from, format).map_err(|_| fields.insert("from", "invalid")).ok();
    let to = Date::parse(to, format).map_err(|_| fields.insert("to", "invalid")).ok();
    if let (Some(from), Some(to)) = (from, to) {
        if from > to {
            fields.insert("from", "after_to");
        } else if (to - from).whole_days() > MAX_DAYS {
            fields.insert("to", "period_too_long");
        }
    }
    match (from, to) {
        (Some(from), Some(to)) if fields.is_empty() => Ok((from, to)),
        _ => Err(ApiError::Validation(fields)),
    }
}

/// Период не должен резать архивный месяц: по нему есть только итог за месяц целиком.
async fn check_archived_months(db: &sqlx::PgPool, from: Date, to: Date) -> ApiResult<()> {
    let partial: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM (SELECT DISTINCT month FROM report_monthly_totals) m
            WHERE m.month <= $2 AND (m.month + interval '1 month')::date > $1
              AND (m.month < $1 OR (m.month + interval '1 month' - interval '1 day')::date > $2))",
    )
    .bind(from)
    .bind(to)
    .fetch_one(db)
    .await?;
    if partial { Err(ApiError::field("from", "archived_month_partial")) } else { Ok(()) }
}

/// Все строки периода: продажи, возвраты, списания. Каждая — с суммой, себестоимостью
/// и ключами для разбивки. `$1`, `$2` — даты периода, `$3` — часовой пояс.
const LINES: &str = "
    WITH RECURSIVE roots AS (
        SELECT id, id AS root FROM categories WHERE parent_id IS NULL
      UNION ALL
        SELECT c.id, r.root FROM categories c JOIN roots r ON c.parent_id = r.id
    ),
    period AS (
        SELECT ($1::date::timestamp AT TIME ZONE $3) AS start,
               (($2::date + 1)::timestamp AT TIME ZONE $3) AS finish
    ),
    lines AS (
        -- cost NULL — себестоимость неизвестна; n_unknown — сколько таких строк
        SELECT 'sale' AS kind, s.channel::text AS channel, s.warehouse_id, i.product_id,
               round(i.qty * i.price, 2) AS amount, i.qty * i.unit_cost AS cost, (i.unit_cost IS NULL)::int AS n_unknown,
               CASE WHEN i.unit_cost IS NULL THEN round(i.qty * i.price, 2) ELSE 0 END AS amount_unknown
        FROM sales s JOIN sale_items i ON i.sale_id = s.id CROSS JOIN period
        WHERE s.occurred_at >= period.start AND s.occurred_at < period.finish
      UNION ALL
        SELECT 'return', r.channel::text, r.warehouse_id, i.product_id,
               round(i.qty * i.price, 2), i.qty * i.unit_cost, (i.unit_cost IS NULL)::int,
               CASE WHEN i.unit_cost IS NULL THEN round(i.qty * i.price, 2) ELSE 0 END
        FROM returns r JOIN return_items i ON i.return_id = r.id CROSS JOIN period
        WHERE r.occurred_at >= period.start AND r.occurred_at < period.finish
      UNION ALL
        SELECT 'loss', NULL, m.warehouse_id, m.product_id, 0, -m.qty * m.unit_cost, (m.unit_cost IS NULL)::int, 0
        FROM stock_movements m CROSS JOIN period
        WHERE m.kind = 'write_off' AND m.occurred_at >= period.start AND m.occurred_at < period.finish
      -- архивные месяцы, целиком входящие в период
      UNION ALL
        SELECT 'sale', t.channel::text, t.warehouse_id, t.product_id, t.turnover, t.sale_cost, t.unknown_cost_lines,
               t.turnover_unknown_cost
        FROM report_monthly_totals t WHERE t.channel IS NOT NULL AND t.month >= $1::date AND t.month <= $2::date
      UNION ALL
        SELECT 'return', t.channel::text, t.warehouse_id, t.product_id, t.returns, t.return_cost, 0,
               t.returns_unknown_cost
        FROM report_monthly_totals t WHERE t.channel IS NOT NULL AND t.month >= $1::date AND t.month <= $2::date
      UNION ALL
        SELECT 'loss', NULL, t.warehouse_id, t.product_id, 0, t.losses, t.unknown_loss_lines, 0
        FROM report_monthly_totals t WHERE t.channel IS NULL AND t.month >= $1::date AND t.month <= $2::date
    ),
    keyed AS (
        SELECT l.*, p.name AS product_name, rt.root AS root_category,
               CASE WHEN l.channel IS NULL THEN NULL WHEN l.channel = 'site' THEN 'site' ELSE 'store' END AS channel_group
        FROM lines l JOIN products p ON p.id = l.product_id LEFT JOIN roots rt ON rt.id = p.category_id
    )";

/// Показатели группы строк — одинаково для итога, групп и товаров.
const METRICS: &str = "
    COALESCE(sum(amount) FILTER (WHERE kind = 'sale'), 0) AS turnover,
    COALESCE(sum(amount) FILTER (WHERE kind = 'return'), 0) AS returns,
    COALESCE(sum(cost) FILTER (WHERE kind = 'sale'), 0) - COALESCE(sum(cost) FILTER (WHERE kind = 'return'), 0) AS cost,
    COALESCE(sum(cost) FILTER (WHERE kind = 'loss'), 0) AS losses,
    COALESCE(sum(n_unknown) FILTER (WHERE kind <> 'loss'), 0) AS unknown_cost_lines,
    COALESCE(sum(n_unknown) FILTER (WHERE kind = 'loss'), 0) AS unknown_loss_lines,
    COALESCE(sum(amount_unknown) FILTER (WHERE kind = 'sale'), 0)
      - COALESCE(sum(amount_unknown) FILTER (WHERE kind = 'return'), 0) AS unknown_amount";

/// Превращает сырые суммы в итоговые показатели (деньги — строками с копейками).
const FINISH: &str = "
    turnover::numeric(14,2)::text AS turnover,
    returns::numeric(14,2)::text AS returns,
    (turnover - returns)::numeric(14,2)::text AS revenue,
    round(cost, 2)::numeric(14,2)::text AS cost_of_sales,
    unknown_amount::numeric(14,2)::text AS revenue_unknown_cost,
    (turnover - returns - unknown_amount - round(cost, 2))::numeric(14,2)::text AS profit,
    round(losses, 2)::numeric(14,2)::text AS losses,
    unknown_cost_lines, unknown_loss_lines";

#[derive(Serialize, sqlx::FromRow)]
struct Metrics {
    turnover: String,
    returns: String,
    revenue: String,
    cost_of_sales: String,
    /// Выручка строк с неизвестной себестоимостью: в прибыль не входит.
    revenue_unknown_cost: String,
    profit: String,
    losses: String,
    /// Строк продаж и возвратов, где себестоимость неизвестна: прибыль по ним не посчитана.
    unknown_cost_lines: i64,
    unknown_loss_lines: i64,
}

#[derive(sqlx::FromRow)]
struct GroupRow {
    key: Option<String>,
    #[sqlx(flatten)]
    metrics: Metrics,
}

/// `GET /reports/summary?from=2026-11-01&to=2026-11-30&group_by=channel|warehouse|category`
pub async fn summary(
    State(state): State<AppState>,
    _owner: Owner,
    ApiQuery(query): ApiQuery<SummaryQuery>,
) -> ApiResult<Json<Value>> {
    let (from, to) = parse_period(&query.from, &query.to)?;
    check_archived_months(&state.db, from, to).await?;
    let group_by = query.group_by.as_deref().unwrap_or("channel");
    let key = match group_by {
        "channel" => "channel_group",
        "warehouse" => "warehouse_id::text",
        "category" => "root_category::text",
        _ => return Err(ApiError::field("group_by", "invalid")),
    };

    let rows: Vec<GroupRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{LINES}
         SELECT key, {FINISH} FROM (
            SELECT {key} AS key, GROUPING({key}) AS is_total, {METRICS}
            FROM keyed GROUP BY GROUPING SETS (({key}), ())
         ) g
         -- строки без ключа (списания при разбивке по каналу) входят только в итог
         WHERE is_total = 1 OR key IS NOT NULL
         ORDER BY is_total DESC, key"
    )))
    .bind(from)
    .bind(to)
    .bind(TIMEZONE)
    .fetch_all(&state.db)
    .await?;

    let names: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
        "SELECT id::text, name FROM warehouses UNION ALL SELECT id::text, name FROM categories",
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let mut rows = rows.into_iter();
    let totals = rows.next().map(|row| row.metrics);
    let groups: Vec<Value> = rows
        .map(|row| {
            let key = row.key.unwrap_or_default();
            let name = match key.as_str() {
                "site" => "Сайт".to_owned(),
                "store" => "Каса".to_owned(),
                other => names.get(other).cloned().unwrap_or_default(),
            };
            let mut value = serde_json::to_value(row.metrics).unwrap_or_default();
            value["key"] = json!(key);
            value["name"] = json!(name);
            value
        })
        .collect();

    Ok(Json(json!({
        "from": from.to_string(),
        "to": to.to_string(),
        "timezone": TIMEZONE,
        "group_by": group_by,
        "totals": totals,
        "groups": groups,
    })))
}

#[derive(Deserialize)]
pub struct ProductsQuery {
    from: String,
    to: String,
    page: Option<i64>,
    per_page: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct ProductRow {
    product_id: Uuid,
    product_name: String,
    #[sqlx(flatten)]
    #[serde(flatten)]
    metrics: Metrics,
}

/// `GET /reports/products?from=&to=&page=` — те же показатели по товарам, по убыванию выручки.
pub async fn products(
    State(state): State<AppState>,
    _owner: Owner,
    ApiQuery(query): ApiQuery<ProductsQuery>,
) -> ApiResult<Json<Value>> {
    let (from, to) = parse_period(&query.from, &query.to)?;
    check_archived_months(&state.db, from, to).await?;
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(50);
    if !(1..=10_000).contains(&page) || !(1..=200).contains(&per_page) {
        return Err(ApiError::field("page", "invalid"));
    }
    let items: Vec<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{LINES}
         SELECT product_id, product_name, {FINISH} FROM (
            SELECT product_id, product_name, {METRICS} FROM keyed GROUP BY product_id, product_name
         ) g
         ORDER BY turnover - returns DESC, product_name, product_id
         LIMIT $4 OFFSET $5"
    )))
    .bind(from)
    .bind(to)
    .bind(TIMEZONE)
    .bind(per_page)
    .bind((page - 1) * per_page)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "from": from.to_string(), "to": to.to_string(), "items": items, "page": page, "per_page": per_page })))
}
