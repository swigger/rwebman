use anyhow::{Result, anyhow, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::http::HeaderMap;
use rand::{RngCore, rngs::OsRng};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub const SESSION_SECONDS: u64 = 8 * 60 * 60;
pub const COOKIE_NAME: &str = "rwebman_session";

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.chars().count() < 12 {
        bail!("Password must contain at least 12 characters");
    }
    if password.len() > 1024 {
        bail!("Password is too long (maximum 1024 bytes)");
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("Password hashing failed: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    if password.len() > 1024 {
        return false;
    }
    PasswordHash::new(hash).ok().is_some_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

#[derive(Clone)]
pub struct Session {
    pub csrf: String,
    pub expires: Instant,
    pub cancelled: CancellationToken,
}

#[derive(Default)]
pub struct Auth {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    attempts: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Auth {
    pub fn create(&self) -> Option<(String, Arc<Session>)> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| s.expires > Instant::now() && !s.cancelled.is_cancelled());
        if sessions.len() >= 32 {
            return None;
        }
        let token = random_token();
        let session = Arc::new(Session {
            csrf: random_token(),
            expires: Instant::now() + Duration::from_secs(SESSION_SECONDS),
            cancelled: CancellationToken::new(),
        });
        sessions.insert(token.clone(), session.clone());
        Some((token, session))
    }

    pub fn session(&self, headers: &HeaderMap) -> Option<Arc<Session>> {
        let token = cookie_token(headers)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get(token)?.clone();
        if session.expires <= Instant::now() || session.cancelled.is_cancelled() {
            session.cancelled.cancel();
            sessions.remove(token);
            return None;
        }
        Some(session)
    }

    pub fn logout(&self, headers: &HeaderMap) {
        if let Some(token) = cookie_token(headers)
            && let Some(session) = self.sessions.lock().unwrap().remove(token)
        {
            session.cancelled.cancel();
        }
    }

    pub fn cancel_all(&self) {
        for session in self.sessions.lock().unwrap().drain().map(|(_, s)| s) {
            session.cancelled.cancel();
        }
    }

    /// Charge attempts before hashing, limiting concurrent brute force as well as CPU use.
    pub fn allow_login(&self, ip: IpAddr) -> bool {
        let mut attempts = self.attempts.lock().unwrap();
        attempts.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        if attempts.len() >= 4096 && !attempts.contains_key(&ip) {
            return false;
        }
        let entry = attempts.entry(ip).or_insert((Instant::now(), 0));
        entry.1 += 1;
        entry.1 <= 5
    }
}

pub fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == COOKIE_NAME
                && value.len() == 64
                && value.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(value)
        })
}

pub fn cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_SECONDS}{}",
        if secure { "; Secure" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passwords_sessions_and_revocation() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("incorrect password", &hash));
        assert!(hash_password("short").is_err());
        let auth = Auth::default();
        let (token, session) = auth.create().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("cookie", format!("{COOKIE_NAME}={token}").parse().unwrap());
        assert!(auth.session(&headers).is_some());
        auth.logout(&headers);
        assert!(auth.session(&headers).is_none());
        assert!(session.cancelled.is_cancelled());
    }
    #[test]
    fn login_is_rate_limited() {
        let auth = Auth::default();
        let ip = "127.0.0.1".parse().unwrap();
        for _ in 0..5 {
            assert!(auth.allow_login(ip));
        }
        assert!(!auth.allow_login(ip));
    }
}
