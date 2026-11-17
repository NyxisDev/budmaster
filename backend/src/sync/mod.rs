//! Синхронизация с приложением (docs/design.md, раздел 9).
//!
//! Все изменения с устройства приходят пачкой операций в `POST /sync/push`. Каждая операция
//! обрабатывается в своей транзакции:
//! 1. берётся общая блокировка изменений (та же, что у счётчика `change_seq`) — всегда первой,
//!    до блокировок строк. Так же поступают все пишущие пути сервера (сотрудники, фото),
//!    поэтому взаимных ожиданий не возникает;
//! 2. права сотрудника перечитываются из базы — отозванное право действует с ближайшей операции;
//! 3. `op_id` записывается в реестр в той же транзакции, что и само действие: повтор той же
//!    операции (обрыв связи, повторная отправка) находит запись и ничего не делает;
//! 4. действие выполняется внутри точки сохранения: если его надо отклонить, откатывается
//!    только оно, а запись об отказе в реестре и журнале остаётся.
//!
//! Временный сбой (нет связи с базой, перегрузка) не превращается в окончательный отказ:
//! обработка пачки останавливается, и эта и все следующие операции возвращаются со статусом
//! `retry`. Любая другая ошибка — окончательный отказ этой одной операции: иначе одна
//! негодная операция навсегда остановила бы очередь устройства.

pub mod ops;
pub mod pull;
pub mod reference;
pub mod stock;
pub mod values;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::AppState;
use crate::audit::{self, Actor, Entry};
use crate::auth::extract::ApiJson;
use crate::auth::{ActiveUser, Role, tokens};
use crate::error::{ApiError, ApiResult};
use crate::permissions;

/// Сколько операций можно прислать за раз.
pub const MAX_OPS_PER_BATCH: usize = 200;
/// Предел тела запроса синхронизации.
pub const MAX_PUSH_BYTES: usize = 2 * 1024 * 1024;
/// Операции старше этого срока не принимаются: их уже нельзя надёжно отличить от повтора.
const MAX_OP_AGE: Duration = Duration::days(90);
/// Расхождение часов, о котором стоит предупредить в журнале.
const CLOCK_SKEW_WARNING: Duration = Duration::minutes(5);

/// Ключ общей блокировки изменений. Тот же ключ берёт триггер `set_change_seq`.
const CHANGES_LOCK: i64 = 727001;

/// Берёт общую блокировку изменений до конца транзакции. Вызывать первой, до любых
/// блокировок строк: тогда все пишущие транзакции блокируют в одном порядке.
pub async fn lock_changes(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(CHANGES_LOCK).execute(conn).await?;
    Ok(())
}

/// Сотрудник, от имени которого выполняются операции.
#[derive(Clone, Debug)]
pub struct SyncUser {
    pub id: Uuid,
    pub full_name: String,
    pub role: Role,
    pub permissions: i64,
    pub device_id: Uuid,
}

impl SyncUser {
    pub fn can(&self, permission: i64) -> bool {
        self.role == Role::Owner || permissions::has(self.permissions, permission)
    }

    pub fn is_owner(&self) -> bool {
        self.role == Role::Owner
    }

    pub fn actor(&self) -> Actor {
        Actor { user_id: Some(self.id), full_name: self.full_name.clone(), device_id: Some(self.device_id) }
    }
}

/// Всё, что нужно действию: кто, какая операция, когда (время после поправок).
pub struct OpCtx<'a> {
    pub user: &'a SyncUser,
    pub op_id: Uuid,
    pub at: OffsetDateTime,
}

impl OpCtx<'_> {
    /// Запись в журнал со временем операции, а не временем приёма.
    pub async fn audit(
        &self,
        conn: &mut PgConnection,
        action: &'static str,
        entity: Option<(&'static str, Uuid)>,
        warehouse_id: Option<Uuid>,
        details: Value,
    ) -> sqlx::Result<()> {
        let actor = self.user.actor();
        let mut entry = Entry::new(&actor, action).details(details).at(self.at);
        if let Some((entity_type, entity_id)) = entity {
            entry = entry.entity(entity_type, entity_id);
        }
        entry.warehouse_id = warehouse_id;
        audit::record(conn, entry).await
    }
}

/// Итог действия.
#[derive(Debug)]
pub enum OpError {
    /// Операция отклонена окончательно; код уходит на устройство.
    Rejected(&'static str),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for OpError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error)
    }
}

