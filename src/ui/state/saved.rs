//! The saved-connections list as the app edits it: adding, marking, grouping,
//! editing and removing entries, and the folds the connections screen keeps
//! for its groups.

use super::*;

use super::secrets_policy::*;

/// How an already-saved server connection is configured to connect: its SSH
/// tunnel and its auth mode, or the no-tunnel/password defaults when hubro has
/// never seen this locator.
///
/// Exists for [`AppState::open_target`], and pure so that its consequence is
/// testable without a runtime: a connect *persists how it connected*, so
/// handing the connect flow anything other than what the entry already records
/// silently rewrites `connections.toml`. The saved locator and the normalized
/// URL a command line produces are the same string by construction — both come
/// out of `normalize_pg_url`/`normalize_mssql_url` — so this is an equality
/// match rather than a fuzzy one.
///
/// SQLite entries never match: they are keyed by path, have no URL, and reach
/// none of the server flows.
pub(super) fn saved_server_settings(
    saved: &[SavedConnection],
    url: &str,
) -> (Option<TunnelConfig>, ServerAuth) {
    saved
        .iter()
        .find_map(|entry| match entry {
            SavedConnection::Postgres {
                url: saved_url,
                tunnel,
                auth,
                ..
            }
            | SavedConnection::SqlServer {
                url: saved_url,
                tunnel,
                auth,
                ..
            } if saved_url == url => Some((tunnel.clone(), auth.clone())),
            _ => None,
        })
        .unwrap_or((None, ServerAuth::Password))
}

/// Carries a renamed group's fold across the rename.
///
/// The fold is remembered by name, in a *different file* from the group
/// itself: the group lives in connections.toml, the fold in session.toml. So
/// a rename that moved only the group would silently expand it and leave a
/// dead name behind to accumulate. Free functions over the plain `Vec` rather
/// than signal code inline, so the claim is a test rather than a comment.
///
/// The dedup at the end is not hypothetical bookkeeping: a rename onto a name
/// the list already holds (a fold left by an earlier group of that name)
/// would otherwise leave it twice, and every later `retain`/rename would have
/// to cope with that.
fn rename_collapsed(collapsed: &mut Vec<String>, old: &str, new: &str) {
    for name in collapsed.iter_mut() {
        if name == old {
            *name = new.to_string();
        }
    }
    let mut seen: Vec<String> = Vec::with_capacity(collapsed.len());
    collapsed.retain(|name| {
        let fresh = !seen.iter().any(|s| s == name);
        if fresh {
            seen.push(name.clone());
        }
        fresh
    });
}

/// Whether `old` is folded, i.e. whether [`rename_collapsed`] has anything to
/// do — kept separate so the caller can take the cheap read before the write.
fn collapsed_needs_rename(collapsed: &[String], old: &str) -> bool {
    collapsed.iter().any(|name| name == old)
}

/// Drops a deleted group's fold. A name nothing can expand again
/// would sit in session.toml forever, and would silently re-collapse a group
/// later created with the same name.
fn forget_collapsed(collapsed: &mut Vec<String>, name: &str) {
    collapsed.retain(|folded| folded != name);
}

/// What a saved-list edit hands back, read for whether it changed the list
/// and so has to be persisted ([`AppState::mutate_saved`]).
trait SavedChange {
    fn changed(&self) -> bool;
}

/// `true` when the edit changed the list.
impl SavedChange for bool {
    fn changed(&self) -> bool {
        *self
    }
}

/// `Ok` when the edit was made; an `Err` was refused and changed nothing.
impl<T, E> SavedChange for Result<T, E> {
    fn changed(&self) -> bool {
        self.is_ok()
    }
}

/// `Some` when the edit removed something.
impl<T> SavedChange for Option<T> {
    fn changed(&self) -> bool {
        self.is_some()
    }
}

impl AppState {
    /// Adds a database file to the saved list (deduped by path) and
    /// persists the list.
    pub fn add_saved(self, path: PathBuf) {
        let path = canonical(&path);
        self.mutate_saved(|saved| {
            saved.add(SavedConnection::Sqlite {
                name: tab_title(&path),
                path,
                protection: WriteProtection::Open,
                color: None,
                group: None,
            })
        });
    }

