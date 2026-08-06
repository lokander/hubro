//! Integration tests for the query-history store, against a temp-dir
//! history database.

use hubro::history::{HistoryStore, HISTORY_CAP};

async fn temp_store() -> (tempfile::TempDir, HistoryStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = HistoryStore::open_at(&dir.path().join("nested").join("history.db"))
        .await
        .unwrap();
    (dir, store)
}

#[tokio::test]
async fn record_and_list_round_trips_newest_first() {
    let (_dir, store) = temp_store().await;
    assert!(store.record("db-a", "SELECT 1", true, None).await.unwrap());
    assert!(store
        .record("db-a", "SELECT broken", false, Some("no such table"))
        .await
        .unwrap());

    let entries = store.list("db-a", None, 50).await.unwrap();
    assert_eq!(entries.len(), 2);
    // Newest first.
    assert_eq!(entries[0].sql, "SELECT broken");
    assert!(!entries[0].success);
    assert_eq!(entries[0].error.as_deref(), Some("no such table"));
    assert_eq!(entries[1].sql, "SELECT 1");
    assert!(entries[1].success);
    assert_eq!(entries[1].error, None);
    assert!(entries[1].executed_at > 0);

    // Entries are scoped by locator.
    assert!(store.list("db-b", None, 50).await.unwrap().is_empty());
}

#[tokio::test]
async fn search_matches_substrings_and_escapes_wildcards() {
    let (_dir, store) = temp_store().await;
    store
        .record("db", "SELECT * FROM artists", true, None)
        .await
        .unwrap();
    store
        .record("db", "SELECT * FROM albums", true, None)
        .await
        .unwrap();
    store
        .record("db", "SELECT '100%' AS pct", true, None)
        .await
        .unwrap();

    let hits = store.list("db", Some("artists"), 50).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].sql, "SELECT * FROM artists");

    // Case-insensitive (SQLite LIKE default for ASCII).
    assert_eq!(
        store.list("db", Some("ARTISTS"), 50).await.unwrap().len(),
        1
    );

    // "%" is matched literally, not as a wildcard-match-everything.
    let percent = store.list("db", Some("100%"), 50).await.unwrap();
    assert_eq!(percent.len(), 1);
    assert_eq!(percent[0].sql, "SELECT '100%' AS pct");
    assert!(store
        .list("db", Some("100%x"), 50)
        .await
        .unwrap()
        .is_empty());

    // Blank search means no filter.
    assert_eq!(store.list("db", Some("  "), 50).await.unwrap().len(), 3);
    assert_eq!(store.list("db", None, 50).await.unwrap().len(), 3);

    // The limit applies.
    assert_eq!(store.list("db", None, 2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn history_is_capped_per_locator() {
    let (_dir, store) = temp_store().await;
    for i in 0..(HISTORY_CAP + 10) {
        store
            .record("big", &format!("SELECT {i}"), true, None)
            .await
            .unwrap();
    }
    store
        .record("small", "SELECT 'kept'", true, None)
        .await
        .unwrap();

    let entries = store.list("big", None, HISTORY_CAP + 100).await.unwrap();
    assert_eq!(entries.len() as i64, HISTORY_CAP);
    // The newest survive; the oldest were pruned.
    assert_eq!(entries[0].sql, format!("SELECT {}", HISTORY_CAP + 9));
    assert_eq!(entries.last().unwrap().sql, "SELECT 10");

    // Pruning one locator never touches another.
    assert_eq!(store.list("small", None, 50).await.unwrap().len(), 1);
}

#[tokio::test]
async fn clear_removes_only_the_given_locator() {
    let (_dir, store) = temp_store().await;
    store.record("db-a", "SELECT 1", true, None).await.unwrap();
    store.record("db-b", "SELECT 2", true, None).await.unwrap();

    store.clear("db-a").await.unwrap();
    assert!(store.list("db-a", None, 50).await.unwrap().is_empty());
    assert_eq!(store.list("db-b", None, 50).await.unwrap().len(), 1);
}

#[tokio::test]
async fn recording_opt_out_round_trips_and_skips_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");

    let store = HistoryStore::open_at(&path).await.unwrap();
    // Defaults to enabled.
    assert!(store.recording_enabled().await.unwrap());

    store.set_recording(false).await.unwrap();
    assert!(!store.recording_enabled().await.unwrap());
    // Disabled recording is a silent no-op that reports "not recorded".
    assert!(!store.record("db", "SELECT 1", true, None).await.unwrap());
    assert!(store.list("db", None, 50).await.unwrap().is_empty());
    drop(store);

    // The flag persists across re-opens of the same file.
    let reopened = HistoryStore::open_at(&path).await.unwrap();
    assert!(!reopened.recording_enabled().await.unwrap());

    reopened.set_recording(true).await.unwrap();
    assert!(reopened.recording_enabled().await.unwrap());
    assert!(reopened.record("db", "SELECT 1", true, None).await.unwrap());
    assert_eq!(reopened.list("db", None, 50).await.unwrap().len(), 1);
}

#[tokio::test]
async fn entries_survive_reopening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");

    let store = HistoryStore::open_at(&path).await.unwrap();
    store.record("db", "SELECT 42", true, None).await.unwrap();
    drop(store);

    let reopened = HistoryStore::open_at(&path).await.unwrap();
    let entries = reopened.list("db", None, 50).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].sql, "SELECT 42");
}