pub type OpResult = Result<(), OpError>;

#[derive(Deserialize)]
pub struct OpEnvelope {
    op_id: Uuid,
    #[serde(rename = "type")]
    op_type: String,
    /// Время действия по часам устройства.
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
pub struct PushRequest {
    /// Текущее время на устройстве — по нему сервер вычисляет расхождение часов.
    #[serde(with = "time::serde::rfc3339")]
    device_time: OffsetDateTime,
    /// Устройство отправило всю свою очередь. Тогда операции, созданные позже, не могут
    /// оказаться старше этого момента — это нижняя граница их времени.
    #[serde(default)]
    queue_drained: bool,
    ops: Vec<OpEnvelope>,
}

#[derive(Serialize)]
pub struct OpOutcome {
    op_id: Uuid,
    /// applied | duplicate | rejected | retry
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

impl OpOutcome {
    fn new(op_id: Uuid, status: &'static str, code: Option<&str>) -> Self {
        Self { op_id, status, code: code.map(str::to_owned) }
    }
}

#[derive(Serialize)]
pub struct PushResponse {
    results: Vec<OpOutcome>,
    #[serde(with = "time::serde::rfc3339")]
    server_time: OffsetDateTime,
}

impl PushResponse {
    fn has_retry(&self) -> bool {
        self.results.iter().any(|result| result.status == "retry")
    }
}

/// `POST /sync/push`
pub async fn push(
    State(state): State<AppState>,
    ActiveUser(user): ActiveUser,
    ApiJson(request): ApiJson<PushRequest>,
) -> ApiResult<Json<PushResponse>> {
    Ok(Json(run_batch(&state.db, user.id, user.device_id, request).await?))
}

#[derive(Deserialize)]
pub struct DrainRequest {
    refresh_token: String,
    device_id: Uuid,
    #[serde(flatten)]
    push: PushRequest,
}

/// `POST /sync/drain` — досылка очереди с устройства заблокированного сотрудника.
///
/// Блокировка закрывает все сессии, но на телефоне могли остаться операции, сделанные до неё
/// (приёмки, списания — то, что реально произошло). Устройство предъявляет свой последний
/// refresh-токен — тот, что был действующим в момент блокировки, — и отправляет очередь.
/// Принимаются только операции, сделанные до блокировки.
///
/// Длинную очередь можно отправить несколькими запросами. Устройство отключается, когда
/// пришёл запрос с `queue_drained: true` и всё обработано; после этого досылка невозможна.
/// Пока один запрос досылки обрабатывается, второй с того же устройства не принимается.
pub async fn drain(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<DrainRequest>,
) -> ApiResult<Json<PushResponse>> {
    if request.refresh_token.len() > 128 {
        return Err(ApiError::Unauthorized("invalid_refresh_token"));
    }

    // Годится только последний токен цепочки (ещё не обменянный на новый), отозванный
    // именно блокировкой, а не как украденный задолго до неё.
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT t.user_id
         FROM refresh_tokens t
         JOIN users u ON u.id = t.user_id AND NOT u.is_active
         JOIN user_blocks b ON b.user_id = u.id AND b.unblocked_at IS NULL
         WHERE t.token_hash = $1 AND t.device_id = $2 AND t.expires_at > now()
           AND t.used_at IS NULL
           AND (t.revoked_at IS NULL OR t.revoked_at >= b.blocked_at)",
    )
    .bind(tokens::hash_refresh_token(&request.refresh_token))
    .bind(request.device_id)
    .fetch_optional(&state.db)
    .await?;
    let user_id = user_id.ok_or(ApiError::Unauthorized("invalid_refresh_token"))?;

