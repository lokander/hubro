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
        let Some(path) = self.paths.peek().session.clone() else {
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
        let Some(path) = self.paths.peek().session.clone() else {
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
        let saved: Vec<SavedConnection> = self.saved.read().entries().to_vec();
        self.restore_tabs(&session, &saved, move |connection| {
            self.connects_silently(connection)
        });
    }

    /// Plans and starts the restore of `session`'s tabs, asking `ready`
    /// whether each server connection can sign in without a prompt.
    ///
    /// SQLite tabs start at once. Each server tab waits only on its own
    /// `ready` — a keyring read — in a task of its own, so a keyring that is
    /// slow to answer (a locked wallet waiting on its unlock dialog) holds up
    /// that tab and nothing else: not the local files, not the other servers,
    /// not the command-line target. It used to be asked about every saved
    /// server connection, one after another, before any tab started. Saved
    /// connections the session never had open are not asked about at all.
    pub(super) fn restore_tabs<F, Fut>(self, session: &Session, saved: &[SavedConnection], ready: F)
    where
        F: Fn(SavedConnection) -> Fut,
        Fut: std::future::Future<Output = bool> + 'static,
    {
        let candidates: Vec<RestoreCandidate> = saved
            .iter()
            .map(|s| RestoreCandidate {
                locator: saved_open_locator(s),
                backend: s.backend(),
            })
            .collect();
        let order: Vec<String> = session.tabs.iter().map(|t| t.locator.clone()).collect();
        // No server connection is ready yet, so this is the local files.
        let local = plan_session_restore(&session.tabs, &candidates, |_| false);
        self.start_restore(local, session.active.clone(), saved, &order);
        for tab in &session.tabs {
            let Some(connection) = saved.iter().find(|s| {
                s.backend() != BackendKind::Sqlite && saved_open_locator(s) == tab.locator
            }) else {
                continue;
            };
            let ready = ready(connection.clone());
            let tab = tab.clone();
            let connection = connection.clone();
            let candidates = candidates.clone();
            let active = session.active.clone();
            let order = order.clone();
            spawn_forever(async move {
                let ready = ready.await;
                // The planner still makes the call, one tab at a time.
                let plan = plan_session_restore(std::slice::from_ref(&tab), &candidates, |_| ready);
                self.start_restore(plan, active, std::slice::from_ref(&connection), &order);
            });
        }
    }

    /// Whether a saved server connection can sign in without asking, so
    /// startup never blocks on a prompt or pops a browser. Password auth
    /// needs a stored or session password; Entra managed identity always
    /// can; Entra interactive only with a kept refresh token — which, across
    /// a restart, means one the user asked to have remembered. The keyring
    /// reads are off-thread, and the session-memory borrow is dropped before
    /// the await.
    async fn connects_silently(self, connection: SavedConnection) -> bool {
        let locator = saved_open_locator(&connection);
        let auth = match connection {
            SavedConnection::Postgres { auth, .. } | SavedConnection::SqlServer { auth, .. } => {
                auth
            }
            // Needs no credentials; the planner always keeps it.
            SavedConnection::Sqlite { .. } => return true,
        };
        match auth {
            ServerAuth::Entra(entra) => {
                // The connect's own lookup, so the two agree on whether a
                // sign-in is kept.
                let has_refresh = self.cached_entra_refresh(&locator).await.is_some();
                entra.can_acquire_silently(has_refresh)
            }
            ServerAuth::Password => {
                let in_session = self.session_passwords.read().contains_key(&locator);
                in_session
                    || crate::secrets::get_password_async(locator)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
            }
        }
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
    ///
    /// For the same reason each tab takes its session place among the tabs
    /// when it opens (`order`, the session's locators), rather than the place
    /// its connect happened to finish in — local files finish first, so
    /// without it every launch would move them ahead of the servers, and the
    /// session saved on the way out would keep the new order.
    pub(super) fn start_restore(
        mut self,
        plan: Vec<SessionTab>,
        active: Option<String>,
        saved: &[SavedConnection],
        order: &[String],
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
            let earlier = order
                .iter()
                .take_while(|locator| **locator != tab.locator)
                .cloned()
                .collect();
            self.restoring.write().insert(
                tab.locator.clone(),
                RestoredTab {
                    tab,
                    in_front,
                    earlier,
                },
            );
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
    /// to the tab `id` that just opened for it, moves it to its session place
    /// ([`restored_place`]), and brings it to the front if
    /// [`restored_tab_takes_focus`] says so.
    pub(super) fn apply_restored(mut self, id: ConnectionId, restored: RestoredTab) {
        let RestoredTab {
            tab,
            in_front,
            earlier,
        } = restored;
        {
            let mut open = self.open_locators.write();
            let after = restored_place(&open, id, &earlier);
            move_after(&mut open, id, after);
            self.registry.write().move_after(id, after);
        }
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

/// Where a restored tab `id` that has just opened belongs among the `open`
/// tabs: just after the last of them that came before it in the session
/// (`earlier`), or first when none has opened yet. Tabs that are not from the
/// session — opened by the user, the command line or the OS — never count, so
/// they keep their places after the session's.
pub(super) fn restored_place(
    open: &[(ConnectionId, String)],
    id: ConnectionId,
    earlier: &[String],
) -> Option<ConnectionId> {
    open.iter()
        .filter(|(open_id, _)| *open_id != id)
        .rfind(|(_, locator)| earlier.contains(locator))
        .map(|(open_id, _)| *open_id)
}

/// Moves `id`'s entry to just after `after`'s, or to the front.
fn move_after(
    open: &mut Vec<(ConnectionId, String)>,
    id: ConnectionId,
    after: Option<ConnectionId>,
) {
    let Some(from) = open.iter().position(|(open_id, _)| *open_id == id) else {
        return;
    };
    let entry = open.remove(from);
    let to = match after {
        None => 0,
        Some(after) => open
            .iter()
            .position(|(open_id, _)| *open_id == after)
            .map_or(open.len(), |i| i + 1),
    };
    open.insert(to, entry);
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
            state.start_restore(plan, Some(restored.clone()), &saved, &[]);
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
            state.start_restore(plan, Some(late.clone()), &saved, &[]);
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

    #[tokio::test]
    async fn a_keyring_that_never_answers_holds_up_only_its_own_tab() {
        // A server tab whose keyring read never returns — a locked wallet
        // waiting on its unlock dialog — must not hold up the local file, nor
        // another server tab whose read does return.
        let silent = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = |db: &str| {
            format!(
                "postgres://u@127.0.0.1:{}/{db}",
                silent.local_addr().unwrap().port()
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let local_db = sqlite_file(dir.path(), "local.db").await;

        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        let saved = dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            for name in ["locked", "unlocked"] {
                saved.add(SavedConnection::Postgres {
                    name: name.into(),
                    url: url(name),
                    tunnel: None,
                    auth: ServerAuth::Password,
                    protection: WriteProtection::Open,
                    color: None,
                    group: None,
                });
            }
            saved.add(SavedConnection::Sqlite {
                name: "local".into(),
                path: local_db.clone(),
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved.entries().to_vec()
        });
        let [locked, unlocked, local] = [0, 1, 2].map(|i| saved_open_locator(&saved[i]));
        let session = Session {
            // The locked tab first: a restore that asked the keyring about
            // every tab before starting any would never get past it.
            tabs: [&locked, &unlocked, &local]
                .map(|locator| SessionTab {
                    locator: locator.clone(),
                    ..SessionTab::default()
                })
                .to_vec(),
            ..Session::default()
        };
        dom.in_runtime(|| {
            // Session memory for the one that gets through, so its connect
            // never reads the real keyring either.
            let mut passwords = state.session_passwords;
            passwords.write().insert(unlocked.clone(), "pw".into());
            state.restore_tabs(&session, &saved, |connection| {
                let answers = connection.name() != "locked";
                async move {
                    if !answers {
                        std::future::pending::<()>().await;
                    }
                    true
                }
            });
        });
        let opened = |dom: &VirtualDom| {
            dom.in_runtime(|| state.open_locators.peek().iter().any(|(_, l)| *l == local))
        };
        let connecting = |dom: &VirtualDom, locator: &str| {
            dom.in_runtime(|| state.connecting.peek().iter().any(|c| c.locator == locator))
        };
        run_until(&mut dom, Duration::from_secs(10), |dom| {
            opened(dom) && connecting(dom, &unlocked)
        })
        .await;
        assert!(
            !connecting(&dom, &locked),
            "a tab whose keyring has not answered must not start"
        );
        dom.in_runtime(|| state.cancel_connect(&unlocked));
    }

    #[tokio::test]
    async fn a_restored_tab_takes_its_session_place_however_late_it_opens() {
        let registry_ids = |n: usize| {
            let mut registry = ConnectionRegistry::default();
            (0..n)
                .map(|i| {
                    let pool = DbPool::Sqlite(
                        sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
                    );
                    registry.insert(format!("{i}"), pool, WriteProtection::Open)
                })
                .collect::<Vec<_>>()
        };
        let ids = registry_ids(4);
        let (server, local, argv, later) = (ids[0], ids[1], ids[2], ids[3]);
        let entry = |id, l: &str| (id, l.to_string());
        let earlier = |ls: &[&str]| ls.iter().map(|l| l.to_string()).collect::<Vec<_>>();

        // Session [server, local]: the local file opened first, and the
        // command line's file after it; the server tab opens last.
        let mut open = vec![
            entry(local, "local"),
            entry(argv, "argv"),
            entry(server, "server"),
        ];
        let after = restored_place(&open, server, &earlier(&[]));
        assert_eq!(after, None, "nothing came before it, so it goes first");
        move_after(&mut open, server, after);
        assert_eq!(
            open.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
            ["server", "local", "argv"],
            "the session's order, and the command line's tab after it"
        );

        // A third session tab, after both, lands after the last of them that
        // is open — not after the command line's tab.
        open.push(entry(later, "later"));
        let after = restored_place(&open, later, &earlier(&["server", "local"]));
        assert_eq!(after, Some(local));
        move_after(&mut open, later, after);
        assert_eq!(
            open.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
            ["server", "local", "later", "argv"]
        );
    }

    #[tokio::test]
    async fn restored_tabs_come_back_and_are_saved_in_session_order() {
        // The server tab first, and slow to start (its readiness answers
        // late), so it is the last to open: it must still come back first,
        // and be saved first.
        let Ok(pg) = std::env::var("HUBRO_PG_TEST_URL") else {
            eprintln!("skipping: HUBRO_PG_TEST_URL not set");
            return;
        };
        let mut parsed = url::Url::parse(&pg).unwrap();
        let password = parsed.password().unwrap_or_default().to_string();
        parsed.set_password(None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let local_db = sqlite_file(dir.path(), "local.db").await;

        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        let saved = dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            saved.add(SavedConnection::Postgres {
                name: "server".into(),
                url: parsed.to_string(),
                tunnel: None,
                auth: ServerAuth::Password,
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved.add(SavedConnection::Sqlite {
                name: "local".into(),
                path: local_db.clone(),
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved.entries().to_vec()
        });
        let order: Vec<String> = saved.iter().map(saved_open_locator).collect();
        let session = Session {
            tabs: order
                .iter()
                .map(|locator| SessionTab {
                    locator: locator.clone(),
                    ..SessionTab::default()
                })
                .collect(),
            ..Session::default()
        };
        dom.in_runtime(|| {
            let mut passwords = state.session_passwords;
            passwords.write().insert(order[0].clone(), password.clone());
            state.restore_tabs(&session, &saved, |_| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                true
            });
        });
        run_until(&mut dom, Duration::from_secs(20), |dom| {
            dom.in_runtime(|| state.open_locators.peek().len() == 2)
        })
        .await;
        dom.in_runtime(|| {
            let open: Vec<String> = state
                .open_locators
                .peek()
                .iter()
                .map(|(_, l)| l.clone())
                .collect();
            assert_eq!(open, order, "the tabs, in session order");
            let names: Vec<String> = state
                .registry
                .peek()
                .iter()
                .map(|c| c.name.clone())
                .collect();
            assert_eq!(names.len(), 2);
            assert_eq!(names[1], "local.db", "the tab bar, in session order");
            let persisted: Vec<String> = state
                .current_session()
                .tabs
                .iter()
                .map(|t| t.locator.clone())
                .collect();
            assert_eq!(persisted, order, "and saved in that order");
        });
    }

    /// A saved Postgres connection to a server that accepts and never
    /// answers, its locator, and the listener to keep alive meanwhile.
    async fn hanging_postgres(
        dom: &VirtualDom,
        state: AppState,
    ) -> (String, SavedConnection, tokio::net::TcpListener) {
        let silent = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!(
            "postgres://u@127.0.0.1:{}/db",
            silent.local_addr().unwrap().port()
        );
        let connection = dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            saved.add(SavedConnection::Postgres {
                name: "hanging".into(),
                url,
                tunnel: None,
                auth: ServerAuth::Password,
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved.entries().last().unwrap().clone()
        });
        let locator = saved_open_locator(&connection);
        dom.in_runtime(|| {
            // Session memory, so the connect never reads the keyring.
            let mut passwords = state.session_passwords;
            passwords.write().insert(locator.clone(), "pw".into());
        });
        (locator, connection, silent)
    }

    #[tokio::test]
    async fn opening_a_tab_whose_restore_is_in_flight_brings_it_forward() {
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        let (locator, connection, _silent) = hanging_postgres(&dom, state).await;
        let focus = |dom: &VirtualDom| {
            dom.in_runtime(|| state.connect_requests.peek().get(&locator).map(|r| r.focus))
        };
        let restore = |dom: &VirtualDom| {
            dom.in_runtime(|| {
                let plan = vec![SessionTab {
                    locator: locator.clone(),
                    ..SessionTab::default()
                }];
                state.start_restore(plan, None, std::slice::from_ref(&connection), &[]);
            })
        };
        restore(&dom);
        run_until(&mut dom, Duration::from_secs(5), |dom| {
            dom.in_runtime(|| state.connecting.peek().iter().any(|c| c.locator == locator))
        })
        .await;
        assert_eq!(
            focus(&dom),
            Some(false),
            "a restore opens in the background"
        );

        // A click on its row.
        dom.in_runtime(|| {
            state.start_connect(
                connection.locator(),
                "hanging".into(),
                BackendKind::Postgres,
                None,
                ServerAuth::Password,
                true,
            )
        });
        assert_eq!(focus(&dom), Some(true), "a click asks for it in front");

        // And a connect with no request of its own — the command line, a file
        // the OS handed over — reaching the reserved locator.
        dom.in_runtime(|| state.cancel_connect(&locator));
        restore(&dom);
        run_until(&mut dom, Duration::from_secs(5), |dom| {
            dom.in_runtime(|| state.connecting.peek().iter().any(|c| c.locator == locator))
        })
        .await;
        assert_eq!(focus(&dom), Some(false));
        dom.in_runtime(|| {
            spawn_forever(state.connect_server(
                ServerBackend::of(BackendKind::Postgres),
                locator.clone(),
                "hanging".into(),
                None,
                ServerAuth::Password,
            ));
        });
        run_until(&mut dom, Duration::from_secs(5), |dom| {
            focus(dom) == Some(true)
        })
        .await;
        dom.in_runtime(|| state.cancel_connect(&locator));
    }

    #[tokio::test]
    async fn a_restore_that_fails_short_of_opening_forgets_its_tab() {
        // A tunnel to a port nothing listens on fails in `open_tunnel`, which
        // ends the connect without reaching `finish_connect`. The remembered
        // view must go with it, or a later open of the same connection — a
        // shift-click, say — would be pulled to the front and onto a stale
        // table.
        let closed = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        let connection = dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            saved.add(SavedConnection::Postgres {
                name: "unreachable".into(),
                url: "postgres://u@db.internal:5432/db".into(),
                tunnel: Some(TunnelConfig {
                    host: "127.0.0.1".into(),
                    port,
                    user: "u".into(),
                    auth: TunnelAuth::Agent,
                }),
                auth: ServerAuth::Password,
                protection: WriteProtection::Open,
                color: None,
                group: None,
            });
            saved.entries().last().unwrap().clone()
        });
        let locator = saved_open_locator(&connection);
        dom.in_runtime(|| {
            let plan = vec![SessionTab {
                locator: locator.clone(),
                selected_table: Some("t".into()),
                ..SessionTab::default()
            }];
            state.start_restore(
                plan,
                Some(locator.clone()),
                std::slice::from_ref(&connection),
                &[],
            );
            assert!(state.restoring.peek().contains_key(&locator));
        });
        run_until(&mut dom, Duration::from_secs(10), |dom| {
            dom.in_runtime(|| state.connect_error.peek().is_some())
        })
        .await;
        dom.in_runtime(|| {
            assert!(
                state
                    .connect_error
                    .peek()
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("SSH tunnel"),
                "{:?}",
                state.connect_error.peek()
            );
            assert!(state.connecting.peek().is_empty());
            assert!(
                state.restoring.peek().is_empty(),
                "a connect that ended must take its remembered view with it"
            );
        });
    }
}
