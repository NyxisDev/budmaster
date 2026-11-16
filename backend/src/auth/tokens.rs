//! Токены: короткий access (JWT, HS256) и длинный refresh (случайная строка,
//! в базе хранится только её SHA-256).

use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    /// id сотрудника
    pub sub: Uuid,
    /// id устройства
    pub did: Uuid,
    /// id сессии (цепочки refresh-токенов). Сессию отозвали — токен больше не принимается.
    pub sid: Uuid,
    pub iat: i64,
    pub exp: i64,
}

/// Ключи подписи access-токенов.
#[derive(Clone)]
pub struct JwtKeys {
    encoding: EncodingKey,
    decoding: DecodingKey,
    ttl_secs: i64,
}

impl JwtKeys {
    pub fn new(secret: &[u8], ttl_secs: i64) -> anyhow::Result<Self> {
        Ok(Self {
            encoding: EncodingKey::from_secret(secret)?,
            decoding: DecodingKey::from_secret(secret)?,
            ttl_secs,
        })
    }

    pub fn ttl_secs(&self) -> i64 {
        self.ttl_secs
    }

    pub fn issue(&self, user_id: Uuid, device_id: Uuid, session_id: Uuid) -> anyhow::Result<String> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let claims = AccessClaims { sub: user_id, did: device_id, sid: session_id, iat: now, exp: now + self.ttl_secs };
        Ok(jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)?)
    }

    /// Возвращает данные токена, если подпись верна и срок не истёк.
    pub fn verify(&self, token: &str) -> Option<AccessClaims> {
        // Алгоритм задан жёстко: токен с другим алгоритмом (в том числе «none») не пройдёт.
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 5;
        validation.set_required_spec_claims(&["exp", "sub"]);
        jsonwebtoken::decode::<AccessClaims>(token, &self.decoding, &validation)
            .ok()
            .map(|data| data.claims)
    }
}

/// Новый refresh-токен: 32 случайных байта в base64url.
pub fn new_refresh_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

/// Отпечаток refresh-токена для хранения в базе.
pub fn hash_refresh_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn access_token_roundtrip() {
        let keys = JwtKeys::new(SECRET, 900).unwrap();
        let (user, device, session) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let claims = keys.verify(&keys.issue(user, device, session).unwrap()).unwrap();
        assert_eq!((claims.sub, claims.did, claims.sid), (user, device, session));
    }

    #[test]
    fn rejects_foreign_expired_and_garbage_tokens() {
        let keys = JwtKeys::new(SECRET, 900).unwrap();
        let other = JwtKeys::new(b"ffffffffffffffffffffffffffffffff", 900).unwrap();
        let token = other.issue(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()).unwrap();
        assert!(keys.verify(&token).is_none());

        let expired = JwtKeys::new(SECRET, -3600)
            .unwrap()
            .issue(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4())
            .unwrap();
        assert!(keys.verify(&expired).is_none());

        assert!(keys.verify("не токен").is_none());
        assert!(keys.verify("").is_none());
    }

    #[test]
    fn rejects_unsigned_token() {
        let keys = JwtKeys::new(SECRET, 900).unwrap();
        let header = BASE64_URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let payload = BASE64_URL_SAFE_NO_PAD.encode(
            serde_json::json!({ "sub": Uuid::new_v4(), "did": Uuid::new_v4(), "sid": Uuid::new_v4(), "iat": now, "exp": now + 900 })
                .to_string(),
        );
        assert!(keys.verify(&format!("{header}.{payload}.")).is_none());
    }

    #[test]
    fn refresh_tokens_are_random_and_hashed() {
        let (a, b) = (new_refresh_token(), new_refresh_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert_eq!(hash_refresh_token(&a).len(), 32);
        assert_eq!(hash_refresh_token(&a), hash_refresh_token(&a));
        assert_ne!(hash_refresh_token(&a), hash_refresh_token(&b));
    }
}
