//! Secret resolution for API keys and the bearer token.
//!
//! Resolution order, most explicit first:
//!   1. the process (and, on Windows, persistent) environment via
//!      [`env_lookup`] — keeps CI, `setx`, and power-user workflows working
//!      unchanged;
//!   2. the OS credential store (Windows Credential Manager), where the
//!      settings UI writes keys so packaged users never touch env vars.
//!
//! Secrets live in memory only once resolved; nothing here writes a secret
//! to config, the database, or logs. The store is keyed by the same string
//! used as the env-var name (e.g. `"DEEPGRAM_API_KEY"`), so the environment
//! and the credential store share one identity per key and a single entry
//! serves every consumer of that name.

use crate::env_lookup;

/// Service name grouping all Auricle secrets in the OS credential store.
#[cfg(windows)]
const SERVICE: &str = "auricle";

/// Resolve a secret by name: environment first, then the OS credential
/// store. Empty values count as unset.
pub fn secret_lookup(name: &str) -> Option<String> {
    if let Some(v) = env_lookup(name) {
        return Some(v);
    }
    keyring_get(name)
}

/// True when a secret resolves from either source. Presence only — callers
/// that just need readiness never see the value.
pub fn secret_present(name: &str) -> bool {
    secret_lookup(name).is_some()
}

/// Store `value` in the OS credential store under `name`, replacing any
/// existing entry. Does not touch the environment.
pub fn secret_set(name: &str, value: &str) -> Result<(), String> {
    keyring_set(name, value)
}

/// Remove `name` from the OS credential store. A missing entry is not an
/// error (delete is idempotent). Does not touch the environment.
pub fn secret_delete(name: &str) -> Result<(), String> {
    keyring_delete(name)
}

#[cfg(windows)]
fn keyring_get(name: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, name).ok()?;
    match entry.get_password() {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

#[cfg(windows)]
fn keyring_set(name: &str, value: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, name)
        .map_err(|e| e.to_string())?
        .set_password(value)
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn keyring_delete(name: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// Non-Windows: no credential store is wired up (the app is Windows-only,
// like the winreg env fallback). Resolution degrades to environment-only.
#[cfg(not(windows))]
fn keyring_get(_name: &str) -> Option<String> {
    None
}

#[cfg(not(windows))]
fn keyring_set(_name: &str, _value: &str) -> Result<(), String> {
    Err("credential store is not available on this platform".to_string())
}

#[cfg(not(windows))]
fn keyring_delete(_name: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_resolves_first() {
        // PATH exists in every test environment; env resolution must find
        // it without any credential-store entry.
        assert_eq!(secret_lookup("PATH"), env_lookup("PATH"));
        assert!(secret_present("PATH"));
    }

    #[test]
    fn absent_everywhere_is_none() {
        let name = "AURICLE_TEST_SECRET_NOT_SET_ANYWHERE_9f2a";
        assert!(secret_lookup(name).is_none());
        assert!(!secret_present(name));
    }

    #[cfg(windows)]
    #[test]
    fn keyring_round_trips_when_store_available() {
        // A name that is never an env var, so this exercises the credential
        // store exclusively. Tolerant of headless environments with no
        // store (e.g. some CI), where set returns an error.
        let name = "AURICLE_TEST_KEYRING_ROUNDTRIP_7c1e";
        if secret_set(name, "s3cr3t-value").is_err() {
            return; // no usable credential store here; nothing to assert
        }
        assert_eq!(secret_lookup(name).as_deref(), Some("s3cr3t-value"));
        secret_delete(name).unwrap();
        assert!(secret_lookup(name).is_none());
    }
}
