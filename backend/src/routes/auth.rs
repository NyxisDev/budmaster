//! Вход, обновление токенов, выход, смена пароля, профиль.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::audit::{self, Actor, Entry};
use crate::auth::extract::{ApiJson, clean_text};
use crate::auth::session::{self, TokenPair};
use crate::auth::{AuthUser, ClientIp, Role, password};
use crate::error::{ApiError, ApiResult, FieldErrors};

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Profile {
    pub id: Uuid,
    pub login: String,
    pub first_name: String,
    pub last_name: String,
    pub role: Role,
    pub permissions: i64,
    pub default_warehouse_id: Option<Uuid>,
    pub must_change_password: bool,
}

#[derive(Serialize)]
pub struct SessionResponse {
    #[serde(flatten)]
    tokens: TokenPair,
    user: Profile,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    login: String,
    password: String,
    device_id: Uuid,
    device_model: Option<String>,
}

#[derive(sqlx::FromRow)]
struct LoginRow {
    id: Uuid,
    password_hash: String,
    is_active: bool,
    full_name: String,
}

pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    ApiJson(body): ApiJson<LoginRequest>,
) -> ApiResult<Json<SessionResponse>> {
    let login = body.login.trim().to_lowercase();
    // Заведомо негодные данные отсекаем до базы и до тяжёлого хэширования.
    if login.is_empty()
        || login.chars().count() > 64
        || login.chars().any(char::is_control)
        || body.password.chars().count() > password::MAX_LEN
    {
        return Err(ApiError::Unauthorized("invalid_credentials"));
    }
    let device_model = body.device_model.map(|model| clean_text(&model, 100)).filter(|model| !model.is_empty());

    // Попытка засчитывается сразу: одновременные запросы не проскочат лимит все разом.
    state
        .login_limiter
        .begin(&login, ip)
        .map_err(|retry_after_secs| ApiError::RateLimited { retry_after_secs })?;

    let user: Option<LoginRow> = sqlx::query_as(
        "SELECT id, password_hash, is_active, first_name || ' ' || last_name AS full_name
         FROM users WHERE login = $1::citext",
    )
    .bind(&login)
    .fetch_optional(&state.db)
    .await?;

    let stored_hash = user.as_ref().map(|user| user.password_hash.clone());
    let password_ok = password::verify(body.password, stored_hash).await?;

    let Some(user) = user.filter(|_| password_ok) else {
        // Журнал неизменяем, поэтому произвольный текст от неизвестного человека в него не пишем:
        // логин сохраняется, только если такой сотрудник есть. Иначе в журнал навсегда попал бы,
        // например, пароль, по ошибке набранный в поле логина.
        let known_login: Option<String> = sqlx::query_scalar("SELECT login::text FROM users WHERE login = $1::citext")
            .bind(&login)
            .fetch_optional(&state.db)
            .await?;
        audit::record(
            &state.db,
            Entry::new(&Actor::anonymous(), "auth.login_failed")
                .details(json!({ "login": known_login, "ip": ip.to_string() })),
        )
        .await?;
        // Один ответ и для неизвестного логина, и для неверного пароля.
        return Err(ApiError::Unauthorized("invalid_credentials"));
    };

    // О блокировке сообщаем только тому, кто ввёл правильный пароль.
    if !user.is_active {
        return Err(ApiError::Unauthorized("user_blocked"));
    }

    let mut tx = state.db.begin().await?;

    // Пароль проверялся вне транзакции. Блокируем строку сотрудника и убеждаемся, что за это
    // время его не заблокировали и пароль не сменили: иначе вход по старому паролю выдал бы
    // сессию уже после сброса.
    let current: Option<(bool, String)> =
        sqlx::query_as("SELECT is_active, password_hash FROM users WHERE id = $1 FOR SHARE")
            .bind(user.id)
            .fetch_optional(&mut *tx)
            .await?;
    match current {
        Some((true, hash)) if hash == user.password_hash => {}
        Some((false, _)) => return Err(ApiError::Unauthorized("user_blocked")),
        _ => return Err(ApiError::Unauthorized("invalid_credentials")),
    }
    state.login_limiter.record_success(&login, ip);

    // Устройство привязывается к вошедшему сотруднику. Прежние сессии на нём закрываются —
    // и свои старые, и чужие, если раньше на устройстве работал другой сотрудник.
    session::revoke_for_device(&mut tx, body.device_id).await?;
    sqlx::query(
        "INSERT INTO devices (id, user_id, model) VALUES ($1, $2, $3)
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, model = EXCLUDED.model, revoked_at = NULL",
    )
    .bind(body.device_id)
    .bind(user.id)
    .bind(&device_model)
    .execute(&mut *tx)
    .await?;

    let tokens =
        session::start(&mut tx, &state.jwt, state.config.refresh_ttl_days, user.id, body.device_id).await?;

    let actor = Actor { user_id: Some(user.id), full_name: user.full_name, device_id: Some(body.device_id) };
    audit::record(
        &mut *tx,
        Entry::new(&actor, "auth.login")
            .entity("user", user.id)
            .details(json!({ "ip": ip.to_string(), "device_model": device_model })),
    )
    .await?;

    let profile = load_profile(&mut tx, user.id).await?;
    tx.commit().await?;

    Ok(Json(SessionResponse { tokens, user: profile }))
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    refresh_token: String,
    device_id: Uuid,
}

