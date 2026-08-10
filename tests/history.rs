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

// --- Saved queries (FRE-113) -------------------------------------------

use hubro::history::SaveOutcome;

#[tokio::test]
async fn saved_queries_are_scoped_per_connection_unless_global() {
    let (_dir, store) = temp_store().await;
    assert_eq!(
        store
            .save_query("Recent orders", None, "SELECT * FROM orders", Some("db-a"))
            .await
            .unwrap(),
        SaveOutcome::Created
    );
    store
        .save_query(
            "Table sizes",
            Some("Works anywhere"),
            "SELECT 1",
            None, // global
        )
        .await
        .unwrap();

    // The connection sees its own plus the global one, its own first.
    let for_a = store.saved_queries("db-a", None).await.unwrap();
    assert_eq!(
        for_a.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
        ["Recent orders", "Table sizes"]
    );
    assert_eq!(for_a[0].locator.as_deref(), Some("db-a"));
    assert_eq!(for_a[1].locator, None);
    assert_eq!(for_a[0].description, None);
    assert_eq!(for_a[1].description.as_deref(), Some("Works anywhere"));
    assert!(for_a[0].updated_at > 0);

    // Another connection sees only the global one — a query written for one
    // schema is not offered against a database that lacks it.
    let for_b = store.saved_queries("db-b", None).await.unwrap();
    assert_eq!(
        for_b.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
        ["Table sizes"]
    );
}

#[tokio::test]
async fn resaving_a_name_replaces_it_within_its_scope_only() {
    let (_dir, store) = temp_store().await;
    store
        .save_query("Counts", None, "SELECT 1", Some("db-a"))
        .await
        .unwrap();
    assert_eq!(
        store
            .save_query("Counts", Some("now with a note"), "SELECT 2", Some("db-a"))
            .await
            .unwrap(),
        SaveOutcome::Replaced
    );
    let saved = store.saved_queries("db-a", None).await.unwrap();
    assert_eq!(saved.len(), 1, "re-saving a name must not duplicate it");
    assert_eq!(saved[0].sql, "SELECT 2");
    assert_eq!(saved[0].description.as_deref(), Some("now with a note"));

    // The same name in another scope is a different query, not a conflict.
    assert_eq!(
        store
            .save_query("Counts", None, "SELECT 3", Some("db-b"))
            .await
            .unwrap(),
        SaveOutcome::Created
    );
    assert_eq!(
        store
            .save_query("Counts", None, "SELECT 4", None)
            .await
            .unwrap(),
        SaveOutcome::Created
    );
    assert_eq!(store.saved_queries("db-a", None).await.unwrap().len(), 2);
    assert_eq!(
        store.saved_queries("db-a", None).await.unwrap()[0].sql,
        "SELECT 2"
    );
}

#[tokio::test]
async fn saved_query_search_covers_name_description_and_sql() {
    let (_dir, store) = temp_store().await;
    store
        .save_query(
            "Orders",
            Some("everything unshipped"),
            "SELECT 1",
            Some("db"),
        )
        .await
        .unwrap();
    store
        .save_query("Artists", None, "SELECT * FROM artists", Some("db"))
        .await
        .unwrap();
    store
        .save_query("Percent", None, "SELECT '100%' AS pct", Some("db"))
        .await
        .unwrap();

    let by_name = store.saved_queries("db", Some("orde")).await.unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].name, "Orders");
    // Case-insensitive, like the history search.
    assert_eq!(
        store.saved_queries("db", Some("ORDE")).await.unwrap().len(),
        1
    );
    // The description is searched too — it is often the only place the word
    // you remember was ever written down.
    let by_description = store.saved_queries("db", Some("unshipped")).await.unwrap();
    assert_eq!(by_description.len(), 1);
    assert_eq!(by_description[0].name, "Orders");
    // And the SQL itself.
    let by_sql = store
        .saved_queries("db", Some("FROM artists"))
        .await
        .unwrap();
    assert_eq!(by_sql.len(), 1);
    assert_eq!(by_sql[0].name, "Artists");
    // LIKE wildcards in the needle are matched literally.
    let percent = store.saved_queries("db", Some("100%")).await.unwrap();
    assert_eq!(percent.len(), 1);
    assert_eq!(percent[0].name, "Percent");
    assert!(store
        .saved_queries("db", Some("100%x"))
        .await
        .unwrap()
        .is_empty());
    // A blank needle is no filter at all.
    assert_eq!(store.saved_queries("db", Some(" ")).await.unwrap().len(), 3);
}

