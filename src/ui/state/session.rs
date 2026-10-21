//! Session persistence: snapshotting the open tabs to
//! `session.toml` and restoring them on the next launch.
//!
//! Restore deliberately drives the ordinary connect flow rather than a
//! shortcut of its own, so a restored tab is indistinguishable from one the
//! user opened by hand — including its prompts.

use super::*;

impl AppState {
    /// Snapshots the current session: the open tabs in tab order,
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
            // schedules the write that remembers it.
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

    /// Reopens the previous session's tabs. Runs once at startup.
    ///
    /// Only connections still in the saved list are reopened (ad-hoc ones are
    /// not resurrected). SQLite reconnects silently; Postgres reconnects only
    /// when its password is already available (session memory or keyring), so
    /// startup never raises a wall of password prompts — a saved Postgres
    /// connection whose password isn't stored is simply left for the user to
    /// click. A locator whose file/server is gone just fails to reopen through
    /// the normal connect-error path; it never blocks the rest of the restore.
    ///
    /// Returns once the connects have *started* (see [`Self::start_restore`]),
    /// not once they have finished.
    pub async fn restore_session(mut self) {
        let Some(path) = default_session_path() else {
            return;
        };
        let session = load_session(&path);
        // Before the early return: which groups were folded is remembered
        // whether or not any tab was open.
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
        self.start_restore(plan, session.active, &saved);
    }

    /// Starts every planned tab's connect at once, each the way a click on
    /// its row in the connections list would ([`Self::start_connect`]), in
    /// the background.
    ///
    /// Concurrent, and cancellable, because restore used to await one tab at
    /// a time: a tab whose server never answered held up every tab after it
    /// and the command-line target behind them all, while its row showed a
    /// spinner with no Cancel — a row has one only for a connect that has a
    /// request.
    ///
    /// What each tab reopens showing — and whether it was the one in front —
    /// waits in [`AppState::restoring`] until its tab opens
    /// ([`Self::finish_connect`] applies it), whichever connect opens it.
    /// The tab that was in front last time comes to the front again only if
    /// nothing else has in the meantime ([`restored_tab_takes_focus`]): the
    /// connects now finish in any order, and alongside whatever the command
    /// line or the OS asked to open, which must not lose the foreground to a
    /// tab the user did not ask for.
    pub(super) fn start_restore(
        mut self,
        plan: Vec<SessionTab>,
        active: Option<String>,
        saved: &[SavedConnection],
    ) {
        for tab in plan {
            let Some(saved_conn) = saved.iter().find(|s| saved_open_locator(s) == tab.locator)
            else {
                continue;
            };
            let (tunnel, auth) = match saved_conn {
                SavedConnection::Postgres { tunnel, auth, .. }
                | SavedConnection::SqlServer { tunnel, auth, .. } => (tunnel.clone(), auth.clone()),
                SavedConnection::Sqlite { .. } => (None, ServerAuth::Password),
            };
            let in_front = active.as_deref() == Some(tab.locator.as_str());
            self.restoring
                .write()
                .insert(tab.locator.clone(), RestoredTab { tab, in_front });
            self.start_connect(
                saved_conn.locator(),
                saved_conn.name().to_string(),
                saved_conn.backend(),
                tunnel,
                auth,
                false,
            );
        }
    }

    /// Applies a restored tab's remembered table, pane and row detail panel
    /// to the tab `id` that just opened for it, and brings it to the front if
    /// [`restored_tab_takes_focus`] says so.
    pub(super) fn apply_restored(mut self, id: ConnectionId, restored: RestoredTab) {
        let RestoredTab { tab, in_front } = restored;
        {
            let selected = tab.selected_table.map(|name| TableRef {
                schema: tab.selected_schema,
                name,
            });
            let mut tab_ui = self.tab_ui.write();
            let ui = tab_ui.entry(id).or_default();
            ui.selected_table = selected;
            ui.pane = Pane::from_session(tab.pane);
            ui.row_detail = tab.row_detail;
        }
        if restored_tab_takes_focus(in_front, *self.active.peek()) {
            self.active.set(ActiveView::Connection(id));
        }
    }
}

