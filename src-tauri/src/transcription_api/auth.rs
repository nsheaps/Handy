//! Bearer-token credential for the transcription API.
//!
//! The token itself lives only in the operating system's credential store
//! (macOS Keychain, Windows Credential Manager, Secret Service on Linux). The
//! running server keeps just a SHA-256 digest of it in memory, so verifying a
//! request never touches the credential store and the plaintext token is not
//! kept around after it has been generated or revealed.

use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};

const KEYRING_SERVICE: &str = "com.pais.handy.transcription-api";
const KEYRING_USER: &str = "bearer-token";
const TOKEN_PREFIX: &str = "handy_";
const TOKEN_BYTES: usize = 32;

#[derive(Debug)]
pub struct SecretError(pub String);

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretError {}

/// Where the token is persisted. Calls may block (D-Bus, Keychain prompts), so
/// never invoke them on an async executor thread or the UI thread.
pub trait SecretStore: Send + Sync {
    fn get(&self) -> Result<Option<String>, SecretError>;
    fn set(&self, token: &str) -> Result<(), SecretError>;
}

/// OS-backed store.
pub struct KeyringStore;

impl KeyringStore {
    fn entry() -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .map_err(|e| SecretError(format!("secure credential store unavailable: {e}")))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self) -> Result<Option<String>, SecretError> {
        match Self::entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError(format!("could not read credential: {e}"))),
        }
    }

    fn set(&self, token: &str) -> Result<(), SecretError> {
        Self::entry()?
            .set_password(token)
            .map_err(|e| SecretError(format!("could not store credential: {e}")))
    }
}

/// A fresh high-entropy token: 256 bits from the OS CSPRNG, hex encoded.
pub fn generate_token() -> Result<String, SecretError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| SecretError(format!("could not gather randomness: {e}")))?;
    let mut token = String::with_capacity(TOKEN_PREFIX.len() + TOKEN_BYTES * 2);
    token.push_str(TOKEN_PREFIX);
    for b in bytes {
        token.push_str(&format!("{b:02x}"));
    }
    Ok(token)
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Constant-time comparison of two equal-length digests.
fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The credential the running server checks requests against. Cloning shares
/// the same state, so rotating or clearing the token is seen by the server
/// immediately.
#[derive(Clone, Default)]
pub struct Credential {
    digest: Arc<RwLock<Option<[u8; 32]>>>,
}

impl Credential {
    pub fn set_token(&self, token: &str) {
        *self.digest.write().unwrap_or_else(|e| e.into_inner()) = Some(digest(token));
    }

    #[cfg(test)]
    pub fn clear(&self) {
        *self.digest.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn is_set(&self) -> bool {
        self.digest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// With no credential configured every request is rejected.
    pub fn verify(&self, presented: &str) -> bool {
        let guard = self.digest.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(expected) => digests_equal(expected, &digest(presented)),
            None => false,
        }
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header value.
pub fn parse_bearer(header: &str) -> Option<&str> {
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    #[allow(dead_code)]
    pub struct MemoryStore(pub Mutex<Option<String>>);

    impl SecretStore for MemoryStore {
        fn get(&self) -> Result<Option<String>, SecretError> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set(&self, token: &str) -> Result<(), SecretError> {
            *self.0.lock().unwrap() = Some(token.to_string());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_long() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_PREFIX.len() + TOKEN_BYTES * 2);
        assert!(a.starts_with(TOKEN_PREFIX));
    }

    #[test]
    fn verify_accepts_only_the_current_token() {
        let cred = Credential::default();
        assert!(!cred.verify("anything"), "no credential rejects everything");
        cred.set_token("secret-one");
        assert!(cred.verify("secret-one"));
        assert!(!cred.verify("secret-two"));
        assert!(!cred.verify(""));
        cred.set_token("secret-two");
        assert!(
            !cred.verify("secret-one"),
            "rotation invalidates the old token"
        );
        assert!(cred.verify("secret-two"));
        cred.clear();
        assert!(!cred.verify("secret-two"));
    }

    #[test]
    fn clones_share_state() {
        let cred = Credential::default();
        let other = cred.clone();
        cred.set_token("t");
        assert!(other.verify("t"));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("BEARER  abc "), Some("abc"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer("abc"), None);
    }
}