    /// Marks a saved connection's write protection and accent colour,
    /// persists the list, and re-marks any tab already open on it
    /// so the change takes effect now rather than on the next reconnect.
    ///
    /// Marking lives on the connections-list row rather than in the connect
    /// form because it has to reach SQLite entries too, and those have no
    /// edit form — the form only exists for the two server backends.
    ///
    /// `locator` is the entry's stored [`SavedConnection::locator`]. Open tabs
    /// are keyed by the *canonical* [`saved_open_locator`] instead, and for a
    /// SQLite path the two can differ (a symlinked directory, a relative path,
    /// macOS `/tmp` → `/private/tmp`) — `normalize_and_dedup` deliberately
    /// leaves SQLite paths alone. So the open tabs are re-marked through the
    /// canonical form translated from the entry we just wrote, not through
    /// `locator` directly: keying both lookups the same way would persist the
    /// marking while leaving the open tab still accepting writes.
    pub fn set_saved_marking(
        mut self,
        locator: &str,
        protection: WriteProtection,
        color: Option<ConnectionColor>,
    ) {
        if !self.mutate_saved(|saved| saved.set_marking(locator, protection, color)) {
            return;
        }
        let open_locator = self
            .saved
            .read()
            .entries()
            .iter()
            .find(|saved| saved.locator() == locator)
            .map(saved_open_locator);
        if let Some(open_locator) = open_locator {
            self.remark_open_connections(&open_locator);
        }
    }

    /// Creates a connection group and persists the list. The error
    /// is handed back for the connections screen to show next to the field
    /// the name was typed into.
    pub fn create_saved_group(self, name: &str) -> Result<String, GroupError> {
        self.mutate_saved(|saved| saved.create_group(name))
    }

    /// Renames a group, carrying its members and its collapsed state with it.
    pub fn rename_saved_group(mut self, old: &str, new: &str) -> Result<String, GroupError> {
        let renamed = self.mutate_saved(|saved| saved.rename_group(old, new));
        let Ok(new_name) = renamed else {
            return renamed;
        };
        if new_name != old && collapsed_needs_rename(&self.collapsed_groups.peek(), old) {
            rename_collapsed(&mut self.collapsed_groups.write(), old, &new_name);
        }
        Ok(new_name)
    }

    /// Removes a group; its connections become ungrouped rather than being
    /// removed with it.
    pub fn remove_saved_group(mut self, name: &str) {
        if !self.mutate_saved(|saved| saved.remove_group(name)) {
            return;
        }
        if self.collapsed_groups.peek().iter().any(|g| g == name) {
            forget_collapsed(&mut self.collapsed_groups.write(), name);
        }
    }

    /// Moves a group one step up or down the display order.
    pub fn move_saved_group(self, name: &str, step: GroupStep) {
        self.mutate_saved(|saved| saved.move_group(name, step));
    }

    /// Files a saved connection under a group, or ungroups it with
    /// `None`. A connection is in at most one group, so this replaces rather
    /// than adds.
    ///
    /// Nothing about an *open* tab depends on the group — unlike the marking,
    /// which has to reach the live connection — so there is no counterpart
    /// here to [`Self::set_saved_marking`]'s re-marking step.
    pub fn assign_saved_group(self, locator: &str, group: Option<&str>) {
        self.mutate_saved(|saved| saved.assign_group(locator, group));
    }

    /// Folds or unfolds a group in the connections list. Persisted with the
    /// session by the shell's snapshot effect, which re-runs because
    /// `current_session` reads this signal.
    pub fn toggle_group_collapsed(mut self, name: &str) {
        let mut collapsed = self.collapsed_groups.write();
        match collapsed.iter().position(|g| g == name) {
            Some(index) => {
                collapsed.remove(index);
            }
            None => collapsed.push(name.to_string()),
        }
    }

    /// Records that the next successful connect to `new_locator` is an edit
    /// of `old_locator` rather than a new connection. Consumed by
    /// [`Self::save_server_if_open`], the one place every connect path saves
    /// through — including the Entra sign-in card, which completes long after
    /// the form has closed.
    pub fn set_pending_edit(mut self, old_locator: String, new_locator: String) {
        self.pending_edit.set(Some(PendingEdit {
            old_locator,
            new_locator,
        }));
    }

    /// Drops a pending edit (the form was cancelled or the connect failed).
    pub fn clear_pending_edit(mut self) {
        if self.pending_edit.peek().is_some() {
            self.pending_edit.set(None);
        }
    }

    /// Saves a just-opened connection, applying a pending edit when one is
    /// waiting for exactly this locator. Matching on the locator keeps a
    /// stale intent (a failed edit the user abandoned) from rewriting an
    /// unrelated connection.
    ///
    /// Both branches can rewrite an entry's auth — an edit, and `add` on a
    /// locator already saved — so whether that signs the connection out
    /// ([`entra_sign_in_dropped`]) is decided here, once, for either. The
    /// entry compared is the one under the locator being saved, which is the
    /// key its kept sign-in lives under; an edit that moves the locator
    /// forgets the old one's in [`Self::update_saved`].
    pub(super) fn save_or_apply_edit(mut self, connection: SavedConnection) {
        let locator = connection.locator();
        let before = self
            .saved
            .peek()
            .entries()
            .iter()
            .find(|entry| entry.locator() == locator)
            .and_then(|entry| entry.auth().cloned());
        let signs_out = entra_sign_in_dropped(before.as_ref(), connection.auth());
        let pending = self
            .pending_edit
            .peek()
            .clone()
            .filter(|edit| edit.new_locator == locator);
        match pending {
            Some(edit) => {
                self.pending_edit.set(None);
                self.update_saved(edit.old_locator, connection);
            }
            None => {
                self.mutate_saved(|saved| saved.add(connection));
            }
        }
        // Best-effort, like forgetting a moved locator's token: nothing was
        // asked of the user, so a refusal has nobody to tell.
        if signs_out {
            let verdict = self.start_entra_sign_out(&locator);
            spawn_forever(async move {
                let _ = self.finish_entra_sign_out(&locator, verdict).await;
            });
        }
    }