/// Whether a restored tab that has just opened comes to the front: only the
/// one that was in front last time, and only while the connections screen is
/// still showing. Anything else showing was put there since — by the user, by
/// the command line, by the OS — and outranks a remembered preference.
pub(super) fn restored_tab_takes_focus(was_in_front: bool, showing: ActiveView) -> bool {
    was_in_front && showing == ActiveView::Connections
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dioxus::core::VirtualDom;

    use super::*;
    use crate::ui::test_dom::provide_state;

    #[tokio::test]
    async fn only_the_tab_that_was_in_front_comes_forward_and_only_onto_the_list() {
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = ConnectionRegistry::default().insert("x.db", pool, WriteProtection::Open);
        let elsewhere = ActiveView::Connection(id);
        assert!(restored_tab_takes_focus(true, ActiveView::Connections));
        assert!(!restored_tab_takes_focus(false, ActiveView::Connections));
        // Something else came forward while it connected: the user's own
        // click, the command line's target, a file the OS handed over.
        assert!(!restored_tab_takes_focus(true, elsewhere));
        assert!(!restored_tab_takes_focus(false, elsewhere));
    }

    fn empty() -> Element {
        rsx! {}
    }

    /// Runs `dom`'s tasks until `done` holds, failing after `limit`.
    async fn run_until(dom: &mut VirtualDom, limit: Duration, done: impl Fn(&VirtualDom) -> bool) {
        let deadline = tokio::time::Instant::now() + limit;
        while !done(dom) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "not reached within {limit:?}"
            );
            let _ = tokio::time::timeout(Duration::from_millis(50), dom.wait_for_work()).await;
            dom.render_immediate(&mut dioxus::core::NoOpMutations);
        }
    }

    async fn sqlite_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        // Options rather than a `sqlite://` URL, which would have to spell a
        // Windows path.
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(options)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        path
    }

    #[tokio::test]
    async fn a_restored_tab_that_never_connects_holds_up_nothing_and_can_be_cancelled() {
        // A Postgres server that accepts and never answers: sqlx waits out its
        // 30s acquire timeout. It is the *first* tab, so a restore that
        // awaited each tab in turn would hold the second up behind it.
        let silent = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let hanging_url = format!(
            "postgres://u@127.0.0.1:{}/db",
            silent.local_addr().unwrap().port()
        );
        let dir = tempfile::tempdir().unwrap();
        let restored_db = sqlite_file(dir.path(), "restored.db").await;
        let argv_db = sqlite_file(dir.path(), "argv.db").await;

        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        let (hanging, restored) = dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            saved.add(SavedConnection::Postgres {
                name: "hanging".into(),
                url: hanging_url.clone(),
                tunnel: None,
                auth: ServerAuth::Password,
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved.add(SavedConnection::Sqlite {
                name: "restored".into(),
                path: restored_db.clone(),
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            let entries = saved.entries();
            (
                saved_open_locator(&entries[0]),
                saved_open_locator(&entries[1]),
            )
        });
        // A password in session memory, so the connect never reads the
        // keyring.
        dom.in_runtime(|| {
            let mut passwords = state.session_passwords;
            passwords.write().insert(hanging.clone(), "pw".into());
        });
        let plan = vec![
            SessionTab {
                locator: hanging.clone(),
                ..SessionTab::default()
            },
            SessionTab {
                locator: restored.clone(),
                selected_table: Some("t".into()),
                pane: SessionPane::Sql,
                ..SessionTab::default()
            },
        ];
        dom.in_runtime(|| {
            let saved: Vec<SavedConnection> = state.saved.peek().entries().to_vec();
            state.start_restore(plan, Some(restored.clone()), &saved);
        });

        let open_id = |dom: &VirtualDom, locator: &str| {
            dom.in_runtime(|| {
                state
                    .open_locators
                    .peek()
                    .iter()
                    .find(|(_, l)| l == locator)
                    .map(|(id, _)| *id)
            })
        };
        run_until(&mut dom, Duration::from_secs(10), |dom| {
            open_id(dom, &restored).is_some()
        })
        .await;
        let restored_id = open_id(&dom, &restored).unwrap();
        dom.in_runtime(|| {
            let tab_ui = state.tab_ui.peek();
            let ui = &tab_ui[&restored_id];
            assert_eq!(
                ui.selected_table.as_ref().map(|t| t.name.as_str()),
                Some("t")
            );
            assert_eq!(ui.pane, Pane::Sql);
            assert_eq!(
                *state.active.peek(),
                ActiveView::Connection(restored_id),
                "the tab that was in front comes forward onto the list"
            );
            // The hanging tab is still connecting, and its row can cancel it.
            assert!(state.connecting.peek().iter().any(|c| c.locator == hanging));
            assert!(state.connect_requests.peek().contains_key(&hanging));
        });

        // A target from the command line opens while the hanging tab still
        // hangs, and stays in front.
        dom.in_runtime(|| {
            dioxus::core::spawn_forever(
                state.open_target(crate::cli::OpenTarget::File(argv_db.clone())),
            );
        });
        let argv = canonical(&argv_db).display().to_string();
        run_until(&mut dom, Duration::from_secs(10), |dom| {
            open_id(dom, &argv).is_some()
        })
        .await;
        let argv_id = open_id(&dom, &argv).unwrap();
        dom.in_runtime(|| {
            assert_eq!(*state.active.peek(), ActiveView::Connection(argv_id));
        });

        // And a restored tab that was in front, opening *after* the target,
        // leaves it in front.
        let late_db = sqlite_file(dir.path(), "late.db").await;
        let late = dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            saved.add(SavedConnection::Sqlite {
                name: "late".into(),
                path: late_db.clone(),
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved_open_locator(saved.entries().last().unwrap())
        });
        dom.in_runtime(|| {
            let saved: Vec<SavedConnection> = state.saved.peek().entries().to_vec();
            let plan = vec![SessionTab {
                locator: late.clone(),
                ..SessionTab::default()
            }];
            state.start_restore(plan, Some(late.clone()), &saved);
        });
        run_until(&mut dom, Duration::from_secs(10), |dom| {
            open_id(dom, &late).is_some()
        })
        .await;
        dom.in_runtime(|| {
            assert_eq!(
                *state.active.peek(),
                ActiveView::Connection(argv_id),
                "a remembered preference must not take the foreground from a target"
            );
            state.cancel_connect(&hanging);
            assert!(state.connecting.peek().is_empty());
            assert!(state.connect_requests.peek().is_empty());
            assert!(state.restoring.peek().is_empty());
        });
    }
}