#[tokio::test]
async fn deleting_a_saved_query_removes_only_that_one() {
    let (_dir, store) = temp_store().await;
    store
        .save_query("Keep", None, "SELECT 1", Some("db"))
        .await
        .unwrap();
    store
        .save_query("Drop", None, "SELECT 2", Some("db"))
        .await
        .unwrap();
    let doomed = store
        .saved_queries("db", Some("Drop"))
        .await
        .unwrap()
        .remove(0);

    store.delete_saved_query(doomed.id).await.unwrap();
    let left = store.saved_queries("db", None).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].name, "Keep");
    // Deleting the same id twice is not an error — the row is simply gone.
    store.delete_saved_query(doomed.id).await.unwrap();
}

#[tokio::test]
async fn saving_needs_a_name_and_something_to_save() {
    let (_dir, store) = temp_store().await;
    assert!(store
        .save_query("   ", None, "SELECT 1", Some("db"))
        .await
        .is_err());
    assert!(store
        .save_query("Empty", None, "  \n ", Some("db"))
        .await
        .is_err());
    assert!(store.saved_queries("db", None).await.unwrap().is_empty());

    // Surrounding whitespace is trimmed rather than stored, so " Name " and
    // "Name" are the same entry and not two that look identical in the list.
    store
        .save_query("  Name  ", Some("  "), "SELECT 1", Some("db"))
        .await
        .unwrap();
    assert_eq!(
        store
            .save_query("Name", None, "SELECT 2", Some("db"))
            .await
            .unwrap(),
        SaveOutcome::Replaced
    );
    let saved = store.saved_queries("db", None).await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].name, "Name");
    assert_eq!(saved[0].description, None);
}

#[tokio::test]
async fn saved_queries_ignore_the_recording_opt_out_and_survive_a_clear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let store = HistoryStore::open_at(&path).await.unwrap();

    // Recording off is about hubro logging what you ran; saving is something
    // the user asked for, so it still lands.
    store.set_recording(false).await.unwrap();
    store
        .save_query("Kept", None, "SELECT 1", Some("db"))
        .await
        .unwrap();
    assert_eq!(store.saved_queries("db", None).await.unwrap().len(), 1);

    // Clearing the connection's history leaves saved queries alone.
    store.set_recording(true).await.unwrap();
    store.record("db", "SELECT 99", true, None).await.unwrap();
    store.clear("db").await.unwrap();
    assert!(store.list("db", None, 50).await.unwrap().is_empty());
    assert_eq!(store.saved_queries("db", None).await.unwrap().len(), 1);
    drop(store);

    // And they survive a restart, like the rest of the store.
    let reopened = HistoryStore::open_at(&path).await.unwrap();
    let saved = reopened.saved_queries("db", None).await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].sql, "SELECT 1");
}

#[tokio::test]
async fn history_pruning_never_reaches_saved_queries() {
    let (_dir, store) = temp_store().await;
    store
        .save_query("Survivor", None, "SELECT 'kept'", Some("big"))
        .await
        .unwrap();
    for i in 0..(HISTORY_CAP + 5) {
        store
            .record("big", &format!("SELECT {i}"), true, None)
            .await
            .unwrap();
    }
    assert_eq!(store.saved_queries("big", None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn opening_a_pre_saved_query_database_upgrades_it_in_place() {
    // A history.db written before FRE-113: `entries`, its index and `meta`,
    // with real content. Opening it must add the saved-query table without
    // disturbing what is already there — the upgrade path every existing
    // install takes, and one no other test reaches, since every other test
    // starts from a file this build created.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let old = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for statement in [
            "CREATE TABLE entries (\
                 id INTEGER PRIMARY KEY, \
                 locator TEXT NOT NULL, \
                 sql TEXT NOT NULL, \
                 executed_at INTEGER NOT NULL, \
                 success INTEGER NOT NULL, \
                 error TEXT\
             )",
            "CREATE INDEX entries_by_locator ON entries(locator, id)",
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "INSERT INTO entries (locator, sql, executed_at, success, error) \
             VALUES ('db', 'SELECT 1', 1700000000, 1, NULL)",
            "INSERT INTO meta (key, value) VALUES ('recording', 'off')",
        ] {
            sqlx::query(statement).execute(&old).await.unwrap();
        }
        old.close().await;
    }

    let store = HistoryStore::open_at(&path).await.unwrap();

    // The old history and the old opt-out survived the upgrade.
    let entries = store.list("db", None, 50).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].sql, "SELECT 1");
    assert!(!store.recording_enabled().await.unwrap());

    // And the new table exists and works, including its uniqueness index —
    // a table created without it would take the second save as a new row.
    store
        .save_query("Kept", None, "SELECT 2", Some("db"))
        .await
        .unwrap();
    assert_eq!(
        store
            .save_query("Kept", None, "SELECT 3", Some("db"))
            .await
            .unwrap(),
        SaveOutcome::Replaced
    );
    let saved = store.saved_queries("db", None).await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].sql, "SELECT 3");
}
