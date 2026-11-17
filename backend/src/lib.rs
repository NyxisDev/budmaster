//! Сервер Budmaster: учёт стройматериалов и публичный каталог.
//! Архитектура и схема данных — в `docs/design.md`.

pub mod archive;
pub mod audit;
pub mod auth;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod error;
pub mod permissions;
pub mod routes;
pub mod sales;
pub mod sync;

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::rate_limit::{LoginLimiter, WindowLimiter};
use crate::auth::tokens::JwtKeys;
use crate::config::Config;

/// Общее состояние сервера. Клонируется дёшево: внутри счётчики ссылок.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub jwt: JwtKeys,
    pub login_limiter: Arc<LoginLimiter>,
    /// Заявки с сайта: по адресу клиента и по номеру телефона.
    pub order_ip_limiter: Arc<WindowLimiter>,
    pub order_phone_limiter: Arc<WindowLimiter>,
}

impl AppState {
    pub fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let jwt = JwtKeys::new(&config.jwt_secret, config.access_ttl_secs)?;
        let hour = std::time::Duration::from_secs(3600);
        Ok(Self {
            db,
            jwt,
            login_limiter: Arc::new(LoginLimiter::default()),
            order_ip_limiter: Arc::new(WindowLimiter::new(hour, config.orders_per_ip_hour)),
            order_phone_limiter: Arc::new(WindowLimiter::new(hour, config.orders_per_phone_hour)),
            config: Arc::new(config),
        })
    }
}
