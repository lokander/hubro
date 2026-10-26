//! Per-connection bookkeeping: [`TabState`], which a tab's close drops in one
//! `remove`, and [`Slot`], the stale-run guard every background operation
//! (a SQL run, an export, an import, a schema edit) records its outcome
//! through.
//!
//! Before these, each feature added a status map, a generation map and a
//! task map to `AppState`, plus a `retain` or `remove` line for each in
//! `close_connection`; a map whose line was forgotten kept the closed tab's
//! entries for the app's lifetime, and nothing could test for it. Now a new
//! piece of per-connection state either goes in [`TabState`] — and is dropped
//! with the tab by construction — or is a field of `AppState` that
//! `every_per_connection_field_is_in_tabs_or_says_why_not` makes name its
//! reason and its teardown.

use super::*;

/// Everything about one open connection that is written at the rate of a
/// user's gestures — a connect, a foreign-key jump, an export started, a
/// query saved — rather than per row, statement or keystroke.
///
/// One signal holds every tab's, so a component reading any field here
/// re-renders on a write to any tab's: that is why only gesture-rate state
/// belongs here. What is written per statement (`sql_runs`), per edit
/// (`staged`), on every run start (`pending_sql`), or read by every pane's
/// render (`schemas`, `tab_ui`) stays a signal of its own, with its reason
/// recorded in `OUTSIDE_TABS` below.
///
/// An entry exists only for a connection in the registry: writes go through
/// [`AppState::tab_mut`], which drops a write for a connection that has
/// closed — so a background task finishing after its tab closed cannot
/// recreate the entry the close removed.
#[derive(Default)]
pub struct TabState {
    /// The connection's live SSH tunnel, if it has one. Dropping the
    /// [`Tunnel`] (with the whole entry, on close) shuts the forward down.
    pub tunnel: Option<Tunnel>,
    /// Set once this connection's SSH session has ended under it, with what
    /// to tell the user. Nothing can reach the database through such a tunnel
    /// any more, but the pool cannot know that, so without this the tab would
    /// go on looking connected while every query failed with a driver error.
    pub lost_tunnel: Option<String>,
    /// The connection's accent colour, seeded from the saved entry at connect
    /// time.
    ///
    /// Kept beside the registry rather than inside it because a colour warns
    /// and never enforces: nothing in `db/` should be able to read it and act
    /// on it. Its enforcing counterpart, [`WriteProtection`], lives on
    /// [`Connection`] for the opposite reason.
    pub color: Option<ConnectionColor>,
    /// Outcome of the most recent saved-query write from this connection's
    /// panel: one connection's "Saved" line has no business appearing in
    /// another connection's panel.
    pub saved_status: Option<SavedStatus>,
    /// The result this connection's latest SQL-pane export was taken from, so
    /// only that result's section shows the status ([`sql_export_status`]).
    pub sql_export_source: Option<SqlExportSource>,
    /// The table this connection's latest grid export was taken from, so the
    /// line names it under any other table ([`grid_export_line`]).
    pub grid_export_table: Option<TableRef>,
    /// Pending foreign-key focus: a target table plus the filter to seed. Set
    /// by [`AppState::navigate_fk`] / [`AppState::navigate_back`] right before
    /// the target grid is selected; the grid consumes it when it matches its
    /// table (on mount, or live for a same-table jump) and clears it.
    pub pending_focus: Option<FocusTarget>,
    /// The Back stack: the views left behind by forward foreign-key jumps,
    /// most recent last. [`AppState::navigate_back`] pops one; a manual table
    /// selection clears it.
    pub nav_history: Vec<FocusTarget>,
    /// Staged saves waiting on the write-protection confirmation, one per
    /// table (by [`TableRef::key`]), each holding the exact change list the
    /// prompt named.
    ///
    /// Only populated for connections marked [`WriteProtection::Confirm`];
    /// an unmarked connection saves straight through. The staged changes
    /// themselves stay in `AppState::staged` — this holds only a copy, so
    /// dismissing loses nothing, and the copy is what makes a confirmation
    /// stale once the stage moves on (see `save_action`). Armed, so a press
    /// straight after the one that parked it does not confirm it.
    pub pending_saves: HashMap<String, ParkedSave>,
}

