//! Пароли: argon2id. Хэширование тяжёлое, поэтому выполняется вне асинхронных потоков
//! и с ограничением числа одновременных вычислений.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::anyhow;
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use tokio::sync::Semaphore;

use crate::error::ApiError;

pub const MIN_LEN: usize = 8;
pub const MAX_LEN: usize = 128;

/// Одно хэширование занимает около 19 МБ памяти и ядро процессора на десятки миллисекунд.
/// Без ограничения сотня одновременных попыток входа съела бы память сервера, поэтому
/// одновременно считается не больше `MAX_PARALLEL` хэшей, остальные ждут в очереди.
const MAX_PARALLEL: usize = 4;
const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);

static SLOTS: Semaphore = Semaphore::const_new(MAX_PARALLEL);

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    /// Очередь не дошла за отведённое время — сервер перегружен.
    #[error("очередь проверки паролей переполнена")]
    Busy,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<PasswordError> for ApiError {
    fn from(error: PasswordError) -> Self {
        match error {
            PasswordError::Busy => Self::Busy,
            PasswordError::Other(error) => Self::Internal(error),
        }
    }
}

/// Пароль подходит по длине. Считаются символы, а не байты: кириллица допустима.
pub fn is_acceptable(password: &str) -> bool {
    (MIN_LEN..=MAX_LEN).contains(&password.chars().count())
}

pub async fn hash(password: String) -> Result<String, PasswordError> {
    run(move || hash_blocking(&password)).await
}

/// Проверяет пароль. Если сотрудника с таким логином нет (`stored_hash = None`),
/// всё равно выполняет полную проверку против заглушки — чтобы по времени ответа
/// нельзя было узнать, существует ли логин.
pub async fn verify(password: String, stored_hash: Option<String>) -> Result<bool, PasswordError> {
    run(move || {
        let user_exists = stored_hash.is_some();
        let hash = stored_hash.unwrap_or_else(|| dummy_hash().to_owned());
        let parsed = PasswordHash::new(&hash).map_err(|e| anyhow!("повреждён хэш пароля: {e}"))?;
        let matches = Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok();
        Ok(matches && user_exists)
    })
    .await
}

async fn run<T, F>(job: F) -> Result<T, PasswordError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let _slot = tokio::time::timeout(QUEUE_TIMEOUT, SLOTS.acquire())
        .await
        .map_err(|_| PasswordError::Busy)?
        .map_err(|error| PasswordError::Other(error.into()))?;
    let result = tokio::task::spawn_blocking(job).await.map_err(|error| PasswordError::Other(error.into()))?;
    Ok(result?)
}

fn hash_blocking(password: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| anyhow!("не удалось захэшировать пароль: {e}"))
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_blocking("заглушка-для-несуществующих-логинов").expect("argon2 работает"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_and_verify_roundtrip() {
        let hash = hash("правильный пароль".to_owned()).await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify("правильный пароль".to_owned(), Some(hash.clone())).await.unwrap());
        assert!(!verify("неправильный".to_owned(), Some(hash)).await.unwrap());
    }

    #[tokio::test]
    async fn unknown_user_never_verifies() {
        assert!(!verify("заглушка-для-несуществующих-логинов".to_owned(), None).await.unwrap());
    }

    #[test]
    fn length_rules() {
        assert!(!is_acceptable("1234567"));
        assert!(is_acceptable("12345678"));
        assert!(is_acceptable("пароль12"));
        assert!(!is_acceptable(&"x".repeat(129)));
    }
}
