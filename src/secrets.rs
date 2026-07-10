//! OS-keyring storage for connection passwords (Secret Service / KWallet on
//! Linux via the `keyring` crate).
//!
//! The config file never holds a password — only the connection URL, which
//! doubles as the keyring account name under the `dataview` service. When no
//! keyring is available every call degrades to an error the caller treats as
//! "fall back to session-only memory".

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use keyring::Entry;

const SERVICE: &str = "dataview";

/// Set `DATAVIEW_DISABLE_KEYRING=1` to force the session-only fallback
/// (useful headless and in the nested-X test setup, where talking to the
/// real desktop's wallet daemon would be intrusive).
fn disabled() -> bool {
    std::env::var_os("DATAVIEW_DISABLE_KEYRING").is_some_and(|v| v == "1")
}

/// Entries are cached per URL. Real backends don't need this, but the
/// `mock` backend used in tests stores state inside the Entry instance, so
/// repeated lookups must reuse the same one.
fn entry(url: &str) -> Result<Arc<Entry>, String> {
    if disabled() {
        return Err("keyring disabled by DATAVIEW_DISABLE_KEYRING".to_string());
    }
    static ENTRIES: OnceLock<Mutex<HashMap<String, Arc<Entry>>>> = OnceLock::new();
    let mut cache = ENTRIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("entry cache poisoned");
    if let Some(cached) = cache.get(url) {
        return Ok(cached.clone());
    }
    let created = Arc::new(Entry::new(SERVICE, url).map_err(|e| e.to_string())?);
    cache.insert(url.to_string(), created.clone());
    Ok(created)
}

/// Stores (or replaces) the password for a connection URL.
pub fn store_password(url: &str, password: &str) -> Result<(), String> {
    entry(url)?
        .set_password(password)
        .map_err(|e| e.to_string())
}

/// Fetches the stored password. `Ok(None)` when no entry exists; `Err` when
/// the keyring itself is unavailable.
pub fn get_password(url: &str) -> Result<Option<String>, String> {
    match entry(url)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

/// Removes the stored password. Idempotent: a missing entry is success.
pub fn delete_password(url: &str) -> Result<(), String> {
    match entry(url)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as TestMutex, Once};

    // The disable-flag test mutates process-wide env; serialize the module's
    // tests so it can't poison the round-trip test running in parallel.
    static TEST_LOCK: TestMutex<()> = TestMutex::new(());

    // Route all keyring calls to the crate's in-memory mock store so tests
    // never touch (or prompt for) the user's real wallet.
    fn use_mock() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
    }

    #[test]
    fn store_get_delete_round_trip() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_mock();
        let url = "postgres://u@h:5432/roundtrip";
        assert_eq!(get_password(url).unwrap(), None);
        store_password(url, "s3cret").unwrap();
        assert_eq!(get_password(url).unwrap(), Some("s3cret".to_string()));
        store_password(url, "changed").unwrap();
        assert_eq!(get_password(url).unwrap(), Some("changed".to_string()));
        delete_password(url).unwrap();
        assert_eq!(get_password(url).unwrap(), None);
        // Deleting again is fine.
        delete_password(url).unwrap();
    }

    #[test]
    fn disable_flag_forces_fallback_errors() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_mock();
        std::env::set_var("DATAVIEW_DISABLE_KEYRING", "1");
        let result = get_password("postgres://u@h/db");
        std::env::remove_var("DATAVIEW_DISABLE_KEYRING");
        assert!(result.is_err());
    }
}