/// One kind of background operation's bookkeeping, per key: the latest
/// status, the generation of the run that owns the key, and the handle of a
/// run still in flight.
///
/// The generation is the stale-run guard. Every [`Self::begin`] takes a new
/// one, so a run that was superseded — or whose key was [closed](Self::close)
/// — learns so from [`Self::finish`] and drops its outcome rather than
/// writing it over a newer run's. Generations come from one counter per slot
/// and are never reused, even across a close, so a run from before a close
/// can never match a run begun after it.
///
/// Plain data, so the decision is a test away; `AppState` keeps each slot in
/// a signal of its own. It reads as its status map ([`Deref`]), which is what
/// every render path wants; writes go through the methods.
///
/// [`Deref`]: std::ops::Deref
pub struct Slot<K, S, T = Task> {
    next_generation: u64,
    generations: HashMap<K, u64>,
    statuses: HashMap<K, S>,
    tasks: HashMap<K, T>,
}

impl<K, S, T> Default for Slot<K, S, T> {
    fn default() -> Self {
        Self {
            next_generation: 0,
            generations: HashMap::new(),
            statuses: HashMap::new(),
            tasks: HashMap::new(),
        }
    }
}

impl<K, S, T> std::ops::Deref for Slot<K, S, T> {
    type Target = HashMap<K, S>;

    fn deref(&self) -> &HashMap<K, S> {
        &self.statuses
    }
}

impl<K: Eq + std::hash::Hash + Clone, S, T> Slot<K, S, T> {
    /// Starts a new run in `key` with `status`, and returns its generation
    /// together with the handle of the run it superseded, if one was still in
    /// flight. The caller decides what becomes of that run: a SQL run cancels
    /// it, while an import or schema edit has always let it finish unobserved.
    #[must_use]
    pub fn begin(&mut self, key: K, status: S) -> (u64, Option<T>) {
        self.next_generation += 1;
        let generation = self.next_generation;
        self.generations.insert(key.clone(), generation);
        self.statuses.insert(key.clone(), status);
        (generation, self.tasks.remove(&key))
    }

    /// Records the handle of the run [`Self::begin`] just started, so a close
    /// (or a cancel) can stop it.
    pub fn attach(&mut self, key: K, task: T) {
        self.tasks.insert(key, task);
    }

    /// Whether the run of `generation` still owns `key`: no newer run has
    /// begun there, and the key has not been closed.
    pub fn is_current(&self, key: &K, generation: u64) -> bool {
        self.generations.get(key) == Some(&generation)
    }

    /// Ends the run of `generation` in `key`. True when it still owns the
    /// key — its handle is released and the caller records its outcome;
    /// false when it was superseded or closed, and its outcome must be
    /// dropped.
    #[must_use]
    pub fn finish(&mut self, key: &K, generation: u64) -> bool {
        if !self.is_current(key, generation) {
            return false;
        }
        self.tasks.remove(key);
        true
    }

    /// The status of the run of `generation`, while it still owns `key` —
    /// for a run reporting progress before it finishes.
    pub fn current_mut(&mut self, key: &K, generation: u64) -> Option<&mut S> {
        if !self.is_current(key, generation) {
            return None;
        }
        self.statuses.get_mut(key)
    }

    /// Replaces `key`'s status without touching its run.
    pub fn set(&mut self, key: K, status: S) {
        self.statuses.insert(key, status);
    }

    /// `key`'s status, to amend in place.
    pub fn status_mut(&mut self, key: &K) -> Option<&mut S> {
        self.statuses.get_mut(key)
    }

    /// Clears `key`'s status line; a run still in flight keeps the key.
    pub fn clear(&mut self, key: &K) {
        self.statuses.remove(key);
    }

    /// Takes the in-flight run's handle out of `key`, for the caller to
    /// cancel.
    pub fn take_task(&mut self, key: &K) -> Option<T> {
        self.tasks.remove(key)
    }

    /// Forgets every key `pred` matches — status, generation and handle — and
    /// returns the handles of the runs still in flight there, for the caller
    /// to cancel. Any of those runs that is not cancelled finds itself stale
    /// at [`Self::finish`].
    #[must_use]
    pub fn close(&mut self, pred: impl Fn(&K) -> bool) -> Vec<T> {
        self.generations.retain(|key, _| !pred(key));
        self.statuses.retain(|key, _| !pred(key));
        let keys: Vec<K> = self.tasks.keys().filter(|key| pred(key)).cloned().collect();
        keys.iter()
            .filter_map(|key| self.tasks.remove(key))
            .collect()
    }

    /// Whether the slot holds nothing at all for keys `pred` matches.
    #[cfg(test)]
    pub fn holds_nothing_for(&self, pred: impl Fn(&K) -> bool) -> bool {
        !self.generations.keys().any(&pred)
            && !self.statuses.keys().any(&pred)
            && !self.tasks.keys().any(&pred)
    }
}

