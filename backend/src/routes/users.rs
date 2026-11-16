//! Управление сотрудниками. Только владелец.
//!
//! Через API можно создавать и менять только работников. Владельцы назначаются
//! командой на сервере, и никакой запрос не может создать, изменить или заблокировать владельца.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::audit::{self, Entry};
use crate::auth::extract::{ApiJson, ApiPath};
use crate::auth::{Owner, Role, password, session};
use crate::error::{ApiError, ApiResult, FieldErrors};
use crate::permissions;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct UserDto {
    pub id: Uuid,
    pub login: String,
    pub first_name: String,
    pub last_name: String,
    pub role: Role,
    pub permissions: i64,
    pub default_warehouse_id: Option<Uuid>,
    pub is_active: bool,
    pub must_change_password: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub async fn list(State(state): State<AppState>, _owner: Owner) -> ApiResult<Json<serde_json::Value>> {
    let users: Vec<UserDto> = sqlx::query_as(
        "SELECT id, login::text AS login, first_name, last_name, role, permissions,
                default_warehouse_id, is_active, must_change_password, created_at
         FROM users
         ORDER BY is_active DESC, last_name, first_name",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "items": users })))
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    login: String,
    password: String,
    first_name: String,
    last_name: String,
    default_warehouse_id: Uuid,
    #[serde(default)]
    permissions: i64,
}

pub async fn create(
    State(state): State<AppState>,
    Owner(owner): Owner,
    ApiJson(body): ApiJson<CreateUserRequest>,
) -> ApiResult<(StatusCode, Json<UserDto>)> {
    let login = body.login.trim().to_lowercase();
    let first_name = body.first_name.trim().to_owned();
    let last_name = body.last_name.trim().to_owned();

    let mut fields = FieldErrors::new();
    if !is_valid_login(&login) {
        fields.insert("login", "invalid");
    }
    if !password::is_acceptable(&body.password) {
        fields.insert("password", "invalid_length");
    }
    if !is_valid_name(&first_name) {
        fields.insert("first_name", "invalid");
    }
    if !is_valid_name(&last_name) {
        fields.insert("last_name", "invalid");
    }
    if !permissions::is_valid_mask(body.permissions) {
        fields.insert("permissions", "invalid");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }

    let password_hash = password::hash(body.password).await?;

    let mut tx = state.db.begin().await?;
    ensure_active_warehouse(&mut tx, body.default_warehouse_id).await?;

    let id = Uuid::new_v4();
    // Роль жёстко «worker»: из запроса она не читается вовсе.
    // Пароль задал владелец, поэтому при первом входе работник обязан его сменить.
    let inserted = sqlx::query(
        "INSERT INTO users
            (id, login, password_hash, first_name, last_name, role, permissions, default_warehouse_id, must_change_password)
         VALUES ($1, $2, $3, $4, $5, 'worker', $6, $7, true)
         ON CONFLICT (login) DO NOTHING",
    )
    .bind(id)
    .bind(&login)
    .bind(password_hash)
    .bind(&first_name)
    .bind(&last_name)
    .bind(body.permissions)
    .bind(body.default_warehouse_id)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(ApiError::field("login", "taken"));
    }

    let user = fetch_user(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    audit::record(
        &mut *tx,
        Entry::new(&owner.actor(), "user.create").entity("user", id).details(json!({
            "login": user.login,
            "first_name": user.first_name,
            "last_name": user.last_name,
            "permissions": user.permissions,
            "default_warehouse_id": user.default_warehouse_id,
        })),
    )
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    first_name: Option<String>,
    last_name: Option<String>,
    default_warehouse_id: Option<Uuid>,
    permissions: Option<i64>,
}

pub async fn update(
    State(state): State<AppState>,
    Owner(owner): Owner,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateUserRequest>,
) -> ApiResult<Json<UserDto>> {
    let first_name = body.first_name.map(|name| name.trim().to_owned());
    let last_name = body.last_name.map(|name| name.trim().to_owned());

    let mut fields = FieldErrors::new();
    if first_name.as_deref().is_some_and(|name| !is_valid_name(name)) {
        fields.insert("first_name", "invalid");
    }
    if last_name.as_deref().is_some_and(|name| !is_valid_name(name)) {
        fields.insert("last_name", "invalid");
    }
    if body.permissions.is_some_and(|mask| !permissions::is_valid_mask(mask)) {
        fields.insert("permissions", "invalid");
    }
    if !fields.is_empty() {
        return Err(ApiError::Validation(fields));
    }
    // Пустой запрос ничего не меняет — не засоряем им журнал и счётчик изменений.
    if first_name.is_none() && last_name.is_none() && body.default_warehouse_id.is_none() && body.permissions.is_none()
    {
        return Err(ApiError::BadRequest("nothing_to_update"));
    }

    let mut tx = state.db.begin().await?;
    let before = lock_worker(&mut tx, id).await?;
    if let Some(warehouse_id) = body.default_warehouse_id {
        ensure_active_warehouse(&mut tx, warehouse_id).await?;
    }

    sqlx::query(
        "UPDATE users SET
            first_name = COALESCE($2, first_name),
            last_name = COALESCE($3, last_name),
            default_warehouse_id = COALESCE($4, default_warehouse_id),
            permissions = COALESCE($5, permissions),
            updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&first_name)
    .bind(&last_name)
    .bind(body.default_warehouse_id)
    .bind(body.permissions)
    .execute(&mut *tx)
    .await?;

    let after = fetch_user(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    // В журнал идут значения «было» и «стало» — в том числе маска прав.
    audit::record(
        &mut *tx,
        Entry::new(&owner.actor(), "user.update").entity("user", id).details(json!({
            "before": editable_fields(&before),
            "after": editable_fields(&after),
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(after))
}

pub async fn block(
    State(state): State<AppState>,
    Owner(owner): Owner,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<UserDto>> {
    let mut tx = state.db.begin().await?;
    let user = lock_worker(&mut tx, id).await?;
    if !user.is_active {
        return Err(ApiError::Conflict("already_blocked"));
    }

    let now = OffsetDateTime::now_utc();
    sqlx::query("UPDATE users SET is_active = false, updated_at = $2 WHERE id = $1")
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO user_blocks (id, user_id, blocked_at, blocked_by) VALUES ($1, $2, $3, $4)")
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(now)
        .bind(owner.id)
        .execute(&mut *tx)
        .await?;
    session::revoke_all_for_user(&mut tx, id).await?;
    audit::record(&mut *tx, Entry::new(&owner.actor(), "user.block").entity("user", id)).await?;

    let after = fetch_user(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    tx.commit().await?;
    Ok(Json(after))
}

pub async fn unblock(
    State(state): State<AppState>,
    Owner(owner): Owner,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<UserDto>> {
    let mut tx = state.db.begin().await?;
    let user = lock_worker(&mut tx, id).await?;
    if user.is_active {
        return Err(ApiError::Conflict("not_blocked"));
    }

    let now = OffsetDateTime::now_utc();
    sqlx::query("UPDATE users SET is_active = true, updated_at = $2 WHERE id = $1")
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    // Запись о блокировке не стирается, а закрывается: период блокировки остаётся в истории.
    sqlx::query(
        "UPDATE user_blocks SET unblocked_at = $2, unblocked_by = $3 WHERE user_id = $1 AND unblocked_at IS NULL",
    )
    .bind(id)
    .bind(now)
    .bind(owner.id)
    .execute(&mut *tx)
    .await?;
    audit::record(&mut *tx, Entry::new(&owner.actor(), "user.unblock").entity("user", id)).await?;

    let after = fetch_user(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    tx.commit().await?;
    Ok(Json(after))
}

#[derive(Deserialize)]
pub struct ResetPasswordRequest {
    new_password: String,
}

/// Владелец задаёт работнику временный пароль. Все сессии работника закрываются,
/// при следующем входе он обязан пароль сменить.
pub async fn reset_password(
    State(state): State<AppState>,
    Owner(owner): Owner,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<ResetPasswordRequest>,
) -> ApiResult<Json<UserDto>> {
    if !password::is_acceptable(&body.new_password) {
        return Err(ApiError::field("new_password", "invalid_length"));
    }
    let new_hash = password::hash(body.new_password).await?;

    let mut tx = state.db.begin().await?;
    lock_worker(&mut tx, id).await?;
    sqlx::query(
        "UPDATE users SET password_hash = $2, must_change_password = true, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(new_hash)
    .execute(&mut *tx)
    .await?;
    session::revoke_all_for_user(&mut tx, id).await?;
    audit::record(&mut *tx, Entry::new(&owner.actor(), "user.password_reset").entity("user", id)).await?;

    let after = fetch_user(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    tx.commit().await?;
    Ok(Json(after))
}

/// Блокирует строку сотрудника до конца транзакции и проверяет, что это работник.
async fn lock_worker(conn: &mut PgConnection, id: Uuid) -> ApiResult<UserDto> {
    let user: Option<UserDto> = sqlx::query_as(
        "SELECT id, login::text AS login, first_name, last_name, role, permissions,
                default_warehouse_id, is_active, must_change_password, created_at
         FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    let user = user.ok_or(ApiError::NotFound)?;
    if user.role == Role::Owner {
        return Err(ApiError::Forbidden("owner_protected"));
    }
    Ok(user)
}

async fn fetch_user(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<UserDto>> {
    sqlx::query_as(
        "SELECT id, login::text AS login, first_name, last_name, role, permissions,
                default_warehouse_id, is_active, must_change_password, created_at
         FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await
}

async fn ensure_active_warehouse(conn: &mut PgConnection, id: Uuid) -> ApiResult<()> {
    let active: Option<bool> = sqlx::query_scalar("SELECT is_active FROM warehouses WHERE id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    match active {
        Some(true) => Ok(()),
        Some(false) => Err(ApiError::field("default_warehouse_id", "inactive")),
        None => Err(ApiError::field("default_warehouse_id", "not_found")),
    }
}

fn editable_fields(user: &UserDto) -> serde_json::Value {
    json!({
        "first_name": user.first_name,
        "last_name": user.last_name,
        "permissions": user.permissions,
        "default_warehouse_id": user.default_warehouse_id,
    })
}

/// Логин: 3–32 символа, латиница в нижнем регистре, цифры, точка, дефис, подчёркивание;
/// начинается с буквы или цифры.
pub fn is_valid_login(login: &str) -> bool {
    let mut chars = login.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && (3..=32).contains(&login.len())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Имя или фамилия: 1–50 символов, без управляющих символов.
pub fn is_valid_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=50).contains(&len) && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_rules() {
        for ok in ["ivan", "ivan.petrenko", "a1_b-2", "abc"] {
            assert!(is_valid_login(ok), "{ok}");
        }
        for bad in ["", "ab", "Ivan", "іван", ".ivan", "ivan petrenko", "a@b", &"a".repeat(33)] {
            assert!(!is_valid_login(bad), "{bad}");
        }
    }

    #[test]
    fn name_rules() {
        assert!(is_valid_name("Іван"));
        assert!(is_valid_name("Анна-Марія"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("a\nb"));
        assert!(!is_valid_name(&"я".repeat(51)));
    }
}
