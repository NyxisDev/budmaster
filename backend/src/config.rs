//! Настройки сервера. Всё берётся из переменных окружения (см. `.env.example`).

use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;

use anyhow::{Context, bail};
use axum::http::HeaderValue;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub jwt_secret: Vec<u8>,
    /// Срок жизни access-токена, секунды.
    pub access_ttl_secs: i64,
    /// Срок жизни refresh-токена, дни.
    pub refresh_ttl_days: i64,
    /// Сервер стоит за обратным прокси и берёт IP клиента из X-Forwarded-For.
    pub trust_proxy: bool,
    /// Адреса сайтов, которым разрешены запросы из браузера.
    pub cors_origins: Vec<String>,
    /// Адрес сервера, каким его видит браузер покупателя. Из него строятся ссылки на фото.
    pub public_base_url: String,
    /// Папка с фото товаров.
    pub media_dir: PathBuf,
    /// Папка архивов (журнал, реестр синхронизации, данные учёта).
    pub archive_dir: PathBuf,
    /// Заявок с сайта в час: с одного адреса и на один номер телефона.
    pub orders_per_ip_hour: usize,
    pub orders_per_phone_hour: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let jwt_secret = std::env::var("JWT_SECRET").context("не задан JWT_SECRET")?;
        // Короткий ключ подбирается перебором, а ключ из образца известен всем:
        // с такими сервер не стартует.
        if jwt_secret.len() < 32 {
            bail!("JWT_SECRET слишком короткий: нужно не меньше 32 символов");
        }
        if is_placeholder(&jwt_secret) {
            bail!("JWT_SECRET остался из образца .env.example — задайте свой случайный ключ");
        }

        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()
            .context("BIND_ADDR должен быть вида 0.0.0.0:8080")?;

        Ok(Self {
            bind_addr,
            jwt_secret: jwt_secret.into_bytes(),
            access_ttl_secs: env_number("ACCESS_TOKEN_TTL_SECS", 900, 60..=3600)?,
            refresh_ttl_days: env_number("REFRESH_TOKEN_TTL_DAYS", 60, 1..=365)?,
            trust_proxy: env_flag("TRUST_PROXY")?,
            cors_origins: parse_origins(&std::env::var("CORS_ALLOWED_ORIGINS").unwrap_or_default())?,
            public_base_url: parse_base_url(
                &std::env::var("PUBLIC_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned()),
            )?,
            media_dir: std::env::var("MEDIA_DIR").unwrap_or_else(|_| "./media".to_owned()).into(),
            archive_dir: std::env::var("ARCHIVE_DIR").unwrap_or_else(|_| "./archive".to_owned()).into(),
            orders_per_ip_hour: usize::try_from(env_number("ORDERS_PER_IP_HOUR", 5, 1..=10_000)?)?,
            orders_per_phone_hour: usize::try_from(env_number("ORDERS_PER_PHONE_HOUR", 3, 1..=10_000)?)?,
        })
    }
}

/// Значение скопировано из `.env.example` и не заменено.
pub fn is_placeholder(value: &str) -> bool {
    value.to_lowercase().contains("change-me")
}

fn env_number(name: &str, default: i64, allowed: RangeInclusive<i64>) -> anyhow::Result<i64> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let value: i64 = raw.trim().parse().with_context(|| format!("{name} должен быть числом"))?;
            if !allowed.contains(&value) {
                bail!("{name} должен быть от {} до {}", allowed.start(), allowed.end());
            }
            Ok(value)
        }
    }
}

/// Флаг «да/нет». Непонятное значение — ошибка, а не молчаливое «нет».
fn env_flag(name: &str) -> anyhow::Result<bool> {
    match std::env::var(name) {
        Err(_) => Ok(false),
        Ok(raw) => parse_flag(&raw).with_context(|| format!("{name} должен быть true или false")),
    }
}

fn parse_flag(raw: &str) -> anyhow::Result<bool> {
    match raw.trim().to_lowercase().as_str() {
        "" | "false" | "0" | "no" | "off" => Ok(false),
        "true" | "1" | "yes" | "on" => Ok(true),
        other => bail!("непонятное значение «{other}»"),
    }
}

/// Адрес вида `https://api.example.com`, без завершающей косой черты.
fn parse_base_url(raw: &str) -> anyhow::Result<String> {
    let url = raw.trim().trim_end_matches('/');
    let valid = (url.starts_with("https://") || url.starts_with("http://"))
        && !url.contains([' ', '?', '#'])
        && url.parse::<HeaderValue>().is_ok();
    if !valid {
        bail!("PUBLIC_BASE_URL: «{url}» — не адрес сервера (нужно вида https://api.example.com)");
    }
    Ok(url.to_owned())
}

/// Список адресов через запятую. Каждый должен быть вида `https://example.com`.
fn parse_origins(raw: &str) -> anyhow::Result<Vec<String>> {
    raw.split(',')
        .map(|origin| origin.trim().trim_end_matches('/'))
        .filter(|origin| !origin.is_empty())
        .map(|origin| {
            let valid = (origin.starts_with("https://") || origin.starts_with("http://"))
                && !origin.contains(['*', ' '])
                && origin.parse::<HeaderValue>().is_ok();
            if !valid {
                bail!("CORS_ALLOWED_ORIGINS: «{origin}» — не адрес сайта (нужно вида https://example.com)");
            }
            Ok(origin.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        for yes in ["true", "TRUE", "1", "yes", " on "] {
            assert!(parse_flag(yes).unwrap(), "{yes}");
        }
        for no in ["", "false", "0", "No", "off"] {
            assert!(!parse_flag(no).unwrap(), "{no}");
        }
        assert!(parse_flag("maybe").is_err());
    }

    #[test]
    fn origins() {
        assert_eq!(
            parse_origins("https://a.example/, http://localhost:5173 ,").unwrap(),
            vec!["https://a.example", "http://localhost:5173"]
        );
        assert!(parse_origins("").unwrap().is_empty());
        assert!(parse_origins("*").is_err());
        assert!(parse_origins("a.example").is_err());
    }

    #[test]
    fn base_url() {
        assert_eq!(parse_base_url(" https://api.example.com/ ").unwrap(), "https://api.example.com");
        assert!(parse_base_url("api.example.com").is_err());
        assert!(parse_base_url("https://a.example/?x=1").is_err());
    }

    #[test]
    fn placeholders_from_env_example_are_detected() {
        assert!(is_placeholder("change-me-generate-a-long-random-string"));
        assert!(!is_placeholder("4f9a0c2e7b1d4f9a0c2e7b1d4f9a0c2e"));
    }
}
