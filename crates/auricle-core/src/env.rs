//! Environment lookup for API keys and tokens.
//!
//! Process environment first. On Windows, falls back to the user's
//! persistent environment in the registry (`HKCU\Environment`) — that is
//! where `setx` writes, and processes launched from terminals that predate
//! the `setx` never see it in their own environment. Without the fallback,
//! a daemon started from such a shell reports keys as "not set" that the
//! user has, in fact, set.

/// Look up `name` in the process environment, then (Windows) the user's
/// persistent environment. Empty values count as unset.
pub fn env_lookup(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => return Some(v),
        _ => {}
    }
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment") {
            if let Ok(v) = key.get_value::<String, _>(name) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_env_wins() {
        // PATH exists in every test environment.
        let path = env_lookup("PATH").expect("PATH resolves");
        assert_eq!(path, std::env::var("PATH").unwrap());
    }

    #[test]
    fn missing_everywhere_is_none() {
        assert!(env_lookup("AURICLE_TEST_DEFINITELY_NOT_SET_ANYWHERE").is_none());
    }
}
