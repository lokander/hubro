//! OS-keyring storage for connection passwords via the `keyring` crate
//! (Secret Service / KWallet on Linux, Keychain on macOS, Credential
//! Manager on Windows — see the target-specific sections in Cargo.toml).
//!
//! The config file never holds a password — only the connection URL, which
//! doubles as the keyring account name under the `hubro` service. When no
//! keyring is available every call degrades to an error the caller treats as
//! "fall back to session-only memory".

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use keyring::Entry;

/// Runs a keyring operation off the UI thread. Secret Service calls can
/// block indefinitely (e.g. waiting for the user to unlock the wallet), and
/// Dioxus polls its tasks on the main thread — so blocking here would freeze
/// the whole window. Falls back to calling inline when no tokio runtime is
/// available (plain unit tests).
async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => match handle.spawn_blocking(f).await {
            Ok(result) => result,
            Err(join_err) => Err(format!("keyring task failed: {join_err}")),
        },
        Err(_) => f(),
    }
}

/// Async wrappers for use from UI tasks — see [`run_blocking`].
pub async fn store_password_async(url: String, password: String) -> Result<(), String> {
    run_blocking(move || store_password(&url, &password)).await
}

pub async fn get_password_async(url: String) -> Result<Option<String>, String> {
    run_blocking(move || get_password(&url)).await
}

pub async fn delete_password_async(url: String) -> Result<(), String> {
    run_blocking(move || delete_password(&url)).await
}

const SERVICE: &str = "hubro";

/// Set `HUBRO_DISABLE_KEYRING=1` to force the session-only fallback
/// (useful headless and in the nested-X test setup, where talking to the
/// real desktop's wallet daemon would be intrusive).
fn disabled() -> bool {
    std::env::var_os("HUBRO_DISABLE_KEYRING").is_some_and(|v| v == "1")
}

/// Entries are cached per URL. Real backends don't need this, but the
/// `mock` backend used in tests ([`new_entry`]) stores state inside the Entry
/// instance, so repeated lookups must reuse the same one.
fn entry(url: &str) -> Result<Arc<Entry>, String> {
    if disabled() {
        return Err("keyring disabled by HUBRO_DISABLE_KEYRING".to_string());
    }
    static ENTRIES: OnceLock<Mutex<HashMap<String, Arc<Entry>>>> = OnceLock::new();
    let mut cache = ENTRIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        // The map itself can't be left inconsistent; recover from poisoning.
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = cache.get(url) {
        return Ok(cached.clone());
    }
    let created = Arc::new(new_entry(url)?);
    cache.insert(url.to_string(), created.clone());
    Ok(created)
}

/// The keyring entry for `url`: in the OS's credential store in the app, and
/// **always in keyring's in-memory mock in this crate's unit tests**.
///
/// The mock is chosen here, per entry, rather than by installing it as the
/// process-wide default builder: that install happened only in the tests that
/// asked for it, so any other test that reached the keyring — a connect
/// through a test's `AppState`, say — wrote the developer's real wallet or
/// not depending on which test ran first. Chosen here, no unit test has a
/// route to the real store whatever the order. `tests/` links the library
/// without `cfg(test)`, so `no_integration_test_reaches_the_keyring` keeps
/// it from naming this module at all.
fn new_entry(url: &str) -> Result<Entry, String> {
    if cfg!(test) {
        let credential = keyring::mock::default_credential_builder()
            .build(None, SERVICE, url)
            .map_err(|e| e.to_string())?;
        return Ok(Entry::new_with_credential(credential));
    }
    Entry::new(SERVICE, url).map_err(|e| e.to_string())
}

/// Stores (or replaces) the password for a connection URL.
///
/// Note: entries are keyed by the exact URL, so re-adding the same server
/// with a different port/user/sslmode creates a distinct entry; the old one
/// lingers until its saved connection is removed.
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
pub(crate) mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Mutex as TestMutex, MutexGuard};

    // The disable-flag test mutates process-wide env; serialize the module's
    // tests so it can't poison the round-trip test running in parallel.
    static TEST_LOCK: TestMutex<()> = TestMutex::new(());

    /// Holds the module's lock with the disable flag cleared, for as long as
    /// the guard lives — for tests elsewhere that drive a flow through the
    /// keyring and need it to answer. Every unit test's keyring is the mock
    /// whether or not it holds this ([`new_entry`]); what it buys is that no
    /// other test sets the disable flag mid-flow.
    pub(crate) fn mock_keyring() -> MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("HUBRO_DISABLE_KEYRING");
        guard
    }

    /// The guard: in a unit test, an entry is the mock's — asked for first,
    /// by a test that installed nothing, so no test order can make it real.
    #[test]
    fn a_unit_test_reaches_only_the_mock_store() {
        let _guard = mock_keyring();
        let entry = entry("postgres://u@h:5432/which-store").unwrap();
        assert!(
            entry
                .get_credential()
                .downcast_ref::<keyring::mock::MockCredential>()
                .is_some(),
            "a unit test reached a real credential store"
        );
    }

    /// `tests/` links the library without `cfg(test)`, where [`new_entry`]
    /// is the OS's store, so no integration test may name this module or the
    /// keyring crate. The other route there — a connect through an
    /// `AppState` — is closed by `no_integration_test_names_the_real_paths`.
    /// Words inside string literals don't count: the help text's test asserts
    /// that the help *mentions* the keyring.
    #[test]
    fn no_integration_test_reaches_the_keyring() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let found =
            crate::config::tests::words_named_in(&root.join("tests"), &["secrets", "keyring"]);
        assert!(
            found.is_empty(),
            "these reach the user's real keyring; drive the keyring from a \
             unit test, where it is the mock:\n{}",
            found.join("\n")
        );
    }

    #[test]
    fn store_get_delete_round_trip() {
        let _guard = TEST_LOCK.lock().unwrap();
        // Hermetic even when the developer's shell exports the disable flag
        // (the headless test recipe suggests it).
        std::env::remove_var("HUBRO_DISABLE_KEYRING");
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

    /// Round-trip against the real OS credential store (Keychain on macOS,
    /// Credential Manager on Windows, Secret Service on Linux), run by hand:
    /// `cargo test real_store_round_trip -- --ignored`. The one deliberate
    /// route to the real store in this crate's tests, so it builds its entry
    /// with `Entry::new` itself — [`entry`] is the mock here — and uses a
    /// service name of its own, never touching an entry hubro stored.
    #[test]
    #[ignore = "touches the real OS credential store; run alone, manually"]
    fn real_store_round_trip() {
        let entry = Entry::new("hubro-real-store-test", "postgres://hubro-test@localhost/x")
            .expect("the platform's credential store");
        let get = || match entry.get_password() {
            Ok(password) => Some(password),
            Err(keyring::Error::NoEntry) => None,
            Err(err) => panic!("{err}"),
        };
        let _ = entry.delete_credential();
        assert_eq!(get(), None);
        entry.set_password("s3cret-real").unwrap();
        assert_eq!(get().as_deref(), Some("s3cret-real"));
        entry.set_password("changed-real").unwrap();
        assert_eq!(get().as_deref(), Some("changed-real"));
        entry.delete_credential().unwrap();
        assert_eq!(get(), None);
    }

    #[test]
    fn disable_flag_forces_fallback_errors() {
        let _guard = TEST_LOCK.lock().unwrap();
        std::env::set_var("HUBRO_DISABLE_KEYRING", "1");
        let result = get_password("postgres://u@h/db");
        std::env::remove_var("HUBRO_DISABLE_KEYRING");
        assert!(result.is_err());
    }
}