impl AppState {
    /// Connection `id`'s [`TabState`], read through `f` — `None` when the tab
    /// is not open. Subscribes the caller to every tab's state; see
    /// [`TabState`] for why that is cheap.
    pub fn tab<R>(&self, id: ConnectionId, f: impl FnOnce(&TabState) -> R) -> Option<R> {
        self.tabs.read().get(&id).map(f)
    }

    /// [`Self::tab`] without subscribing — for event handlers and tasks.
    pub fn tab_peek<R>(&self, id: ConnectionId, f: impl FnOnce(&TabState) -> R) -> Option<R> {
        self.tabs.peek().get(&id).map(f)
    }

    /// Writes connection `id`'s [`TabState`] through `f`, creating it on the
    /// first write — but only while `id` is in the registry. A write for a
    /// connection that is not open is dropped: a task finishing after its tab
    /// closed would otherwise recreate the entry the close had just removed,
    /// and hold it for the app's lifetime.
    pub fn tab_mut<R>(self, id: ConnectionId, f: impl FnOnce(&mut TabState) -> R) -> Option<R> {
        let open = self.registry.peek().get(id).is_some();
        if !open {
            return None;
        }
        let mut tabs = self.tabs;
        let mut tabs = tabs.write();
        Some(f(tabs.entry(id).or_default()))
    }

    /// Connection `id`'s lost-tunnel message, if its SSH session has ended.
    pub fn lost_tunnel(&self, id: ConnectionId) -> Option<String> {
        self.tab(id, |tab| tab.lost_tunnel.clone()).flatten()
    }

    /// The outcome of connection `id`'s latest saved-query write.
    pub fn saved_status(&self, id: ConnectionId) -> Option<SavedStatus> {
        self.tab(id, |tab| tab.saved_status.clone()).flatten()
    }

    /// Clears connection `id`'s saved-query line.
    pub fn clear_saved_status(self, id: ConnectionId) {
        self.tab_mut(id, |tab| tab.saved_status = None);
    }

    /// The source of connection `id`'s latest SQL-pane export.
    pub fn sql_export_source(&self, id: ConnectionId) -> Option<SqlExportSource> {
        self.tab(id, |tab| tab.sql_export_source.clone()).flatten()
    }

    /// The table connection `id`'s latest grid export was taken from.
    pub fn grid_export_table(&self, id: ConnectionId) -> Option<TableRef> {
        self.tab(id, |tab| tab.grid_export_table.clone()).flatten()
    }

    /// Connection `id`'s pending foreign-key focus. Subscribes; use
    /// [`Self::tab_peek`] where that is not wanted.
    pub fn pending_focus(&self, id: ConnectionId) -> Option<FocusTarget> {
        self.tab(id, |tab| tab.pending_focus.clone()).flatten()
    }

