//! Time-based one-time passwords (RFC 6238: HMAC-SHA1, 30-second steps, 6 digits), the format every
//! authenticator app understands (NFR-17).

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

pub const STEP_SECONDS: i64 = 30;

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let offset = (h[19] & 0x0f) as usize;
    ((h[offset] as u32 & 0x7f) << 24) | ((h[offset + 1] as u32) << 16) | ((h[offset + 2] as u32) << 8) | (h[offset + 3] as u32)
}

/// The 6-digit code for the 30-second step containing `unix` seconds.
pub fn code_at(secret: &[u8], unix: i64) -> String {
    format!("{:06}", hotp(secret, unix.div_euclid(STEP_SECONDS) as u64) % 1_000_000)
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Checks a code against the current step and one step either side (clock drift). Returns the step that
/// matched, or `None`. A step at or before `last_step` is refused, so a code works only once.
pub fn verify(secret: &[u8], code: &str, unix: i64, last_step: Option<i64>) -> Option<i64> {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = unix.div_euclid(STEP_SECONDS);
    [now - 1, now, now + 1].into_iter().find(|step| {
        last_step.is_none_or(|last| *step > last) && same(&format!("{:06}", hotp(secret, *step as u64) % 1_000_000), code)
    })
}

/// A fresh 160-bit secret, base32 encoded as authenticator apps expect.
pub fn new_secret() -> String {
    let mut bytes = [0u8; 20];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &bytes)
}

pub fn decode_secret(b32: &str) -> Option<Vec<u8>> {
    base32::decode(base32::Alphabet::Rfc4648 { padding: false }, b32)
}

/// The `otpauth://` link an authenticator app turns into an account (usually shown as a QR code).
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    format!("otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30", enc(issuer), enc(account), secret_b32, enc(issuer))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B test secret ("12345678901234567890"); the RFC lists 8 digits, we keep the last 6.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_the_rfc_6238_test_vectors() {
        assert_eq!(code_at(RFC_SECRET, 59), "287082");
        assert_eq!(code_at(RFC_SECRET, 1_111_111_109), "081804");
        assert_eq!(code_at(RFC_SECRET, 1_234_567_890), "005924");
        assert_eq!(code_at(RFC_SECRET, 2_000_000_000), "279037");
    }

    #[test]
    fn a_code_is_accepted_once_and_only_near_its_time() {
        let t = 1_234_567_890;
        let step = verify(RFC_SECRET, "005924", t, None).expect("valid");
        assert_eq!(step, t / 30);
        assert!(verify(RFC_SECRET, "005924", t, Some(step)).is_none(), "the same step cannot be used twice");
        assert!(verify(RFC_SECRET, "005924", t + 29, None).is_some(), "still the same step");
        assert!(verify(RFC_SECRET, "005924", t + 120, None).is_none(), "too old");
        assert!(verify(RFC_SECRET, "00592", t, None).is_none() && verify(RFC_SECRET, "abcdef", t, None).is_none());
    }

    #[test]
    fn secrets_round_trip_and_uris_are_well_formed() {
        let s = new_secret();
        assert_eq!(decode_secret(&s).unwrap().len(), 20);
        let uri = otpauth_uri("Store Management", "ada@amiable.test", &s);
        assert!(uri.starts_with("otpauth://totp/Store%20Management:ada%40amiable.test?secret="));
    }
}
