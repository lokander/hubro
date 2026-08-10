//! Session persistence (FRE-30): snapshotting the open tabs to
//! `session.toml` and restoring them on the next launch.
//!
//! Restore deliberately drives the ordinary connect flow rather than a
//! shortcut of its own, so a restored tab is indistinguishable from one the
//! user opened by hand — including its prompts.

use super::*;

impl AppState {
    /// Snapshots the current session (FRE-30): the open tabs in tab order,
    /// each with its selected table and pane, plus the active tab's locator.
    /// Cheap and pure — the SQL editor buffer is deliberately not included, so
    /// per-keystroke `tab_ui` changes produce an identical snapshot and the
    /// persistence effect skips the write.
    pub fn current_session(&self) -> Session {
        let open = self.open_locators.read();
        let tab_ui = self.tab_ui.read();
        let mut tabs = Vec::with_capacity(open.len());
        for (id, locator) in open.iter() {
            let (schema, table, pane, row_detail) = match tab_ui.get(id) {
                Some(ui) => (
                    ui.selected_table.as_ref().and_then(|t| t.schema.clone()),
                    ui.selected_table.as_ref().map(|t| t.name.clone()),
                    ui.pane.to_session(),
                    ui.row_detail,
                ),
                None => (None, None, SessionPane::default(), false),
            };
            tabs.push(SessionTab {
                locator: locator.clone(),
                selected_schema: schema,
                selected_table: table,
                pane,
                row_detail,
            });
        }
        let active = match *self.active.read() {
            ActiveView::Connection(id) => open
                .iter()
                .find(|(open_id, _)| *open_id == id)
                .map(|(_, locator)| locator.clone()),
            ActiveView::Connections => None,
        };
        Session {
            tabs,
            active,
            // Read (not peeked) on purpose: the persistence effect re-runs on
            // the signals this function reads, so collapsing a group is what
            // schedules the write that remembers it (FRE-120).
            collapsed_groups: self.collapsed_groups.read().clone(),
        }
    }

    /// Persists the current session (best-effort, off the UI path). Called by
    /// the persistence effect only when the snapshot actually changed.
    pub fn persist_session(&self) {
        let Some(path) = default_session_path() else {
            return;
        };
        let session = self.current_session();
        spawn_forever(async move {
            let _ = save_session(&path, &session);
        });
    }

    /// Reopens the previous session's tabs (FRE-30). Runs once at startup.
    ///
    /// Only connections still in the saved list are reopened (ad-hoc ones are
    /// not resurrected). SQLite reconnects silently; Postgres reconnects only
    /// when its password is already available (session memory or keyring), so
    /// startup never raises a wall of password prompts — a saved Postgres
    /// connection whose password isn't stored is simply left for the user to
    /// click. A locator whose file/server is gone just fails to reopen through
    /// the normal connect-error path; it never blocks the rest of the restore.
    pub async fn restore_session(mut self) {
        let Some(path) = default_session_path() else {
            return;
        };
        let session = load_session(&path);
        // Before the early return: which groups were folded is remembered
        // whether or not any tab was open (FRE-120).
        if !session.collapsed_groups.is_empty() {
            self.collapsed_groups.set(session.collapsed_groups.clone());
        }
        if session.tabs.is_empty() {
            return;
        }
        // Snapshot the saved list in canonical open-locator form up front.
        let saved: Vec<SavedConnection> = self.saved.read().entries().to_vec();
        let candidates: Vec<RestoreCandidate> = saved
            .iter()
            .map(|s| RestoreCandidate {
                locator: saved_open_locator(s),
                backend: s.backend(),
            })
            .collect();
        // Which server-backend locators (Postgres, SQL Server) can connect
        // silently, so startup never blocks on a prompt or pops a browser?
        // Password auth needs a stored/session password; Entra managed
        // identity always can; Entra interactive only with a cached refresh
        // token. The keyring reads are off-thread and the session-memory
        // borrow is dropped before the await.
        let mut ready_locators: HashSet<String> = HashSet::new();
        for candidate in &candidates {
            match candidate.backend {
                // SQLite needs no credentials; plan_session_restore always
                // keeps it.
                BackendKind::Sqlite => continue,
                // Both fall through to the auth-availability check below.
                BackendKind::Postgres | BackendKind::SqlServer => {}
            }
            let auth = saved.iter().find_map(|s| match s {
                SavedConnection::Postgres { auth, .. }
                | SavedConnection::SqlServer { auth, .. }
                    if saved_open_locator(s) == candidate.locator =>
                {
                    Some(auth.clone())
                }
                _ => None,
            });
            let ready = match auth {
                Some(ServerAuth::Entra(entra)) => {
                    let has_refresh =
                        crate::secrets::get_password_async(entra_secret_key(&candidate.locator))
                            .await
                            .ok()
                            .flatten()
                            .is_some();
                    entra.can_acquire_silently(has_refresh)
                }
                _ => {
                    let in_session = self
                        .session_passwords
                        .read()
                        .contains_key(&candidate.locator);
                    in_session
                        || crate::secrets::get_password_async(candidate.locator.clone())
                            .await
                            .ok()
                            .flatten()
                            .is_some()
                }
            };
            if ready {
                ready_locators.insert(candidate.locator.clone());
            }
        }
        let plan = plan_session_restore(&session.tabs, &candidates, |loc| {
            ready_locators.contains(loc)
        });
        for tab in &plan {
            let saved_conn = saved
                .iter()
                .find(|s| saved_open_locator(s) == tab.locator)
                .cloned();
            let Some(saved_conn) = saved_conn else {
                continue;
            };
            let backend = ServerBackend::of(saved_conn.backend());
            match saved_conn {
                SavedConnection::Sqlite { path, .. } => self.connect(path).await,
                SavedConnection::Postgres {
                    url,
                    name,
                    tunnel,
                    auth,
                    ..
                }
                | SavedConnection::SqlServer {
                    url,
                    name,
                    tunnel,
                    auth,
                    ..
                } => self.connect_server(backend, url, name, tunnel, auth).await,
            }
            // Apply the remembered table, pane, and row detail panel (FRE-109)
            // to the freshly opened tab.
            let id = self
                .open_locators
                .read()
                .iter()
                .find(|(_, locator)| *locator == tab.locator)
                .map(|(id, _)| *id);
            if let Some(id) = id {
                let selected = tab.selected_table.as_ref().map(|name| TableRef {
                    schema: tab.selected_schema.clone(),
                    name: name.clone(),
                });
                let pane = Pane::from_session(tab.pane);
                let mut tab_ui = self.tab_ui.write();
                let ui = tab_ui.entry(id).or_default();
                ui.selected_table = selected;
                ui.pane = pane;
                ui.row_detail = tab.row_detail;
            }
        }
        // Restore the active view: the remembered active tab if it reopened,
        // else the connections screen. (Each connect above set `active` to its
        // own tab, so this override runs last.)
        let active_id = session.active.as_ref().and_then(|locator| {
            self.open_locators
                .read()
                .iter()
                .find(|(_, l)| l == locator)
                .map(|(id, _)| *id)
        });
        match active_id {
            Some(id) => self.active.set(ActiveView::Connection(id)),
            None => self.active.set(ActiveView::Connections),
        }
    }
}