    /// Clears connection `id`'s pending foreign-key focus, once consumed.
    pub fn clear_pending_focus(self, id: ConnectionId) {
        self.tab_mut(id, |tab| tab.pending_focus = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `AppState` field keyed by connection that is not in
    /// [`TabState`], with why it stays out. Each must also be torn down in
    /// `close_connection`, which the test below checks.
    const OUTSIDE_TABS: &[(&str, &str)] = &[
        (
            "registry",
            "the connections themselves; the tab's existence, which TabState hangs off",
        ),
        (
            "open_locators",
            "an ordered list searched by locator across every tab, not looked up by id",
        ),
        (
            "schemas",
            "read by the sidebar, grid and editor renders of every pane",
        ),
        (
            "tab_ui",
            "the selection and pane every render of the tab reads; hot path",
        ),
        ("staged", "written on every cell edit"),
        (
            "grid_refresh",
            "a nonce whose bump must wake the grids, read through a per-table memo; in \
             TabState every bump would wake every TabState reader too",
        ),
        (
            "sql_runs",
            "a Slot whose status is written once per statement of a running script",
        ),
        (
            "pending_sql",
            "written on every run start, beside sql_runs; in TabState every run would wake \
             every TabState reader",
        ),
        (
            "export_status",
            "a Slot: the stale-run guard is the same for every slot, keyed across connections",
        ),
        (
            "import_status",
            "a Slot: the stale-run guard is the same for every slot, keyed across connections",
        ),
        (
            "schema_edits",
            "a Slot: the stale-run guard is the same for every slot, keyed across connections",
        ),
    ];

    /// The `AppState` fields whose type names a connection id, with their
    /// types: one entry per field, continuation lines of a wrapped type
    /// joined on.
    fn per_connection_fields() -> Vec<(String, String)> {
        let source = mod_rs_code();
        let body: Vec<&str> = source
            .lines()
            .skip_while(|line| line.trim() != "pub struct AppState {")
            .skip(1)
            .take_while(|line| *line != "}")
            .collect();
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in body {
            let Some(rest) = line.strip_prefix("    ") else {
                continue;
            };
            if rest.trim().is_empty() {
                continue;
            }
            let starts_field = !rest.starts_with(' ');
            let rest = rest.strip_prefix("pub ").unwrap_or(rest);
            match rest.split_once(':') {
                Some((name, ty))
                    if starts_field
                        && !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') =>
                {
                    fields.push((name.to_string(), ty.trim().to_string()));
                }
                _ => {
                    if let Some(last) = fields.last_mut() {
                        last.1.push_str(rest.trim());
                    }
                }
            }
        }
        assert!(
            fields.len() > 30,
            "the AppState definition was not found: {} fields",
            fields.len()
        );
        fields
            .into_iter()
            .filter(|(_, ty)| ty.contains("ConnectionId") || ty.contains("ConnectionRegistry"))
            .collect()
    }

    /// The body of `close_connection`, comments removed.
    fn close_connection_body() -> String {
        let source = mod_rs_code();
        let start = source
            .find("pub fn close_connection(")
            .expect("close_connection is in mod.rs");
        let rest = &source[start..];
        let end = crate::source_scan::block_end(rest).expect("close_connection ends");
        rest[..end].to_string()
    }

    /// `mod.rs` as shipped, comments removed.
    fn mod_rs_code() -> String {
        crate::source_scan::without_comments(&crate::source_scan::shipped(include_str!("mod.rs")))
    }

    /// Closing a tab drops all of its per-connection state: what is in
    /// `tabs` by construction, and everything else because this fails until
    /// a new per-connection field either moves into [`TabState`] or names its
    /// reason in [`OUTSIDE_TABS`] and is torn down in `close_connection`.
    #[test]
    fn every_per_connection_field_is_in_tabs_or_says_why_not() {
        let fields = per_connection_fields();
        let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
        assert!(
            names.contains(&"tabs") && names.contains(&"registry"),
            "the field scan found {names:?}"
        );
        let close = close_connection_body();
        assert!(
            close.contains("self.tabs.write().remove(&id)"),
            "close_connection no longer drops the tab's TabState"
        );
        for (name, ty) in &fields {
            if name == "tabs" {
                continue;
            }
            let Some((_, reason)) = OUTSIDE_TABS.iter().find(|(listed, _)| listed == name) else {
                panic!(
                    "AppState::{name} ({ty}) is per-connection state outside TabState with no \
                     recorded reason: move it into TabState, or say why it stays out in \
                     OUTSIDE_TABS and tear it down in close_connection"
                );
            };
            assert!(!reason.trim().is_empty(), "{name} has an empty reason");
            assert!(
                close.contains(&format!("self.{name}")),
                "close_connection does not tear down AppState::{name}"
            );
        }
        // No stale entries: each listed name is still a per-connection field.
        for (listed, _) in OUTSIDE_TABS {
            assert!(
                names.contains(listed),
                "OUTSIDE_TABS lists {listed}, which is no longer a per-connection field"
            );
        }
    }

    /// Through the real `close_connection`: one connection's state in
    /// `tabs` and in every slot goes with its tab, the other's stays, and a
    /// write arriving after the close — a background task finishing late —
    /// does not bring the closed tab's entry back.
    #[tokio::test]
    async fn closing_a_tab_drops_its_state_and_a_late_write_cannot_restore_it() {
        use dioxus::core::VirtualDom;

        use crate::ui::notice::Gesture;
        use crate::ui::test_dom::provide_state;

        let dom = VirtualDom::new(|| rsx! {});
        let state = provide_state(&dom);
        let pool =
            || DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        dom.in_runtime(|| {
            let mut registry = state.registry;
            let closed = registry
                .write()
                .insert("a.db", pool(), WriteProtection::Open);
            let kept = registry
                .write()
                .insert("b.db", pool(), WriteProtection::Open);
            let table = TableRef {
                schema: None,
                name: "t".into(),
            };
            for id in [closed, kept] {
                state.tab_mut(id, |tab| {
                    tab.lost_tunnel = Some("lost".into());
                    tab.saved_status = Some(SavedStatus::Failed("x".into()));
                    tab.grid_export_table = Some(table.clone());
                    tab.nav_history.push(FocusTarget {
                        table: table.clone(),
                        filter: None,
                    });
                });
                let run = SqlRun {
                    statements: Vec::new(),
                    status: RunStatus::Running,
                    explain: false,
                };
                let (mut runs, mut exports) = (state.sql_runs, state.export_status);
                let (mut imports, mut edits) = (state.import_status, state.schema_edits);
                let _ = runs.write().begin((id, 1), run);
                let _ = exports
                    .write()
                    .begin((id, ExportPane::Grid), ExportStatus::Running);
                let _ = imports
                    .write()
                    .begin(id, ImportStatus::Running { table: "t".into() });
                let _ = edits
                    .write()
                    .begin(id, SchemaEditStatus::Running { label: "x".into() });
            }

            state.close_connection(closed, Gesture::CLICK);

            let tabs = state.tabs.peek();
            assert!(!tabs.contains_key(&closed));
            assert!(tabs.contains_key(&kept));
            drop(tabs);
            assert!(state
                .sql_runs
                .peek()
                .holds_nothing_for(|(conn, _)| *conn == closed));
            assert!(state
                .export_status
                .peek()
                .holds_nothing_for(|(conn, _)| *conn == closed));
            assert!(state
                .import_status
                .peek()
                .holds_nothing_for(|conn| *conn == closed));
            assert!(state
                .schema_edits
                .peek()
                .holds_nothing_for(|conn| *conn == closed));
            assert!(state.sql_runs.peek().contains_key(&(kept, 1)));
            assert!(state.import_status.peek().contains_key(&kept));
            assert!(state.lost_tunnel(kept).is_some());

            // What a saved-query write finishing after the close does.
            assert_eq!(state.tab_mut(closed, |tab| tab.saved_status = None), None);
            assert!(!state.tabs.peek().contains_key(&closed));
        });
    }

    #[test]
    fn a_superseded_or_closed_run_cannot_record_its_outcome() {
        let mut slot: Slot<(u64, u64), &str, &str> = Slot::default();
        let key = (1, 0);
        let (first, superseded) = slot.begin(key, "running first");
        assert_eq!(superseded, None);
        slot.attach(key, "first's task");
        let (second, superseded) = slot.begin(key, "running second");
        // The newer run takes the older one's handle, for the caller to stop.
        assert_eq!(superseded, Some("first's task"));
        slot.attach(key, "second's task");

        // The superseded run finishing: stale, so its outcome is dropped and
        // the newer run's handle is left alone.
        assert!(!slot.is_current(&key, first));
        assert!(!slot.finish(&key, first));
        assert!(slot.current_mut(&key, first).is_none());
        assert_eq!(slot.get(&key), Some(&"running second"));
        assert_eq!(slot.take_task(&key), Some("second's task"));
        slot.attach(key, "second's task");

        // The current run: its progress lands, and finishing releases its
        // handle.
        *slot.current_mut(&key, second).expect("current") = "progress";
        assert_eq!(slot.get(&key), Some(&"progress"));
        assert!(slot.finish(&key, second));
        assert_eq!(slot.take_task(&key), None);

        // Another key is its own slot: a run there neither supersedes nor is
        // superseded by this one.
        let other = (2, 0);
        let (elsewhere, _) = slot.begin(other, "elsewhere");
        slot.attach(other, "elsewhere's task");
        assert!(slot.is_current(&other, elsewhere));
        assert!(slot.is_current(&key, second));

        // Closing connection 2 hands back its in-flight handle and forgets
        // everything about it, so the run is stale if it ever finishes — and
        // stays stale after a new run begins in the same key, because
        // generations are never reused.
        assert_eq!(slot.close(|(conn, _)| *conn == 2), vec!["elsewhere's task"]);
        assert!(slot.holds_nothing_for(|(conn, _)| *conn == 2));
        assert!(!slot.finish(&other, elsewhere));
        let (after_close, _) = slot.begin(other, "after close");
        assert_ne!(after_close, elsewhere);
        assert!(!slot.finish(&other, elsewhere));
        assert_eq!(slot.get(&other), Some(&"after close"));
        // Connection 1 was not touched.
        assert_eq!(slot.get(&key), Some(&"progress"));
        assert!(slot.is_current(&key, second));
    }
}