pub async fn refresh(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<RefreshRequest>,
) -> ApiResult<Json<TokenPair>> {
    if body.refresh_token.len() > 128 {
        return Err(ApiError::Unauthorized("invalid_refresh_token"));
    }
    let pair = session::rotate(
        &state.db,
        &state.jwt,
        state.config.refresh_ttl_days,
        &body.refresh_token,
        body.device_id,
    )
    .await?;
    Ok(Json(pair))
}

/// Выход на этом устройстве. Доступен и тому, кто ещё не сменил временный пароль.
/// Сессия закрывается целиком: перестают действовать и refresh-, и access-токен.
pub async fn logout(State(state): State<AppState>, user: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let mut tx = state.db.begin().await?;
    session::revoke_session(&mut tx, user.session_id).await?;
    audit::record(&mut *tx, Entry::new(&user.actor(), "auth.logout").entity("user", user.id)).await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// Смена своего пароля. Все прежние сессии закрываются — вместе с уже выданными токенами,
/// включая токен этого запроса, — а устройство получает новую сессию.
pub async fn change_password(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    user: AuthUser,
    ApiJson(body): ApiJson<ChangePasswordRequest>,
) -> ApiResult<Json<SessionResponse>> {
    let mut fields = FieldErrors::new();
    if !password::is_acceptable(&body.new_password) {
        fields.insert("new_password", "invalid_length");
    } else if body.new_password == body.current_password {
        fields.insert("new_password", "same_as_current");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }
    if body.current_password.chars().count() > password::MAX_LEN {
        return Err(ApiError::field("current_password", "invalid"));
    }

    // Перебор текущего пароля по украденному токену ограничен так же, как вход.
    let limiter_key = format!("change-password:{}", user.id);
    state
        .login_limiter
        .begin(&limiter_key, ip)
        .map_err(|retry_after_secs| ApiError::RateLimited { retry_after_secs })?;

    let stored_hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if !password::verify(body.current_password, Some(stored_hash.clone())).await? {
        return Err(ApiError::field("current_password", "invalid"));
    }
    state.login_limiter.record_success(&limiter_key, ip);

    let new_hash = password::hash(body.new_password).await?;

    let mut tx = state.db.begin().await?;
    // Условие на прежний хэш: если пароль за это время уже сменили (второй запрос, сброс
    // владельцем), этот запрос ничего не меняет. UPDATE блокирует строку сотрудника, поэтому
    // идущее одновременно обновление токена дождётся конца и получит уже закрытую сессию.
    let updated = sqlx::query(
        "UPDATE users SET password_hash = $2, must_change_password = false, updated_at = $3
         WHERE id = $1 AND password_hash = $4 AND is_active",
    )
    .bind(user.id)
    .bind(new_hash)
    .bind(OffsetDateTime::now_utc())
    .bind(stored_hash)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::Conflict("password_changed_elsewhere"));
    }

    session::revoke_all_for_user(&mut tx, user.id).await?;
    let tokens =
        session::start(&mut tx, &state.jwt, state.config.refresh_ttl_days, user.id, user.device_id).await?;
    audit::record(&mut *tx, Entry::new(&user.actor(), "auth.password_changed").entity("user", user.id)).await?;
    let profile = load_profile(&mut tx, user.id).await?;
    tx.commit().await?;

    Ok(Json(SessionResponse { tokens, user: profile }))
}

/// Профиль текущего сотрудника. Доступен и до смены временного пароля —
/// приложение узнаёт из него, что пароль нужно сменить.
pub async fn me(State(state): State<AppState>, user: AuthUser) -> ApiResult<Json<Profile>> {
    let mut conn = state.db.acquire().await?;
    Ok(Json(load_profile(&mut conn, user.id).await?))
}

async fn load_profile(conn: &mut sqlx::PgConnection, user_id: Uuid) -> sqlx::Result<Profile> {
    sqlx::query_as(
        "SELECT id, login::text AS login, first_name, last_name, role, permissions,
                default_warehouse_id, must_change_password
         FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(conn)
    .await
}
