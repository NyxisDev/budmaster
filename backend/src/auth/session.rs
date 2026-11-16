//! Сессии устройств: выдача, обновление и отзыв refresh-токенов.
//!
//! Сессия — это цепочка refresh-токенов с общим `family_id`. Её номер записан в каждом
//! access-токене (`sid`), и сервер при каждом запросе проверяет, что сессия жива.
//! Поэтому выход, блокировка, смена и сброс пароля действуют сразу, а новый вход на том же
//! устройстве не «оживляет» старые токены: у него другая сессия.
//!
//! Порядок блокировок везде один — сначала строка сотрудника, потом токены. Так обновление
//! токена и смена пароля, идущие одновременно, выполняются по очереди и не мешают друг другу.

use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::tokens::{self, JwtKeys};
use crate::audit::{self, Actor, Entry};
use crate::error::{ApiError, ApiResult};

/// Если ответ на обновление токена потерялся в плохой сети, телефон повторит запрос
/// со старым токеном. В течение этого времени повтор считается нормальным, позже —
/// признаком кражи токена.
const REUSE_GRACE: Duration = Duration::seconds(60);

#[derive(Debug, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    /// Через сколько секунд истечёт access-токен.
    pub expires_in: i64,
}

/// Открывает новую сессию (вход, смена пароля).
pub async fn start(
    conn: &mut PgConnection,
    jwt: &JwtKeys,
    refresh_ttl_days: i64,
    user_id: Uuid,
    device_id: Uuid,
) -> ApiResult<TokenPair> {
    issue(conn, jwt, refresh_ttl_days, user_id, device_id, Uuid::new_v4()).await
}

async fn issue(
    conn: &mut PgConnection,
    jwt: &JwtKeys,
    refresh_ttl_days: i64,
    user_id: Uuid,
    device_id: Uuid,
    family_id: Uuid,
) -> ApiResult<TokenPair> {
    let refresh_token = tokens::new_refresh_token();
    sqlx::query(
        "INSERT INTO refresh_tokens (id, user_id, device_id, family_id, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(device_id)
    .bind(family_id)
    .bind(tokens::hash_refresh_token(&refresh_token))
    .bind(OffsetDateTime::now_utc() + Duration::days(refresh_ttl_days))
    .execute(&mut *conn)
    .await?;

    Ok(TokenPair {
        access_token: jwt.issue(user_id, device_id, family_id)?,
        refresh_token,
        expires_in: jwt.ttl_secs(),
    })
}

#[derive(sqlx::FromRow)]
struct RefreshRow {
    id: Uuid,
    device_id: Uuid,
    family_id: Uuid,
    expires_at: OffsetDateTime,
    used_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
}

/// Меняет refresh-токен на новую пару. Старый токен после этого «использован».
pub async fn rotate(
    db: &PgPool,
    jwt: &JwtKeys,
    refresh_ttl_days: i64,
    presented_token: &str,
    device_id: Uuid,
) -> ApiResult<TokenPair> {
    let token_hash = tokens::hash_refresh_token(presented_token);
    let mut tx = db.begin().await?;

    // Шаг 1: чей это токен. Затем блокируем строку сотрудника — смена пароля и блокировка
    // делают то же самое первым действием, поэтому либо они уже завершились и токен отозван,
    // либо подождут, пока мы закончим, и отзовут в том числе только что выданный токен.
    let user_id: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM refresh_tokens WHERE token_hash = $1")
        .bind(&token_hash)
        .fetch_optional(&mut *tx)
        .await?;
    let user_id = user_id.ok_or(ApiError::Unauthorized("invalid_refresh_token"))?;

    let user: Option<(bool, String)> =
        sqlx::query_as("SELECT is_active, first_name || ' ' || last_name FROM users WHERE id = $1 FOR SHARE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (user_active, full_name) = user.ok_or(ApiError::Unauthorized("invalid_refresh_token"))?;
    if !user_active {
        return Err(ApiError::Unauthorized("user_blocked"));
    }

    // Шаг 2: сам токен, под блокировкой: два одновременных запроса с одним токеном
    // обрабатываются по очереди, и второй видит, что токен уже использован.
    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT id, device_id, family_id, expires_at, used_at, revoked_at
         FROM refresh_tokens WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(&token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let row = row.ok_or(ApiError::Unauthorized("invalid_refresh_token"))?;
    let now = OffsetDateTime::now_utc();

    if row.expires_at <= now {
        return Err(ApiError::Unauthorized("invalid_refresh_token"));
    }

    // Признаки того, что токен скопирован: предъявлен с другого устройства, предъявлен
    // отозванным (его уже заменили при повторе) или использован давно.
    let stolen = row.device_id != device_id
        || row.revoked_at.is_some()
        || row.used_at.is_some_and(|used_at| now - used_at > REUSE_GRACE);
    if stolen {
        // Закрываем всю сессию: и настоящему владельцу, и вору придётся войти по паролю.
        // Вместе с сессией перестают действовать и её access-токены.
        let newly_revoked = revoke_family(&mut tx, row.family_id).await?;
        // В журнал пишем только когда действительно что-то закрыли: повторное предъявление
        // токена из уже закрытой сессии (после выхода, смены пароля) — не новость.
        if newly_revoked > 0 {
            let actor = Actor { user_id: Some(user_id), full_name, device_id: Some(device_id) };
            audit::record(
                &mut *tx,
                Entry::new(&actor, "auth.token_reuse")
                    .entity("user", user_id)
                    .details(serde_json::json!({ "token_device_id": row.device_id })),
            )
            .await?;
        }
        tx.commit().await?;
        let code = if newly_revoked > 0 { "refresh_token_reused" } else { "invalid_refresh_token" };
        return Err(ApiError::Unauthorized(code));
    }

    if row.used_at.is_some() {
        // Быстрый повтор: прошлый ответ не дошёл до телефона. Токен, выданный в тот раз,
        // отзываем — у сессии всегда ровно один действующий refresh-токен, без ответвлений.
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = $2
             WHERE family_id = $1 AND used_at IS NULL AND revoked_at IS NULL",
        )
        .bind(row.family_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("UPDATE refresh_tokens SET used_at = $2 WHERE id = $1")
            .bind(row.id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }

    let pair = issue(&mut tx, jwt, refresh_ttl_days, user_id, row.device_id, row.family_id).await?;
    tx.commit().await?;
    Ok(pair)
}

async fn revoke_family(conn: &mut PgConnection, family_id: Uuid) -> sqlx::Result<u64> {
    let result =
        sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE family_id = $1 AND revoked_at IS NULL")
            .bind(family_id)
            .execute(conn)
            .await?;
    Ok(result.rows_affected())
}

/// Закрывает одну сессию (выход).
pub async fn revoke_session(conn: &mut PgConnection, session_id: Uuid) -> sqlx::Result<()> {
    revoke_family(conn, session_id).await.map(|_| ())
}

/// Закрывает все сессии сотрудника (блокировка, смена и сброс пароля).
/// Вызывающий код обязан перед этим заблокировать строку сотрудника (UPDATE или FOR UPDATE).
pub async fn revoke_all_for_user(conn: &mut PgConnection, user_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL")
        .bind(user_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Закрывает сессии на одном устройстве (на нём входит другой сотрудник).
pub async fn revoke_for_device(conn: &mut PgConnection, device_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE device_id = $1 AND revoked_at IS NULL")
        .bind(device_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Удаляет токены, истёкшие больше 30 дней назад. Вызывается раз в сутки.
pub async fn purge_expired(db: &PgPool) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM refresh_tokens WHERE expires_at < now() - interval '30 days'")
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}