    /// Applies an edit to a saved connection: overwrites the entry
    /// at `old_locator` — name included, which [`Self::save_server_if_open`]
    /// deliberately never does — and persists.
    ///
    /// When the edit changes host/port/database the normalized locator moves
    /// with it, and that locator keys the keyring account too. The stored
    /// secrets are carried across to the new key so an untouched password
    /// keeps working, then dropped from the old one; a secret already stored
    /// under the new locator (the connect that just succeeded wrote one) wins.
    /// The Entra refresh token is the exception: the old one is forgotten,
    /// never carried ([`carried_secret_keys`]).
    pub fn update_saved(mut self, old_locator: String, connection: SavedConnection) {
        let new_locator = connection.locator().to_string();
        self.mutate_saved(|saved| saved.update(&old_locator, connection));
        if new_locator != old_locator {
            // Synchronously, and before the spawn: session memory is what the
            // *next* connect reads first, and the next connect can begin as
            // soon as this returns — the user is back on the connections
            // screen. Inside the spawned block it would instead land after
            // three keyring round-trips, and a reconnect in that window would
            // re-prompt for a secret the app is holding. The keyring migration
            // is spawned because it is I/O; this is a map write.
            carry_session_secrets(
                &mut self.session_passwords.write(),
                &old_locator,
                &new_locator,
            );
            // The refresh token is not carried ([`carried_secret_keys`]): the
            // confirming connect already put its own where the user said. The
            // old locator is signed out now, for the reason above — its
            // session copy goes and a connect to it still under way keeps
            // nothing — and its keyring copy in the spawn.
            let forgotten = self.start_entra_sign_out(&old_locator);
            // Read after the carry: what the confirming connect validated, or
            // the old session copy moved across when it validated nothing.
            let moves: Vec<_> = carried_secret_keys(&old_locator)
                .into_iter()
                .zip(carried_secret_keys(&new_locator))
                .map(|(old, new)| {
                    let validated = self.session_passwords.peek().get(&new).cloned();
                    (old, new, validated)
                })
                .collect();
            spawn_forever(async move {
                let _ = self.finish_entra_sign_out(&old_locator, forgotten).await;
                for (old, new, validated) in moves {
                    migrate_secret(old, new, validated).await;
                }
            });
        }
    }

    /// Removes a saved connection and persists. Postgres and SQL Server
    /// entries also drop their keyring credentials (database password, SSH key
    /// passphrase, and cached Entra refresh token; deleting a missing entry is
    /// a no-op), and every entry drops the same keys from session memory along
    /// with any parked "remember the passphrase" choice.
    ///
    /// An open tab on the removed connection keeps working — nothing here
    /// touches the live connection — but it no longer has a remembered secret
    /// to reconnect with, which is the intended reading of a deletion rather
    /// than an oversight.
    pub fn remove_saved(mut self, locator: &str) {
        if let Some(entry) = self.mutate_saved(|saved| saved.remove(locator)) {
            // The in-memory stores are keyed by locator whatever the backend
            // is, and clearing them is not I/O, so it happens for every entry
            // and before the keyring work is spawned.
            forget_connection_secrets(
                &mut self.session_passwords.write(),
                &mut self.ssh_remember.write(),
                locator,
            );
            if let SavedConnection::Postgres { url, .. } | SavedConnection::SqlServer { url, .. } =
                entry
            {
                // The kept sign-in goes the way every sign-out does, so a
                // connect to it still under way keeps nothing it gets back.
                let signed_out = self.start_entra_sign_out(&url);
                // Best-effort, off-thread: a missing keyring just means
                // nothing was stored.
                spawn_forever(async move {
                    for key in carried_secret_keys(&url) {
                        let _ = crate::secrets::delete_password_async(key).await;
                    }
                    let _ = self.finish_entra_sign_out(&url, signed_out).await;
                });
            }
        }
    }

