//! Вход и проверка доступа.
//!
//! Правило: токен говорит только «кто это». Роль, права и блокировка читаются из базы
//! при каждом запросе, поэтому отозванное право или блокировка действуют сразу.

pub mod extract;
pub mod password;
pub mod rate_limit;
pub mod session;
pub mod tokens;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::AppState;
use crate::audit::Actor;
use crate::error::ApiError;
use crate::permissions;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, sqlx::Type)]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Worker,
}

/// Сотрудник, чей токен принят. Может быть обязан сменить пароль —
/// для обычных эндпоинтов используйте [`ActiveUser`] или [`Owner`].
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub role: Role,
    pub permissions: i64,
    pub must_change_password: bool,
    pub device_id: Uuid,
    /// Сессия, в рамках которой выдан токен.
    pub session_id: Uuid,
}

impl AuthUser {
    pub fn is_owner(&self) -> bool {
        self.role == Role::Owner
    }

    /// Есть ли право из битовой маски. Владельцу разрешено всё.
    pub fn can(&self, permission: i64) -> bool {
        self.is_owner() || permissions::has(self.permissions, permission)
    }

    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
    }

    pub fn actor(&self) -> Actor {
        Actor { user_id: Some(self.id), full_name: self.full_name(), device_id: Some(self.device_id) }
    }
}

#[derive(sqlx::FromRow)]
struct AuthRow {
    first_name: String,
    last_name: String,
    role: Role,
    permissions: i64,
    is_active: bool,
    must_change_password: bool,
    session_alive: bool,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let state = AppState::from_ref(state);

        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized("missing_token"))?;
        let claims = state.jwt.verify(token).ok_or(ApiError::Unauthorized("invalid_token"))?;

        load_user(&state.db, claims.sub, claims.did, claims.sid).await
    }
}

async fn load_user(db: &PgPool, user_id: Uuid, device_id: Uuid, session_id: Uuid) -> Result<AuthUser, ApiError> {
    let row: Option<AuthRow> = sqlx::query_as(
        "SELECT u.first_name, u.last_name, u.role, u.permissions, u.is_active, u.must_change_password,
                EXISTS (
                    SELECT 1 FROM refresh_tokens t
                    WHERE t.family_id = $2 AND t.user_id = u.id AND t.device_id = $3
                      AND t.revoked_at IS NULL AND t.expires_at > now()
                ) AS session_alive
         FROM users u
         WHERE u.id = $1",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(device_id)
    .fetch_optional(db)
    .await?;

    let row = row.ok_or(ApiError::Unauthorized("invalid_token"))?;
    if !row.is_active {
        return Err(ApiError::Unauthorized("user_blocked"));
    }
    // Сессию закрыли (выход, смена или сброс пароля, вход другого сотрудника на устройстве,
    // кража refresh-токена) — её access-токен больше не действует, не дожидаясь своего срока.
    if !row.session_alive {
        return Err(ApiError::Unauthorized("invalid_token"));
    }

    Ok(AuthUser {
        id: user_id,
        first_name: row.first_name,
        last_name: row.last_name,
        role: row.role,
        permissions: row.permissions,
        must_change_password: row.must_change_password,
        device_id,
        session_id,
    })
}

/// Сотрудник, которому можно работать: токен принят и пароль не требует смены.
#[derive(Clone, Debug)]
pub struct ActiveUser(pub AuthUser);

impl<S> FromRequestParts<S> for ActiveUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if user.must_change_password {
            return Err(ApiError::Forbidden("password_change_required"));
        }
        Ok(Self(user))
    }
}

/// Владелец. Работник получает 403.
#[derive(Clone, Debug)]
pub struct Owner(pub AuthUser);

impl<S> FromRequestParts<S> for Owner
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ActiveUser(user) = ActiveUser::from_request_parts(parts, state).await?;
        if !user.is_owner() {
            return Err(ApiError::Forbidden("owner_only"));
        }
        Ok(Self(user))
    }
}

/// IP клиента — для ограничения попыток входа и журнала.
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let state = AppState::from_ref(state);
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|info| info.0.ip());

        let forwarded = state.config.trust_proxy.then(|| forwarded_ip(&parts.headers)).flatten();
        Ok(Self(forwarded.or(peer).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))))
    }
}

/// IP клиента из X-Forwarded-For. Клиент может прислать этот заголовок сам, поэтому верим ему,
/// только когда сервер стоит за нашим прокси, и берём самый последний адрес — его дописал прокси.
/// Заголовок может прийти несколькими строками: смотрим последнюю.
fn forwarded_ip(headers: &axum::http::HeaderMap) -> Option<IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|last| last.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    fn headers(lines: &[&'static str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for line in lines {
            map.append("x-forwarded-for", HeaderValue::from_static(line));
        }
        map
    }

    #[test]
    fn takes_the_address_added_by_our_proxy() {
        let ip = |text: &str| text.parse::<IpAddr>().ok();
        assert_eq!(forwarded_ip(&headers(&["203.0.113.7"])), ip("203.0.113.7"));
        // Клиент подставил свой адрес первым — он игнорируется.
        assert_eq!(forwarded_ip(&headers(&["1.1.1.1, 203.0.113.7"])), ip("203.0.113.7"));
        // Клиент прислал свою строку заголовка, прокси добавил отдельную.
        assert_eq!(forwarded_ip(&headers(&["1.1.1.1", "203.0.113.7"])), ip("203.0.113.7"));
        assert_eq!(forwarded_ip(&headers(&["2001:db8::1"])), ip("2001:db8::1"));
        assert_eq!(forwarded_ip(&headers(&["not-an-ip"])), None);
        assert_eq!(forwarded_ip(&headers(&[])), None);
    }
}
