//! Password hashing and opaque token handling (FR-04, NFR-16).

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};

pub fn hash_password(password: &str) -> ApiResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiError::Internal(format!("hashing failed: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// A fixed hash to verify against when the email is unknown, so unknown and known emails take the same time.
pub fn dummy_verify(password: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = DUMMY.get_or_init(|| hash_password("dummy-password-for-timing").unwrap_or_default());
    let _ = verify_password(password, hash);
}

/// Password rules: at least 10 characters, with a letter and a digit.
pub fn check_password_strength(password: &str) -> ApiResult<()> {
    let long = password.chars().count() >= 10;
    let letter = password.chars().any(|c| c.is_alphabetic());
    let digit = password.chars().any(|c| c.is_ascii_digit());
    if long && letter && digit {
        Ok(())
    } else {
        Err(ApiError::bad("password must be at least 10 characters and contain a letter and a digit"))
    }
}

/// 256 random bits, hex encoded. The caller shows this once; only `hash_token` of it is stored.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn normalize_email(email: &str) -> ApiResult<String> {
    let e = email.trim().to_lowercase();
    let ok = e.len() <= 254
        && e.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.'))
        && !e.contains(char::is_whitespace);
    if ok {
        Ok(e)
    } else {
        Err(ApiError::bad("a valid email address is required"))
    }
}