    // Занимаем устройство: пока идёт эта досылка, вторая с него же не пройдёт.
    let claimed = sqlx::query(
        "UPDATE devices SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(request.device_id)
    .bind(user_id)
    .execute(&state.db)
    .await?;
    if claimed.rows_affected() == 0 {
        return Err(ApiError::Unauthorized("invalid_refresh_token"));
    }

    let queue_drained = request.push.queue_drained;
    let response = run_batch(&state.db, user_id, request.device_id, request.push).await;

    let finished = matches!(&response, Ok(response) if queue_drained && !response.has_retry());
    let mut tx = state.db.begin().await?;
    if finished {
        let full_name: String = sqlx::query_scalar("SELECT first_name || ' ' || last_name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
        let actor = Actor { user_id: Some(user_id), full_name, device_id: Some(request.device_id) };
        audit::record(&mut *tx, Entry::new(&actor, "sync.drain").entity("user", user_id)).await?;
    } else {
        // Очередь ещё не вся или был временный сбой: устройство сможет продолжить досылку.
        sqlx::query("UPDATE devices SET revoked_at = NULL WHERE id = $1").bind(request.device_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Json(response?))
}

async fn run_batch(db: &PgPool, user_id: Uuid, device_id: Uuid, request: PushRequest) -> ApiResult<PushResponse> {
    if request.ops.len() > MAX_OPS_PER_BATCH {
        return Err(ApiError::field("ops", "too_many"));
    }
    let server_now = OffsetDateTime::now_utc();
    let offset = server_now - request.device_time;

    // Нижняя граница времени операций этого устройства: не раньше первого входа этого
    // сотрудника на нём (до входа операций быть не могло) и не раньше последней полной отправки.
    let floor: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT GREATEST(
            (SELECT sync_floor FROM devices WHERE id = $2),
            (SELECT min(created_at) FROM refresh_tokens WHERE user_id = $1 AND device_id = $2))",
    )
    .bind(user_id)
    .bind(device_id)
    .fetch_one(db)
    .await?;

    if offset.abs() > CLOCK_SKEW_WARNING {
        let full_name: String = sqlx::query_scalar("SELECT first_name || ' ' || last_name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(db)
            .await?;
        let actor = Actor { user_id: Some(user_id), full_name, device_id: Some(device_id) };
        audit::record(
            db,
            Entry::new(&actor, "sync.clock_skew").details(json!({ "offset_seconds": offset.whole_seconds() })),
        )
        .await?;
    }

    let times = Times { server_now, offset, floor };
    let mut results = Vec::with_capacity(request.ops.len());
    let mut stopped = false;
    for op in request.ops {
        if stopped {
            results.push(OpOutcome::new(op.op_id, "retry", None));
            continue;
        }
        let outcome = process_op(db, user_id, device_id, &op, &times).await;
        stopped = outcome.status == "retry";
        results.push(outcome);
    }

    // Нижняя граница сдвигается, только когда устройство отправило всю очередь и всё принято:
    // иначе операции из следующей части очереди «прилипли» бы к этому моменту.
    sqlx::query(
        "UPDATE devices SET last_sync_at = now(),
                sync_floor = CASE WHEN $2 THEN GREATEST(COALESCE(sync_floor, $3), $3) ELSE sync_floor END
         WHERE id = $1",
    )
    .bind(device_id)
    .bind(request.queue_drained && !stopped)
    .bind(server_now)
    .execute(db)
    .await?;

    Ok(PushResponse { results, server_time: OffsetDateTime::now_utc() })
}

struct Times {
    server_now: OffsetDateTime,
    offset: Duration,
    floor: Option<OffsetDateTime>,
}

impl Times {
    /// Время по часам устройства с поправкой на их расхождение с сервером.
    fn corrected(&self, occurred_at: OffsetDateTime) -> OffsetDateTime {
        occurred_at.checked_add(self.offset).unwrap_or(self.server_now)
    }

    /// Время, по которому проводится операция: исправленное, но не раньше нижней границы
    /// устройства и не позже момента приёма.
    fn effective(&self, occurred_at: OffsetDateTime) -> OffsetDateTime {
        clamp(self.corrected(occurred_at), self.floor, self.server_now)
    }
}

fn clamp(time: OffsetDateTime, floor: Option<OffsetDateTime>, ceiling: OffsetDateTime) -> OffsetDateTime {
    let time = match floor {
        Some(floor) if time < floor => floor,
        _ => time,
    };
    time.min(ceiling)
}

/// Тип операции: латиница, цифры, точка и подчёркивание, до 64 символов.
fn valid_type(op_type: &str) -> bool {
    (1..=64).contains(&op_type.len())
        && op_type.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_'))
}

async fn process_op(db: &PgPool, user_id: Uuid, device_id: Uuid, op: &OpEnvelope, times: &Times) -> OpOutcome {
    if !valid_type(&op.op_type) {
        return OpOutcome::new(op.op_id, "rejected", Some("unknown_type"));
    }
    match process_op_inner(db, user_id, device_id, op, times).await {
        Ok(outcome) => outcome,
        Err(error) if is_transient(&error) => {
            tracing::warn!(?error, op_id = %op.op_id, "временный сбой, устройство повторит");
            OpOutcome::new(op.op_id, "retry", None)
        }
        Err(error) => {
            // Ошибка, которая повторится и при следующей попытке. Отклоняем эту операцию,
            // чтобы она не держала всю очередь; запись в реестр — чтобы повтор дал тот же ответ.
            tracing::error!(?error, op_id = %op.op_id, op_type = %op.op_type, "операция не обработана");
            let recorded = record_failure(db, user_id, device_id, op, times).await;
            if let Err(error) = recorded {
                if is_transient(&error) {
                    return OpOutcome::new(op.op_id, "retry", None);
                }
                tracing::error!(?error, op_id = %op.op_id, "не удалось записать отказ в реестр");
            }
            OpOutcome::new(op.op_id, "rejected", Some("server_error"))
        }
    }
}

/// Записывает в реестр окончательный отказ из-за ошибки сервера.
async fn record_failure(
    db: &PgPool,
    user_id: Uuid,
    device_id: Uuid,
    op: &OpEnvelope,
    times: &Times,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO sync_operations
            (op_id, user_id, device_id, op_type, result, reject_code, clock_offset_ms, occurred_at, effective_at)
         VALUES ($1, $2, $3, $4, 'rejected', 'server_error', $5, $6, $7)
         ON CONFLICT (op_id) DO NOTHING",
    )
    .bind(op.op_id)
    .bind(user_id)
    .bind(device_id)
    .bind(&op.op_type)
    .bind(i64::try_from(times.offset.whole_milliseconds()).unwrap_or(i64::MAX))
    .bind(op.occurred_at)
    .bind(times.effective(op.occurred_at))
    .execute(db)
    .await?;
    Ok(())
}

/// Временный ли сбой: связь с базой, перегрузка, взаимная блокировка — повтор может помочь.
fn is_transient(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(db) => db.code().is_some_and(|code| {
            // 08 — соединение, 40 — сериализация и взаимная блокировка, 53 — нехватка ресурсов,
            // 57 — остановка сервера, 58 — системная ошибка.
            ["08", "40", "53", "57", "58"].iter().any(|class| code.starts_with(class))
        }),
        _ => false,
    }
}

async fn process_op_inner(
    db: &PgPool,
    user_id: Uuid,
    device_id: Uuid,
    op: &OpEnvelope,
    times: &Times,
) -> sqlx::Result<OpOutcome> {
    let corrected = times.corrected(op.occurred_at);
    let at = times.effective(op.occurred_at);
    let mut tx = db.begin().await?;
    lock_changes(&mut tx).await?;

    // Права и имя — из базы на момент этой операции.
    let (full_name, role, permissions): (String, Role, i64) =
        sqlx::query_as("SELECT first_name || ' ' || last_name, role, permissions FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    let user = SyncUser { id: user_id, full_name, role, permissions, device_id };

    // Запись в реестре. Если она уже есть — это повтор: возвращаем прежний итог.
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO sync_operations (op_id, user_id, device_id, op_type, result, clock_offset_ms, occurred_at, effective_at)
         VALUES ($1, $2, $3, $4, 'applied', $5, $6, $7)
         ON CONFLICT (op_id) DO NOTHING
         RETURNING op_id",
    )
    .bind(op.op_id)
    .bind(user_id)
    .bind(device_id)
    .bind(&op.op_type)
    .bind(i64::try_from(times.offset.whole_milliseconds()).unwrap_or(i64::MAX))
    .bind(op.occurred_at)
    .bind(at)
    .fetch_optional(&mut *tx)
    .await?;

    if claimed.is_none() {
        let previous: (Uuid, String, Option<String>) =
            sqlx::query_as("SELECT user_id, result, reject_code FROM sync_operations WHERE op_id = $1")
                .bind(op.op_id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        // Чужой op_id — не повтор, а совпадение номеров; такую операцию не выполняем.
        if previous.0 != user_id {
            return Ok(OpOutcome::new(op.op_id, "rejected", Some("op_id_conflict")));
        }
        return Ok(match previous.1.as_str() {
            "applied" => OpOutcome::new(op.op_id, "duplicate", None),
            _ => OpOutcome::new(op.op_id, "rejected", previous.2.as_deref()),
        });
    }

    let ctx = OpCtx { user: &user, op_id: op.op_id, at };
    // Давность проверяется по исправленному времени до подтягивания к нижней границе:
    // иначе граница «омолодила» бы операцию, которую уже нельзя отличить от повтора.
    let verdict = if corrected < times.server_now - MAX_OP_AGE {
        Err(OpError::Rejected("too_old"))
    } else if in_block_period(&mut tx, user_id, at).await? {
        Err(OpError::Rejected("user_blocked"))
    } else {
        let mut savepoint = sqlx::Acquire::begin(&mut *tx).await?;
        let result = ops::apply(&mut savepoint, &ctx, &op.op_type, &op.payload).await;
        match result {
            Ok(()) => {
                savepoint.commit().await?;
                Ok(())
            }
            Err(error) => {
                savepoint.rollback().await?;
                Err(error)
            }
        }
    };

    let code = match verdict {
        Ok(()) => None,
        Err(OpError::Rejected(code)) => Some(code),
        Err(OpError::Db(error)) => match permanent_code(&error) {
            Some(code) => Some(code),
            None => return Err(error),
        },
    };

    if let Some(code) = code {
        sqlx::query("UPDATE sync_operations SET result = 'rejected', reject_code = $2 WHERE op_id = $1")
            .bind(op.op_id)
            .bind(code)
            .execute(&mut *tx)
            .await?;
        ctx.audit(&mut tx, "sync.rejected", None, None, json!({ "op_id": op.op_id, "type": op.op_type, "code": code }))
            .await?;
    } else if at != corrected {
        // Время операции пришлось подтянуть к допустимым границам — это видно в журнале.
        ctx.audit(
            &mut tx,
            "sync.time_adjusted",
            None,
            None,
            json!({ "op_id": op.op_id, "device_time": op.occurred_at.to_string(), "corrected": corrected.to_string() }),
        )
        .await?;
    }
    tx.commit().await?;

    Ok(match code {
        None => OpOutcome::new(op.op_id, "applied", None),
        Some(code) => OpOutcome::new(op.op_id, "rejected", Some(code)),
    })
}

/// Ошибка базы, которая означает негодные данные операции, а не временный сбой.
fn permanent_code(error: &sqlx::Error) -> Option<&'static str> {
    let code = error.as_database_error()?.code()?;
    match code.as_ref() {
        // Запись с таким id уже есть.
        "23505" => Some("already_exists"),
        // 22 — ошибка данных (переполнение числа и т. п.), 23 — прочие нарушения ограничений.
        code if code.starts_with("22") || code.starts_with("23") => Some("invalid_data"),
        _ => None,
    }
}

/// Попадает ли момент в период, когда сотрудник был заблокирован.
async fn in_block_period(conn: &mut PgConnection, user_id: Uuid, at: OffsetDateTime) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM user_blocks
            WHERE user_id = $1 AND blocked_at <= $2 AND (unblocked_at IS NULL OR unblocked_at > $2)
        )",
    )
    .bind(user_id)
    .bind(at)
    .fetch_one(conn)
    .await
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn effective_time_corrects_and_clamps() {
        let now = datetime!(2026-11-17 12:00 UTC);
        let times = |offset, floor| Times { server_now: now, offset, floor };
        // Часы устройства отстают на час: время сдвигается вперёд.
        assert_eq!(
            times(Duration::hours(1), None).effective(datetime!(2026-11-17 10:30 UTC)),
            datetime!(2026-11-17 11:30 UTC)
        );
        // Из будущего — не позже момента приёма.
        assert_eq!(times(Duration::ZERO, None).effective(datetime!(2026-11-18 10:00 UTC)), now);
        // Раньше нижней границы устройства — подтягивается к ней.
        let floor = datetime!(2026-11-17 09:00 UTC);
        assert_eq!(times(Duration::ZERO, Some(floor)).effective(datetime!(2026-01-01 00:00 UTC)), floor);
    }

    #[test]
    fn operation_types() {
        assert!(valid_type("stock.write_off"));
        assert!(!valid_type(""));
        assert!(!valid_type("x\u{0}"));
        assert!(!valid_type("Stock.Write"));
        assert!(!valid_type(&"a".repeat(65)));
    }
}