    /// Applies `edit` to the saved list and persists the list when the edit
    /// says it changed something ([`SavedChange`]), handing back what the
    /// edit returned. The one mutate-then-persist step every saved-list
    /// edit goes through, so none can change the list and forget to write it.
    ///
    /// The write borrow ends with `edit`, before [`Self::persist_saved`]
    /// takes its read one.
    fn mutate_saved<R: SavedChange>(mut self, edit: impl FnOnce(&mut SavedList) -> R) -> R {
        let result = edit(&mut self.saved.write());
        if result.changed() {
            self.persist_saved();
        }
        result
    }

    fn persist_saved(mut self) {
        let Some(config) = self.paths.peek().connections.clone() else {
            self.connect_error
                .set(Some("no config directory found".to_string()));
            return;
        };
        let result = self.saved.read().persist(&config);
        if let Err(err) = result {
            self.connect_error.set(Some(err.to_string()));
        }
    }

    /// The write protection and accent colour saved for `locator`,
    /// or the defaults when the connection was opened ad hoc rather than from
    /// the saved list.
    pub(super) fn saved_marking(
        &self,
        locator: &str,
    ) -> (WriteProtection, Option<ConnectionColor>) {
        self.saved
            .read()
            .entries()
            .iter()
            .find(|saved| saved_open_locator(saved) == locator)
            .map(|saved| (saved.protection(), saved.color()))
            .unwrap_or_default()
    }

    /// Re-marks every open tab on `locator` after its saved entry is edited,
    /// so an open connection starts obeying a new protection
    /// immediately instead of on the next reconnect — the opposite would
    /// leave the tab you just protected still writable.
    fn remark_open_connections(&mut self, locator: &str) {
        let (protection, color) = self.saved_marking(locator);
        let ids: Vec<ConnectionId> = self
            .open_locators
            .read()
            .iter()
            .filter(|(_, open)| open == locator)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.registry.write().set_protection(id, protection);
            self.tab_mut(id, |tab| tab.color = color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_list_edit_is_written_exactly_when_it_changed_the_list() {
        // `mutate_saved` reads each edit's own answer — a `bool`, a `Result`,
        // an `Option` — for whether to write. Each kind is driven both ways
        // against the test state's own file: a write recreates the deleted
        // file, and an edit that changed nothing leaves it gone.
        fn empty() -> Element {
            rsx! {}
        }
        let dom = dioxus::core::VirtualDom::new(empty);
        let state = crate::ui::test_dom::provide_state(&dom);
        let file = dom.in_runtime(|| state.paths.peek().connections.clone().unwrap());
        let written = |edit: &dyn Fn()| {
            let _ = std::fs::remove_file(&file);
            dom.in_runtime(edit);
            file.exists()
        };
        assert!(written(&|| {
            state.create_saved_group("Prod").unwrap();
        }));
        assert!(!written(&|| {
            state.create_saved_group("Prod").unwrap_err();
        }));
        assert!(written(&|| state.add_saved(PathBuf::from("/tmp/a.db"))));
        let locator = dom.in_runtime(|| state.saved.peek().entries()[0].locator());
        assert!(written(&|| state.assign_saved_group(&locator, Some("Prod"))));
        assert!(!written(
            &|| state.assign_saved_group(&locator, Some("Prod"))
        ));
        assert!(!written(&|| state.remove_saved("/nowhere.db")));
        assert!(written(&|| state.remove_saved(&locator)));
    }

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_renamed_group_keeps_its_fold_and_leaves_no_dead_name() {
        let mut collapsed = v(&["Production", "Archive"]);
        assert!(collapsed_needs_rename(&collapsed, "Production"));
        rename_collapsed(&mut collapsed, "Production", "Prod (live)");
        assert_eq!(collapsed, v(&["Prod (live)", "Archive"]));
        assert!(
            !collapsed.iter().any(|n| n == "Production"),
            "the old name must not linger"
        );

        // An expanded group has no fold to carry, and renaming it must not
        // invent one.
        let mut collapsed = v(&["Archive"]);
        assert!(!collapsed_needs_rename(&collapsed, "Production"));
        rename_collapsed(&mut collapsed, "Production", "Prod (live)");
        assert_eq!(collapsed, v(&["Archive"]));

        // Renaming onto a name already in the list leaves it once, not twice.
        let mut collapsed = v(&["A", "B"]);
        rename_collapsed(&mut collapsed, "A", "B");
        assert_eq!(collapsed, v(&["B"]));
    }

    #[test]
    fn a_deleted_groups_fold_is_forgotten() {
        // Left behind, it would re-collapse a group later made with the same
        // name — a fold the user never set, restored from a group that no
        // longer exists.
        let mut collapsed = v(&["Production", "Archive"]);
        forget_collapsed(&mut collapsed, "Production");
        assert_eq!(collapsed, v(&["Archive"]));
        // Deleting an expanded (or unknown) group changes nothing.
        forget_collapsed(&mut collapsed, "Production");
        assert_eq!(collapsed, v(&["Archive"]));
    }
}
