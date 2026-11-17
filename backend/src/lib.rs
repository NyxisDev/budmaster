//! Сервер Budmaster: учёт стройматериалов и публичный каталог.
//! Архитектура и схема данных — в `docs/design.md`.

pub mod audit;
pub mod auth;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod error;
pub mod permissions;
pub mod routes;

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::rate_limit::LoginLimiter;
use crate::auth::tokens::JwtKeys;
use crate::config::Config;

/// Общее состояние сервера. Клонируется дёшево: внутри счётчики ссылок.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub jwt: JwtKeys,
    pub login_limiter: Arc<LoginLimiter>,
}

impl AppState {
    pub fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let jwt = JwtKeys::new(&config.jwt_secret, config.access_ttl_secs)?;
        Ok(Self { db, config: Arc::new(config), jwt, login_limiter: Arc::new(LoginLimiter::default()) })
    }
}
