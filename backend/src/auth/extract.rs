//! Разбор тела, строки запроса и пути. Обёртки над стандартными экстракторами axum:
//! при ошибке клиент получает ответ в общем формате `{ "error": { "code": ... } }`,
//! а не текст с подробностями разбора.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(match rejection {
                JsonRejection::MissingJsonContentType(_) => ApiError::BadRequest("json_expected"),
                // Тело разобрано как JSON, но в нём нет нужных полей или они не того типа.
                JsonRejection::JsonDataError(_) => ApiError::BadRequest("invalid_body"),
                JsonRejection::JsonSyntaxError(_) => ApiError::BadRequest("invalid_json"),
                other if other.status() == StatusCode::PAYLOAD_TOO_LARGE => ApiError::PayloadTooLarge,
                _ => ApiError::BadRequest("invalid_body"),
            }),
        }
    }
}

pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|_| ApiError::BadRequest("invalid_query"))
    }
}

pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // Неразборчивый идентификатор в пути — это адрес, которого нет.
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| ApiError::NotFound)
    }
}

/// Убирает управляющие символы (в том числе нулевой байт, который PostgreSQL не хранит)
/// и обрезает строку до `max_chars` символов.
pub fn clean_text(text: &str, max_chars: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::clean_text;

    #[test]
    fn clean_text_drops_control_chars_and_truncates() {
        assert_eq!(clean_text("Pixel\u{0} 8\n", 100), "Pixel 8");
        assert_eq!(clean_text("абвгд", 3), "абв");
    }
}
