//! The app's shared reactive state: [`AppState`], the signals it holds, and
//! the view/selection model the UI renders from.
//!
//! Split by concern (FRE-142). This file is the state layer proper — the
//! struct, its constructor, the view/navigation/schema accessors, and the
//! plain data types every part of the UI names. The procedural workflows that
//! merely *drive* those signals live beside it:
//!
//! - [`connect`] — the saved list and the connect/auth/tunnel flows;
//! - [`sql`] — running SQL, query history, and exports;
//! - [`staging`] — staged edits and the save path;
//! - [`session`] — snapshotting and restoring the open tabs.
//!
//! `impl AppState` blocks live in all five files, so the split is invisible
//! to callers: everything is still reached as `state::…`.
//!
//! The submodules open with `use super::*`. They hold nothing but
//! `impl AppState` and its private helpers — they are one type's
//! implementation spread across files rather than modules with a surface of
//! their own — so they deliberately inherit this file's imports and types
//! instead of restating thirty `use` lines four times.

mod connect;
mod import;
mod session;
mod sql;
mod staging;

pub use connect::ServerBackend;
pub use import::{ImportRequest, ImportStatus};
// Reached from `session`'s restore path; defined beside the flow that
// writes it.
pub(crate) use connect::entra_secret_key;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::core::{spawn_forever, Task};
use dioxus::prelude::*;

use crate::azure::{self, EntraAuth};
use crate::cli::OpenTarget;
use crate::config::{
    default_config_path, default_session_path, default_settings_path, load_session, load_settings,
    plan_session_restore, save_session, save_show_internal_objects, save_theme, BackendKind,
    ConnectionColor, GroupError, RestoreCandidate, SavedConnection, SavedList, ServerAuth, Session,
    SessionPane, SessionTab, Theme,
};
use crate::db::{
    apply_staged, build_fk_filter, explain_statement, mssql_url_target, mssql_url_via_local_port,
    mssql_url_with_password, needs_confirmation, open_source, run_import, run_script,
    script_refusal, split_statements, statement_preview, url_target, url_via_local_port,
    url_with_password, write_result, Capabilities, CellFetch, Connection, ConnectionId,
    ConnectionRegistry, DbError, DbPool, Ddl, DdlObject, Encoding, ExportFormat, Filter,
    ForeignKeyMeta, ImportOptions, ImportReport, MssqlAuth, QueryResult, Rollback, RowLocator,
    SourceFormat, StagedChange, StatementResult, TableAccess, TableMeta, TableStats, Value,
    WriteProtection,
};
use crate::history::{HistoryStore, SaveOutcome};
use crate::tunnel::{HostKeyInfo, Tunnel, TunnelAuth, TunnelConfig, TunnelError};
use crate::ui::notice::SPINNER_DELAY;
use crate::ui::stage::TableStage;

/// Which screen the main panel shows: the connections screen or one open
/// connection tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Connections,
    Connection(ConnectionId),
}

/// Schema introspection state for one connection.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaLoad {
    Loading,
    Ready(Vec<TableMeta>),
    Failed(String),
}

/// A table identified by optional schema + name (schema is `None` on
/// SQLite).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    /// Stable string form, used as component keys and expansion-set
    /// entries. Debug-formats both parts so schema/name splits can't
    /// collide (schema "a" + table "b.c" vs schema "a.b" + table "c").
    pub fn key(&self) -> String {
        format!("{:?}:{:?}", self.schema, self.name)
    }

    /// Human-readable qualified name.
    pub fn label(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{schema}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// A table plus the filter to show it under — the payload of a foreign-key
/// jump or a Back restore ([`AppState::navigate_fk`] /
/// [`AppState::navigate_back`]). The grid consumes a pending focus that
/// targets its table, seeding its filter from it (see
/// [`AppState::pending_focus`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FocusTarget {
    pub table: TableRef,
    /// `None` restores an unfiltered view (a Back to a table the user was
    /// browsing without a filter).
    pub filter: Option<Filter>,
}

/// A saved-connection edit that has been submitted but not yet confirmed by
/// a successful connect (FRE-75). Matched on `new_locator` so an abandoned
/// edit can never rewrite an unrelated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub old_locator: String,
    pub new_locator: String,
}

/// Which pane a connection tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Browser,
    Sql,
    /// The selected table's structure — columns and indexes (FRE-69).
    Schema,
}

impl Pane {
    /// The serializable form used in the persisted session (FRE-30).
    fn to_session(self) -> SessionPane {
        match self {
            Pane::Browser => SessionPane::Browser,
            Pane::Sql => SessionPane::Sql,
            Pane::Schema => SessionPane::Schema,
        }
    }

    fn from_session(pane: SessionPane) -> Self {
        match pane {
            SessionPane::Browser => Pane::Browser,
            SessionPane::Sql => Pane::Sql,
            SessionPane::Schema => Pane::Schema,
        }
    }
}

/// Minimum age of a parked navigation intent before repeating the action
/// confirms it. Without a floor, a double-click delivers two identical
/// attempts ~100 ms apart — parking and immediately "confirming" the
/// discard the user never read. Repeats inside the floor are ignored (the
/// original park time is kept, so a deliberate later repeat still confirms).
const NAV_CONFIRM_MIN_DELAY: Duration = Duration::from_millis(500);

/// A navigation the unsaved-changes guard intercepted (see
/// [`AppState::nav_guard`]).
#[derive(Debug, Clone)]
pub struct PendingNav {
    pub id: ConnectionId,
    pub action: NavAction,
    /// When the intent was parked (drives the double-click floor).
    parked_at: Instant,
}

impl PendingNav {
    fn new(id: ConnectionId, action: NavAction) -> Self {
        PendingNav {
            id,
            action,
            parked_at: Instant::now(),
        }
    }

    /// Whether a new attempt repeats this parked intent.
    fn matches(&self, id: ConnectionId, action: &NavAction) -> bool {
        self.id == id && self.action == *action
    }

    /// Whether the intent is old enough for a repeat to confirm it.
    fn confirmable(&self) -> bool {
        self.parked_at.elapsed() >= NAV_CONFIRM_MIN_DELAY
    }
}

/// The navigations guarded against unsaved staged edits.
#[derive(Debug, Clone, PartialEq)]
pub enum NavAction {
    SelectTable(TableRef),
    SetPane(Pane),
    CloseConnection,
}

/// One buffer ("query tab") in a connection's SQL pane. A saved query opens
/// into a new one (FRE-113), which is what keeps it from overwriting whatever
/// was in the editor.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlBuffer {
    /// Per-tab id. Monotonic and never reused, so a run or a pending write
    /// confirmation that names a buffer can only ever mean the buffer it was
    /// started from — a closed buffer's id is not handed to a later one.
    pub id: u64,
    /// The saved query this buffer was opened from, shown on its tab.
    /// `None` for a scratch buffer.
    pub title: Option<String>,
    /// The editor text, synced from the webview so it survives buffer, pane,
    /// and tab switches.
    pub text: String,
    /// A version of the document held under *this* buffer id. It moves when
    /// text is placed into the buffer from outside the editor **and the editor
    /// is already showing that buffer**: [`SqlBuffers::load`], and the
    /// [`SqlBuffers::open`] branch that reuses a buffer.
    ///
    /// Deliberately not a count of placements — a buffer `open` *creates*
    /// starts at `0` despite having been given text. Its id has never been on
    /// screen, so the id alone already tells the pane the editor's document is
    /// stale, and nothing can be in flight stamped for it.
    ///
    /// It does two jobs, both of which need it to be per buffer rather than
    /// per tab:
    ///
    /// - With the buffer id it forms [`SqlBuffers::doc_target`], the thing the
    ///   SQL pane's `setDoc` effect is gated on. The id alone cannot carry it:
    ///   opening a saved query into an untitled blank buffer reuses that
    ///   buffer, so the id stays the same and an id-gated effect never fires —
    ///   the tab renames itself and the editor stays empty.
    /// - It is handed to the editor and comes back on every message from it,
    ///   which is what lets [`SqlBuffers::set_text`] tell a reply describing
    ///   the current document from one describing a document that has since
    ///   been replaced (FRE-154).
    ///
    /// Text arriving *from* the editor deliberately does not move it: pushing
    /// a keystroke back into CodeMirror would fight the caret.
    pub doc_generation: u64,
}

impl SqlBuffer {
    /// An empty, untitled buffer.
    pub fn scratch(id: u64) -> Self {
        SqlBuffer {
            id,
            title: None,
            text: String::new(),
            doc_generation: 0,
        }
    }

    /// Whether this buffer holds nothing worth keeping — untitled and blank.
    /// Opening a saved query into such a buffer overwrites nothing, so it is
    /// reused rather than leaving an empty tab behind.
    fn is_scratch(&self) -> bool {
        self.title.is_none() && self.text.trim().is_empty()
    }
}

/// The id a tab's first SQL buffer gets.
pub const FIRST_SQL_BUFFER: u64 = 1;

/// One connection tab's SQL buffers, in tab order, plus which one is showing.
///
/// Never empty: a tab always has an editor, so closing the last buffer leaves
/// a fresh scratch one rather than nothing. That invariant is why the fields
/// are private — every mutation goes through a method that maintains it.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlBuffers {
    list: Vec<SqlBuffer>,
    active: u64,
    /// Highest id ever handed out in this tab. Ids are never reused: a run
    /// and a parked write confirmation each name their buffer by id, and
    /// handing a closed buffer's id to a later one would re-attach them to a
    /// query they did not come from.
    issued: u64,
}

impl Default for SqlBuffers {
    fn default() -> Self {
        SqlBuffers {
            list: vec![SqlBuffer::scratch(FIRST_SQL_BUFFER)],
            active: FIRST_SQL_BUFFER,
            issued: FIRST_SQL_BUFFER,
        }
    }
}

impl SqlBuffers {
    /// The buffers in tab order. Always at least one.
    pub fn list(&self) -> &[SqlBuffer] {
        &self.list
    }

    /// The buffer the SQL pane is showing.
    pub fn active(&self) -> u64 {
        self.active
    }

    /// What the editor should currently be displaying: the active buffer and
    /// that buffer's [`SqlBuffer::doc_generation`]. Either changing means the
    /// editor's document is stale, and it is also the token handed to the
    /// editor so its replies can be dated.
    pub fn doc_target(&self) -> (u64, u64) {
        (self.active, self.doc_generation(self.active))
    }

    /// One buffer's document generation, `0` for an id that is gone.
    fn doc_generation(&self, buffer: u64) -> u64 {
        self.list
            .iter()
            .find(|b| b.id == buffer)
            .map_or(0, |b| b.doc_generation)
    }

    /// One buffer's text, empty for an id that is gone.
    pub fn text(&self, buffer: u64) -> &str {
        self.list
            .iter()
            .find(|b| b.id == buffer)
            .map_or("", |b| b.text.as_str())
    }

    /// Stores one buffer's text **as reported by the editor**, reporting
    /// whether it actually changed.
    ///
    /// `generation` is the [`SqlBuffer::doc_generation`] the editor was
    /// holding when that text was typed, which it sends back with the text.
    /// Two writes are therefore dropped rather than applied:
    ///
    /// - **A buffer that is gone.** A closed tab's last message from the
    ///   webview must not resurrect it.
    /// - **A stale generation.** Something has replaced that buffer's document
    ///   since — a saved query opened into it, a history entry loaded — so the
    ///   message describes text that is no longer on screen, and applying it
    ///   would silently undo the load. This matters because the editor now
    ///   *flushes* its pending text on the way out (FRE-154) instead of
    ///   dropping it, so such a message is a message in flight rather than a
    ///   hypothetical.
    ///
    /// Note what is deliberately **not** dropped: a message for a buffer that
    /// is no longer active. Its generation still matches, because switching
    /// tabs replaces no document — and that message is precisely the tail of
    /// typing this fix exists to keep.
    pub fn set_text(&mut self, buffer: u64, generation: u64, text: String) -> bool {
        match self.list.iter_mut().find(|b| b.id == buffer) {
            Some(existing) if existing.doc_generation == generation => {
                let changed = existing.text != text;
                existing.text = text;
                changed
            }
            _ => false,
        }
    }

    /// Puts `text` into an existing buffer from *outside* the editor — the
    /// history panel's Load and Run. Ignores an id that is gone.
    ///
    /// Moving the generation is what makes this safe: the editor's reply about
    /// the text being replaced is already in flight when this runs, and
    /// without the bump [`Self::set_text`] would apply it and undo the load.
    /// The bump also moves [`Self::doc_target`], so the pane pushes the loaded
    /// text into the editor — this is the whole of the load, with no second
    /// `setDoc` at the call site to drift from it.
    pub fn load(&mut self, buffer: u64, text: String) -> bool {
        match self.list.iter_mut().find(|b| b.id == buffer) {
            Some(existing) => {
                existing.text = text;
                existing.doc_generation += 1;
                true
            }
            None => false,
        }
    }

    /// Names a buffer after the query just saved from it.
    pub fn set_title(&mut self, buffer: u64, title: String) {
        if let Some(existing) = self.list.iter_mut().find(|b| b.id == buffer) {
            existing.title = Some(title);
        }
    }

    /// Switches to another buffer. An id that is gone is ignored, so `active`
    /// always names a buffer that exists.
    pub fn select(&mut self, buffer: u64) {
        if self.list.iter().any(|b| b.id == buffer) {
            self.active = buffer;
        }
    }

    /// The next never-used id.
    fn issue(&mut self) -> u64 {
        self.issued += 1;
        self.issued
    }

    /// Puts `text` in front of the user without losing what is already there
    /// (FRE-113): a new buffer, unless the active one is an untitled blank,
    /// in which case that one is reused. Makes it active and returns its id.
    pub fn open(&mut self, title: Option<String>, text: String) -> u64 {
        let active = self.active;
        if let Some(buffer) = self
            .list
            .iter_mut()
            .find(|b| b.id == active && b.is_scratch())
        {
            buffer.title = title;
            buffer.text = text;
            // The reuse branch is the one that needs this: its buffer id does
            // not change, so the generation is all that tells the pane the
            // editor is holding a document that has been replaced.
            buffer.doc_generation += 1;
            return active;
        }
        let id = self.issue();
        self.list.push(SqlBuffer {
            id,
            title,
            text,
            doc_generation: 0,
        });
        self.active = id;
        // The new buffer's id has never been active before, so the target has
        // moved regardless of where its generation starts.
        id
    }

    /// Closes one buffer and returns the id now active. Closing the last one
    /// leaves a fresh scratch buffer — with a new id, like every other.
    pub fn close(&mut self, buffer: u64) -> u64 {
        let Some(index) = self.list.iter().position(|b| b.id == buffer) else {
            return self.active;
        };
        self.list.remove(index);
        if self.list.is_empty() {
            let id = self.issue();
            self.list.push(SqlBuffer::scratch(id));
            self.active = id;
        } else if self.active == buffer {
            // The buffer that slid into the closed one's place, or the new
            // last one.
            self.active = self.list[index.min(self.list.len() - 1)].id;
        }
        self.active
    }
}

/// Per-tab UI state that must survive tab switches.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TabUi {
    /// Table selected in the sidebar (shown in the data grid).
    pub selected_table: Option<TableRef>,
    /// Data browser, SQL editor, or schema.
    pub pane: Pane,
    /// The SQL pane's buffers ("query tabs", FRE-113) and which is showing.
    pub sql: SqlBuffers,
    /// Whether the row detail panel is docked open beside the grid
    /// (FRE-109). Per tab rather than per table, so it stays open while
    /// browsing from table to table; persisted in the session.
    pub row_detail: bool,
    /// The row detail panel's dragged width in CSS pixels, or `None` for the
    /// default. Kept here (not in the grid) so a table switch — which
    /// remounts the grid — doesn't snap the panel back. Deliberately not
    /// persisted: the session remembers whether the panel is open, and a
    /// width is cheap to re-drag.
    pub row_detail_width: Option<f64>,
}

/// Where a script run currently stands. Per-statement outcomes accumulate
/// in [`SqlRun::statements`] as they finish; the status carries the
/// script-level timing and failure info.
#[derive(Debug, Clone, PartialEq)]
pub enum RunStatus {
    Running,
    Done {
        elapsed_ms: u64,
    },
    /// The statement at `statement_index` (0-based, into the script)
    /// failed. Outcomes of the statements before it stay visible in
    /// [`SqlRun::statements`]. `rollback` says what the failure undid — an
    /// atomic run (the whole script), a run that undid nothing (the sequential
    /// path, where earlier statements persisted, or a failure before the
    /// transaction opened), or an atomic run on an engine whose rollback
    /// doesn't reach schema changes (FRE-146).
    Failed {
        error: String,
        statement_index: usize,
        preview: String,
        elapsed_ms: u64,
        rollback: Rollback,
    },
    /// The connection's capabilities forbid the statement at
    /// `statement_index` (FRE-87), so **nothing was sent**. Distinct from
    /// [`Self::Failed`]: there is no partial state to explain and no timing
    /// to report, because no statement ran.
    Refused {
        reason: String,
        statement_index: usize,
        preview: String,
    },
    /// The user aborted the run. Outcomes of the statements that finished
    /// before the abort stay visible; the in-flight statement may still
    /// complete server-side (see [`AppState::cancel_sql`]).
    Cancelled,
}

/// One finished statement's result, shared rather than owned: a result can
/// hold thousands of rows, and the editor re-reads the run on every render,
/// so cloning it must cost an `Arc` bump, not a row-by-row copy (FRE-134).
///
/// `PartialEq` is pointer identity, not a deep compare. That is sound because
/// results are write-once — pushed into [`SqlRun::statements`] and never
/// mutated — so two equal pointers are the same result and two different
/// pointers belong to different runs. It is also the point: pointer equality
/// is what lets prop diffing on the result components short-circuit without
/// walking every row (a derived `PartialEq` on `Arc` deep-compares, since
/// row values hold floats and so are only `PartialEq`, not `Eq`).
#[derive(Debug, Clone)]
pub struct SharedStatement(Arc<StatementResult>);

impl SharedStatement {
    pub fn new(result: StatementResult) -> Self {
        Self(Arc::new(result))
    }
}

impl PartialEq for SharedStatement {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for SharedStatement {
    type Target = StatementResult;

    fn deref(&self) -> &StatementResult {
        &self.0
    }
}

/// State of the most recent SQL script run in one query tab.
///
/// Keyed per `(connection, buffer)` rather than per connection: results
/// belong to the buffer that produced them, and a run started in one query
/// tab must not overwrite — or silently discard — what another tab is
/// showing. That also keeps each tab's own Cancel button reachable while its
/// script is in flight, whichever tab is in front.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlRun {
    /// Outcomes of the statements that finished, in script order.
    pub statements: Vec<SharedStatement>,
    pub status: RunStatus,
    /// Whether Explain started this run (FRE-119), so its results render as
    /// query plans instead of result grids.
    ///
    /// A property of the run rather than of the buffer: the same query tab
    /// alternates between running a statement and explaining it, and what is
    /// on screen belongs to whichever was asked for last.
    pub explain: bool,
}

/// Which pane an export belongs to. Statuses are keyed per connection AND
/// per pane so a grid export and a SQL-result export never overwrite each
/// other's toolbar line (FRE-73).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportPane {
    Grid,
    Sql,
}

/// Progress of the most recent export in one pane of one connection.
/// Shown as a small transient line in the pane's toolbar; it stays until the
/// next export replaces it.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportStatus {
    Running,
    Done { rows: u64 },
    Failed(String),
}

impl ExportStatus {
    /// The toolbar line for this status: display text plus a Tailwind color
    /// class. Shared by the grid and SQL-editor export controls.
    pub fn line(&self) -> (String, &'static str) {
        match self {
            ExportStatus::Running => (
                "Exporting…".to_string(),
                "text-slate-500 dark:text-slate-400",
            ),
            ExportStatus::Done { rows } => (
                format!("Exported {rows} row{}", if *rows == 1 { "" } else { "s" }),
                "text-emerald-700 dark:text-emerald-400",
            ),
            ExportStatus::Failed(err) => (
                format!("Export failed: {err}"),
                "text-red-600 dark:text-red-400",
            ),
        }
    }
}

/// A script held back by the write-confirmation banner: the original text
/// (recorded into history when the run happens) plus its split statements.
#[derive(Debug, Clone, PartialEq)]
/// Keyed per `(connection, buffer)` like [`SqlRun`], so the banner is shown
/// under the buffer the script came from and confirming always means the text
/// on screen.
pub struct PendingSql {
    pub script: String,
    pub statements: Vec<String>,
    /// Carried across the confirmation so the run the user confirms is still
    /// the Explain they asked for (FRE-119).
    pub explain: bool,
}

/// The outcome of the most recent saved-query write (FRE-113), shown as one
/// line in the saved-queries panel until the next one replaces it.
#[derive(Debug, Clone, PartialEq)]
pub enum SavedStatus {
    /// Stored under this name; `replaced` when it overwrote an existing
    /// query of the same name in the same scope.
    Saved {
        name: String,
        replaced: bool,
    },
    Failed(String),
}

impl SavedStatus {
    /// The panel line for this status: display text plus a Tailwind color
    /// class, in the shape [`ExportStatus::line`] established.
    pub fn line(&self) -> (String, &'static str) {
        match self {
            // "Updated" rather than "Saved" when something was overwritten:
            // the user is entitled to know a name they reused was already
            // taken.
            SavedStatus::Saved { name, replaced } => (
                format!("{} “{name}”", if *replaced { "Updated" } else { "Saved" }),
                "text-emerald-700 dark:text-emerald-400",
            ),
            SavedStatus::Failed(err) => (
                format!("Save failed: {err}"),
                "text-red-600 dark:text-red-400",
            ),
        }
    }
}

/// What a pending [`PasswordPrompt`] is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The database password.
    DbPassword,
    /// The passphrase decrypting the SSH tunnel's key file.
    SshPassphrase,
}

/// A pending secret request for a saved Postgres or SQL Server connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordPrompt {
    pub url: String,
    pub name: String,
    pub kind: PromptKind,
    /// Which backend the retry must reconnect with (Postgres or SqlServer —
    /// SQLite never prompts).
    pub backend: BackendKind,
    /// Tunnel settings of the connect attempt, carried through the prompt so
    /// the retry resumes the same flow.
    pub tunnel: Option<TunnelConfig>,
    /// Auth mode of the attempt, carried through so the retry (e.g. after an
    /// SSH passphrase) resumes the same Entra/password flow.
    pub auth: ServerAuth,
}

/// A pending host-key trust decision for a tunneled Postgres or SQL Server
/// connect. The server presented a key not yet in known_hosts; the connect is
/// parked here until the user trusts it (persist + retry) or cancels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub url: String,
    pub name: String,
    /// Tunnel settings of the attempt, carried through so the retry resumes.
    pub tunnel: TunnelConfig,
    /// The offered key: what to display and, on trust, what to persist.
    pub info: HostKeyInfo,
    /// Auth mode of the attempt, carried through so the retry resumes it.
    pub auth: ServerAuth,
    /// Which backend the retry must reconnect with.
    pub backend: BackendKind,
}

/// A pending interactive Microsoft Entra sign-in for a Postgres or SQL Server
/// connection: the connect needs a browser sign-in (no cached refresh token),
/// so it's parked here until the user starts the sign-in or cancels (FRE-44).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraPrompt {
    pub url: String,
    pub name: String,
    pub tunnel: Option<TunnelConfig>,
    pub entra: EntraAuth,
    /// Which backend the sign-in retry must reconnect with (routes the token's
    /// OAuth resource and the driver, FRE-58).
    pub backend: BackendKind,
}

/// Which phase of a connect is running, so the connections list can say more
/// than "working". The slow, hang-prone steps are the ones worth naming: a
/// tunnel to an unreachable jump host and a locked keyring both stall for a
/// long time, and "connecting…" alone gives the user nothing to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// Bringing up the SSH tunnel the connection goes through.
    Tunnel,
    /// Reading the saved password from this session or the OS keyring — a
    /// locked wallet blocks here until the user answers its own dialog.
    Credentials,
    /// Acquiring a Microsoft Entra token.
    SigningIn,
    /// Opening the database connection itself.
    Opening,
}

impl ConnectStep {
    /// Lowercase, trailing ellipsis: this renders as a status line under the
    /// connection name, not as a sentence.
    pub fn label(self) -> &'static str {
        match self {
            ConnectStep::Tunnel => "opening SSH tunnel…",
            ConnectStep::Credentials => "reading saved password…",
            ConnectStep::SigningIn => "signing in…",
            ConnectStep::Opening => "connecting…",
        }
    }
}

/// The key a connect is tracked under: the locator as an open tab would
/// report it (what [`AppState::connect`] stores in `open_locators`). Only
/// SQLite differs from the saved form — its locator is the canonicalized
/// path, so a row keyed on the saved spelling would never match its own
/// in-flight progress.
pub fn connect_key(locator: &str, backend: BackendKind) -> String {
    match backend {
        BackendKind::Sqlite => canonical(Path::new(locator)).display().to_string(),
        _ => locator.to_string(),
    }
}

/// A connect in flight, from the reservation until the tab opens or the
/// attempt fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connecting {
    /// Canonical locator, the same key [`AppState::open_locators`] uses.
    pub locator: String,
    pub step: ConnectStep,
    /// Whether the row should show progress yet. Stays false for
    /// [`SPINNER_DELAY`] so a local SQLite open — which finishes in
    /// milliseconds — never flashes a spinner.
    pub visible: bool,
}

/// A connect started from the connections list, as opposed to one started by
/// submitting a form: the list can cancel it, and a shift-click asks for the
/// tab to open without stealing focus.
#[derive(Clone, Copy)]
pub struct ConnectRequest {
    /// Root-scope task running the connect. Cancelling drops it mid-await,
    /// which drops any half-open tunnel with it.
    task: Task,
    /// False for a shift-click: open the tab, but stay on the list.
    focus: bool,
}

/// App-wide state provided via context. `Copy` because it only holds signals.
#[derive(Clone, Copy)]
pub struct AppState {
    pub registry: Signal<ConnectionRegistry>,
    pub active: Signal<ActiveView>,
    /// Saved connections shown on the launch screen.
    pub saved: Signal<SavedList>,
    /// Locator (canonical file path / URL) each open tab came from, for
    /// "already open" detection.
    pub open_locators: Signal<Vec<(ConnectionId, String)>>,
    /// Connects in flight, reserved before the pool open await. Drives the
    /// connections list's per-row progress.
    pub connecting: Signal<Vec<Connecting>>,
    /// Cancel handle and focus intent for connects started from the
    /// connections list, keyed by locator. Connects started by submitting a
    /// form have no entry — they still show progress, just no cancel.
    pub connect_requests: Signal<HashMap<String, ConnectRequest>>,
    /// Error from the most recent connect/config operation, shown on the
    /// connections screen.
    pub connect_error: Signal<Option<String>>,
    /// Secrets entered this session: Postgres passwords keyed by stored URL,
    /// SSH key passphrases keyed by [`connect::ssh_secret_key`]. Never
    /// persisted here — the OS keyring handles "remember".
    pub session_passwords: Signal<HashMap<String, String>>,
    /// When set, the connections screen asks for this connection's password
    /// or SSH key passphrase.
    pub password_prompt: Signal<Option<PasswordPrompt>>,
    /// When set, the connections screen asks the user to trust an unrecognized
    /// SSH host key before the tunneled connect proceeds (trust-on-first-use).
    pub host_key_prompt: Signal<Option<HostKeyPrompt>>,
    /// When set, the connections screen offers to start an interactive Entra
    /// browser sign-in for a Postgres connect that needs one (FRE-44).
    pub entra_prompt: Signal<Option<EntraPrompt>>,
    /// Set when the user tries to close the window with unsaved staged edits:
    /// the close is vetoed and a discard-and-quit confirmation is shown
    /// (FRE-37). Cleared on cancel or once the user discards and quits.
    pub confirm_quit: Signal<bool>,
    /// Live SSH tunnels, one per tunneled open connection. Removing an entry
    /// drops the [`Tunnel`], which shuts the forward down.
    pub tunnels: Signal<HashMap<ConnectionId, Tunnel>>,
    /// Locators whose SSH key passphrase the user asked to remember, waiting
    /// for a tunnel open to accept it (FRE-161).
    ///
    /// The choice is made at the passphrase prompt but cannot be acted on
    /// there: FRE-151 established that only a passphrase a tunnel open has
    /// **accepted** may reach the keyring, because `open_tunnel` reads a
    /// keyring hit as "previously validated" and deletes it when the server
    /// rejects it. So the choice is parked here and redeemed at the one place
    /// that learns the passphrase was good.
    ///
    /// An intent rather than an argument because the connect that validates
    /// the passphrase is frequently **not** the one the choice was made on: an
    /// untrusted host key, a database password prompt and an Entra sign-in
    /// each park the attempt and resume it from somewhere else. Threading a
    /// `remember` flag through all of those means getting all of them right;
    /// this way there is one producer and one consumer.
    ///
    /// An entry is consumed on acceptance, and replaced by whatever the next
    /// prompt is answered with — so a later "don't remember" is not overridden
    /// by an earlier attempt that said otherwise.
    pub ssh_remember: Signal<HashSet<String>>,
    /// Introspected schema per open connection.
    pub schemas: Signal<HashMap<ConnectionId, SchemaLoad>>,
    /// Sidebar/grid UI state per open connection.
    pub tab_ui: Signal<HashMap<ConnectionId, TabUi>>,
    /// Staged (unsaved) edits per connection, keyed by [`TableRef::key`].
    /// FRE-24/25 push changes in via [`Self::stage_cell_edit`], the
    /// `stage_insert_*` family, and [`Self::stage_delete`];
    /// [`Self::save_staged`] applies a table's stage in one transaction.
    pub staged: Signal<HashMap<ConnectionId, HashMap<String, TableStage>>>,
    /// Per-table grid refresh nonce, keyed by (connection,
    /// [`TableRef::key`]). Lifted out of the grid component so a successful
    /// save can force a refetch; the grid's Refresh button bumps it too.
    /// (Signal subscription is per-signal, so any write here wakes every
    /// mounted grid's readers — the grid therefore reads its own entry
    /// through a PartialEq-gated memo, and only the bumped table's grid
    /// actually resets/refetches; FRE-129.)
    pub grid_refresh: Signal<HashMap<(ConnectionId, String), u64>>,
    /// Two-step unsaved-changes guard. Navigating away from staged edits
    /// (selecting another table, switching panes, closing the tab) does not
    /// happen immediately: the first attempt parks the intent here and the
    /// grid's Save bar shows a blocking notice; repeating the *same* action
    /// at least [`NAV_CONFIRM_MIN_DELAY`] later (double-click protection)
    /// discards the affected stage(s) and proceeds. Any other action —
    /// saving, discarding, staging more edits, or a different navigation —
    /// replaces or clears the parked intent. While a save is in flight the
    /// guarded navigations no-op entirely: discarding then would race the
    /// running transaction.
    ///
    /// Closing the OS window with staged edits is guarded separately by
    /// [`Self::any_dirty`] + [`Self::confirm_quit`] (FRE-37), which raises a
    /// discard-and-quit confirmation rather than losing the edits silently.
    pub nav_guard: Signal<Option<PendingNav>>,
    /// Latest free-form SQL result per query tab (FRE-113).
    pub sql_runs: Signal<HashMap<(ConnectionId, u64), SqlRun>>,
    /// Scripts containing writes, held here until the user confirms (or
    /// dismisses) the write-confirmation banner, per query tab.
    pub pending_sql: Signal<HashMap<(ConnectionId, u64), PendingSql>>,
    /// Staged saves waiting on the FRE-111 confirmation, one per table, each
    /// holding the exact change list the prompt named.
    ///
    /// Only populated for connections marked [`WriteProtection::Confirm`];
    /// an unmarked connection saves straight through as before. The staged
    /// changes themselves stay in [`Self::staged`] — this holds only a copy,
    /// so dismissing loses nothing, and the copy is what makes a confirmation
    /// stale once the stage moves on (see `take_pending_save`).
    pub pending_saves: Signal<HashMap<(ConnectionId, String), Vec<StagedChange>>>,
    /// Per-connection accent colour (FRE-111), seeded from the saved entry at
    /// connect time.
    ///
    /// Kept beside the registry rather than inside it because a colour warns
    /// and never enforces: nothing in `db/` should be able to read it and act
    /// on it. Its enforcing counterpart, [`WriteProtection`], lives on
    /// [`Connection`] for the opposite reason.
    pub connection_colors: Signal<HashMap<ConnectionId, ConnectionColor>>,
    /// Handle of the in-flight run per query tab, kept so the Cancel button
    /// can abort it. Entries are removed when a run completes — and cancelled
    /// when the query tab or the connection closes, since a run nothing can
    /// reach still burns a core and pins a pool connection.
    pub sql_tasks: Signal<HashMap<(ConnectionId, u64), Task>>,
    /// Stale-run guard: each started run gets the next generation number,
    /// and a completing task only writes its result while its generation is
    /// still current.
    pub sql_generations: Signal<HashMap<(ConnectionId, u64), u64>>,
    /// Persisted query-history store, opened in the background at startup.
    /// `None` while opening or when opening failed (see
    /// [`Self::history_error`]) — history is best-effort and the app works
    /// without it.
    pub history: Signal<Option<HistoryStore>>,
    /// Why the history store is unavailable, shown in the history panel.
    pub history_error: Signal<Option<String>>,
    /// A failed history write for an otherwise-successful run (FRE-72).
    /// Shown as a dismissible banner above the (still readable) history
    /// list; cleared by the next record attempt that reaches the store.
    pub history_record_error: Signal<Option<String>>,
    /// Bumped whenever a run lands in (or is cleared from) the history
    /// store, so open history panels re-query.
    pub history_nonce: Signal<u64>,
    /// UI mirror of the store's persisted recording flag.
    pub history_recording: Signal<bool>,
    /// Bumped whenever a saved query is written or deleted (FRE-113), so open
    /// saved-query panels re-query. Separate from [`Self::history_nonce`]:
    /// running a script must not make every open panel refetch a list that
    /// cannot have changed.
    pub saved_nonce: Signal<u64>,
    /// Outcome of the most recent saved-query write, per connection. Keyed
    /// like [`Self::export_status`] (FRE-73): one connection's "Saved" line
    /// has no business appearing in another connection's panel.
    pub saved_status: Signal<HashMap<ConnectionId, SavedStatus>>,
    /// The persisted theme choice (System / Light / Dark). The toggle cycles
    /// it; [`Self::set_theme`] persists it to settings.toml.
    pub theme: Signal<Theme>,
    /// Resolved dark/light, driving the root `.dark` class. Derived from
    /// `theme` and — for `System` — a one-time startup read of the OS
    /// preference; written from the startup detection task.
    pub dark: Signal<bool>,
    /// Whether the schema sidebar lists the objects a backend declared
    /// internal (FRE-88). Persisted by
    /// [`Self::set_show_internal_objects`]; global rather than
    /// per-connection, since it expresses what the user wants to look at
    /// rather than anything about one database.
    pub show_internal_objects: Signal<bool>,
    /// Connection groups the user has collapsed in the connections list
    /// (FRE-120), by name. Seeded from session.toml at restore and written
    /// back with the rest of the session: a fold is view state, not
    /// connection data (see [`Session::collapsed_groups`]).
    pub collapsed_groups: Signal<Vec<String>>,
    /// A saved-connection edit awaiting the connect that confirms it
    /// (FRE-75). The Entra sign-in card resolves it from a background task
    /// after the form has closed.
    pub pending_edit: Signal<Option<PendingEdit>>,
    /// Latest export progress per connection and pane. Written from the
    /// `spawn_forever` export task.
    pub export_status: Signal<HashMap<(ConnectionId, ExportPane), ExportStatus>>,
    /// Monotonic id per export slot; a finishing task only records its
    /// outcome while it is still the slot's latest export, so a slow export
    /// can't overwrite the status of one started after it (FRE-73).
    pub export_generations: Signal<HashMap<(ConnectionId, ExportPane), u64>>,
    /// Pending foreign-key focus per connection (FRE-29): a target table plus
    /// the filter to seed. Set by [`Self::navigate_fk`] / [`Self::navigate_back`]
    /// right before the target grid is selected; the grid consumes the entry
    /// matching its table (on mount, or live for a same-table jump) and clears
    /// it. At most one is pending per connection.
    pub pending_focus: Signal<HashMap<ConnectionId, FocusTarget>>,
    /// Back stack per connection (FRE-29): the views left behind by forward
    /// foreign-key jumps, most recent last. [`Self::navigate_back`] pops one;
    /// a manual table selection clears the stack.
    pub nav_history: Signal<HashMap<ConnectionId, Vec<FocusTarget>>>,
    /// Latest import progress per connection (FRE-112). Written from the
    /// `spawn_forever` import task. Keyed per connection only — an
    /// import is always started from the grid, so there is no second pane to
    /// tell apart.
    pub import_status: Signal<HashMap<ConnectionId, ImportStatus>>,
    /// Monotonic id per connection's import, so a slow import cannot record
    /// its outcome over one started after it — the same guard the export
    /// statuses carry.
    pub import_generations: Signal<HashMap<ConnectionId, u64>>,
    /// The in-flight import's task per connection, so closing the connection
    /// can cancel it rather than leaving it to commit into a pool that is
    /// being torn down. Held for the same reason [`Self::sql_tasks`] is.
    pub import_tasks: Signal<HashMap<ConnectionId, Task>>,
    /// Whether the keyboard-shortcut cheatsheet overlay is showing (FRE-15).
    /// App-global (one overlay for the whole window), toggled by `?` and
    /// dismissed by Escape / a backdrop click / `?` again.
    pub show_cheatsheet: Signal<bool>,
}

/// Finds one table's metadata in a loaded schema — the single
/// `(schema, name)` match behind every "which table is this?" lookup in the
/// app.
///
/// Borrows out of the caller's signal guard, so a render path can read a
/// field without cloning the whole [`TableMeta`]; the async paths use
/// [`AppState::table_meta`], which clones.
pub fn find_table_meta<'a>(
    load: Option<&'a SchemaLoad>,
    table: &TableRef,
) -> Option<&'a TableMeta> {
    match load? {
        SchemaLoad::Ready(tables) => tables
            .iter()
            .find(|t| t.name == table.name && t.schema == table.schema),
        _ => None,
    }
}

impl AppState {
    /// Must be called from a component (signals need the runtime).
    pub fn new() -> Self {
        // A failed config load surfaces on the launch screen and the app
        // starts with an empty list; SavedList remembers the failure and
        // refuses to persist over the unreadable file.
        let (saved, load_error) = match default_config_path() {
            Some(path) => {
                let (list, error) = SavedList::load(&path);
                (list, error.map(|e| e.to_string()))
            }
            None => (
                SavedList::load(Path::new("/nonexistent")).0,
                Some("no config directory found".to_string()),
            ),
        };
        // UI preferences: best-effort load, defaults on any problem (settings
        // are non-critical and never block the app).
        let settings = default_settings_path()
            .map(|path| load_settings(&path))
            .unwrap_or_default();
        let theme = settings.theme;
        // Every signal is root-scoped, without exception.
        //
        // `AppState` is app-global by construction: `App` provides exactly one
        // via `use_context_provider`, and it lives as long as the process. But
        // it is *created* inside that component's scope, which is not an
        // ancestor of the root scope the `spawn_forever` tasks run in — so a
        // component-scoped field read or written from one of those tasks trips
        // `__copy_value_hoisted` and can fail once the creating scope drops.
        //
        // These were scoped field by field, and that judgement does not
        // survive contact with the code. Six previously component-scoped
        // fields are named *directly* inside a `spawn_forever` body, and only
        // three of those had been noticed (FRE-156) — but a task that calls a
        // method that touches a signal reaches it just as surely, and nothing
        // at the call site shows it. Following those calls, 18 of the 27 were
        // reachable. One connect handler alone reaches twelve.
        //
        // Scoping them all the same way retires the question instead of
        // re-answering it per field, per task, and per intervening call — and
        // costs nothing, since the alternative owner outlives the app too.
        // (A few fields, `show_internal_objects` among them, were genuinely
        // unreachable and correctly reasoned about. Uniformity is still worth
        // more than a correct exception that has to be re-verified forever.)
        //
        // `every_app_state_signal_is_root_scoped` enforces this.
        let state = Self {
            registry: Signal::new_in_scope(ConnectionRegistry::default(), ScopeId::ROOT),
            active: Signal::new_in_scope(ActiveView::Connections, ScopeId::ROOT),
            saved: Signal::new_in_scope(saved, ScopeId::ROOT),
            open_locators: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            connecting: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            connect_requests: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            connect_error: Signal::new_in_scope(load_error, ScopeId::ROOT),
            session_passwords: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            password_prompt: Signal::new_in_scope(None, ScopeId::ROOT),
            host_key_prompt: Signal::new_in_scope(None, ScopeId::ROOT),
            entra_prompt: Signal::new_in_scope(None, ScopeId::ROOT),
            confirm_quit: Signal::new_in_scope(false, ScopeId::ROOT),
            tunnels: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            ssh_remember: Signal::new_in_scope(HashSet::new(), ScopeId::ROOT),
            schemas: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            tab_ui: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            staged: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            grid_refresh: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            nav_guard: Signal::new_in_scope(None, ScopeId::ROOT),
            sql_runs: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            pending_sql: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            pending_saves: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            connection_colors: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            sql_tasks: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            sql_generations: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            history: Signal::new_in_scope(None, ScopeId::ROOT),
            history_error: Signal::new_in_scope(None, ScopeId::ROOT),
            history_record_error: Signal::new_in_scope(None, ScopeId::ROOT),
            history_nonce: Signal::new_in_scope(0, ScopeId::ROOT),
            history_recording: Signal::new_in_scope(true, ScopeId::ROOT),
            saved_nonce: Signal::new_in_scope(0, ScopeId::ROOT),
            saved_status: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            theme: Signal::new_in_scope(theme, ScopeId::ROOT),
            // `false`: assume a light system default until the startup
            // detection task below resolves the real one. An explicit
            // Light/Dark choice overrides it either way.
            dark: Signal::new_in_scope(theme.resolve_dark(false), ScopeId::ROOT),
            show_internal_objects: Signal::new_in_scope(
                settings.show_internal_objects,
                ScopeId::ROOT,
            ),
            // Seeded by `restore_session` rather than here: session.toml is
            // read once, at restore, and reading it twice would be two
            // chances to disagree about what the last session was.
            collapsed_groups: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            pending_edit: Signal::new_in_scope(None, ScopeId::ROOT),
            export_status: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            export_generations: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            import_status: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            import_generations: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            import_tasks: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            pending_focus: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            nav_history: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            show_cheatsheet: Signal::new_in_scope(false, ScopeId::ROOT),
        };
        // Resolve the OS dark-mode preference once at startup. Reacting to
        // live OS theme changes is out of scope (a startup read suffices);
        // an explicit Light/Dark choice overrides it regardless.
        let mut state_for_theme = state;
        spawn_forever(async move {
            if *state_for_theme.theme.peek() != Theme::System {
                return;
            }
            let prefers_dark = system_prefers_dark().await;
            // Guard against a toggle landing before the read resolved.
            if *state_for_theme.theme.peek() == Theme::System {
                state_for_theme.dark.set(prefers_dark);
            }
        });
        // Open the history store in the background; a failure only disables
        // the history panel, never the app. spawn_forever: the opening must
        // not be tied to whichever component happened to create the state.
        let mut state_for_open = state;
        spawn_forever(async move {
            match HistoryStore::open().await {
                Ok(store) => {
                    let enabled = store.recording_enabled().await.unwrap_or(true);
                    state_for_open.history_recording.set(enabled);
                    state_for_open.history.set(Some(store));
                }
                Err(err) => state_for_open.history_error.set(Some(err)),
            }
        });
        // Session restore is triggered once from the Shell component (see
        // `Shell`), not here: it drives the normal `connect`/`connect_server`
        // flow, and doing it there keeps it beside the manual connect path it
        // mirrors.
        //
        // It used to be justified by scope — the signals it writes were owned
        // by `App`, so a root task here would have written them from a foreign
        // scope. That reason is gone: every signal above is root-scoped now,
        // and `ScopeId::ROOT` is an ancestor of every scope, so no reader is
        // foreign to them. The placement is a structural preference now, not a
        // constraint.
        state
    }

    /// This connection's *effective* capabilities resolved for one object —
    /// the backend's (FRE-87) narrowed by the user's marking (FRE-111).
    /// `None` when the connection is gone.
    ///
    /// Every UI gate reads through here so the disabled button, the refused
    /// script and the rejected save all state one reason from one resolution.
    pub fn table_access(&self, id: ConnectionId, table: &TableMeta) -> Option<TableAccess> {
        self.registry.read().get(id).map(|c| c.access(table))
    }

    /// This connection's effective capabilities, ignoring any single object.
    pub fn connection_caps(&self, id: ConnectionId) -> Option<Capabilities> {
        self.registry.read().get(id).map(Connection::capabilities)
    }

    /// Whether writes through this connection must be confirmed first
    /// (FRE-111). False for a connection that is gone.
    pub fn confirms_writes(&self, id: ConnectionId) -> bool {
        self.registry
            .read()
            .get(id)
            .is_some_and(Connection::confirms_writes)
    }

    /// Whether the *user's marking* — rather than the backend — is what makes
    /// this connection unwritable (FRE-111), so the UI can name the right
    /// culprit. False when the engine refuses writes on its own: then the
    /// marking changed nothing and mustn't claim the credit.
    pub fn marked_read_only(&self, id: ConnectionId) -> bool {
        self.registry.read().get(id).is_some_and(|connection| {
            connection.protection == WriteProtection::ReadOnly
                && connection.pool.backend_capabilities().mutate
        })
    }

    /// This connection's accent colour, if the user set one (FRE-111).
    pub fn connection_color(&self, id: ConnectionId) -> Option<ConnectionColor> {
        self.connection_colors.read().get(&id).copied()
    }

    /// Introspects (or re-introspects) one connection's schema in the
    /// background. The pool is cloned out of the registry before the await;
    /// no signal borrow is held across it.
    pub fn load_schema(mut self, id: ConnectionId) {
        let Some(pool) = self.registry.read().get(id).map(|c| c.pool.clone()) else {
            return;
        };
        self.schemas.write().insert(id, SchemaLoad::Loading);
        // spawn_forever: a plain spawn would tie the task to the calling
        // component's scope, and connect() runs in the connections screen,
        // which unmounts (cancelling its tasks) the moment the view switches
        // to the new tab.
        spawn_forever(async move {
            let loaded = match pool.introspect().await {
                Ok(tables) => SchemaLoad::Ready(tables),
                Err(err) => SchemaLoad::Failed(err.to_string()),
            };
            // The tab may have been closed while introspecting.
            if self.registry.read().get(id).is_some() {
                self.schemas.write().insert(id, loaded);
            }
        });
    }

    /// Switches a tab between the data browser, the SQL editor, and the
    /// schema view. Guarded: leaving the browser while the selected table
    /// has staged edits takes two attempts (see [`Self::nav_guard`]); the
    /// second discards them. While that table's save is in flight the
    /// switch no-ops.
    pub fn set_pane(mut self, id: ConnectionId, pane: Pane) {
        let (current_pane, selected) = {
            let tab_ui = self.tab_ui.read();
            let ui = tab_ui.get(&id);
            (
                ui.map(|ui| ui.pane).unwrap_or_default(),
                ui.and_then(|ui| ui.selected_table.clone()),
            )
        };
        if current_pane == pane {
            return;
        }
        if current_pane == Pane::Browser {
            if let Some(table) = &selected {
                if self.stage_dirty(id, table) {
                    if self.stage_saving(id, table) {
                        return;
                    }
                    if !self.nav_guard_allows(id, NavAction::SetPane(pane)) {
                        return;
                    }
                    self.discard_staged(id, table);
                }
            }
        }
        self.nav_guard.set(None);
        self.tab_ui.write().entry(id).or_default().pane = pane;
    }

    /// Cycles the currently active connection tab through Data → SQL →
    /// Schema (FRE-15's `Ctrl+E` shortcut, extended for the third pane in
    /// FRE-69). A no-op on the connections screen. Routed through
    /// [`Self::set_pane`], so the unsaved-changes guard still applies when
    /// leaving the browser with staged edits.
    pub fn toggle_active_pane(self) {
        let ActiveView::Connection(id) = *self.active.read() else {
            return;
        };
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .map(|ui| ui.pane)
            .unwrap_or_default();
        // Cycles rather than toggles now that there are three panes (FRE-69).
        let target = match current {
            Pane::Browser => Pane::Sql,
            Pane::Sql => Pane::Schema,
            Pane::Schema => Pane::Browser,
        };
        self.set_pane(id, target);
    }

    /// Whether one tab has the row detail panel docked open (FRE-109).
    pub fn row_detail_open(&self, id: ConnectionId) -> bool {
        self.tab_ui.read().get(&id).is_some_and(|ui| ui.row_detail)
    }

    /// Opens or closes one tab's row detail panel. Unlike a pane switch this
    /// is unguarded: the panel is docked beside the grid rather than replacing
    /// it, so closing it navigates nowhere and can't strand staged edits.
    pub fn set_row_detail(mut self, id: ConnectionId, open: bool) {
        self.tab_ui.write().entry(id).or_default().row_detail = open;
    }

    /// Toggles the active tab's row detail panel (FRE-109's shortcut). A no-op
    /// on the connections screen.
    pub fn toggle_row_detail(self) {
        let ActiveView::Connection(id) = *self.active.read() else {
            return;
        };
        self.set_row_detail(id, !self.row_detail_open(id));
    }

    /// The row detail panel's remembered width for one tab, or `None` for the
    /// default (FRE-109).
    pub fn row_detail_width(&self, id: ConnectionId) -> Option<f64> {
        self.tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.row_detail_width)
    }

    /// Remembers the width the panel was dragged to. Written once per drag
    /// (on release), not per frame — the drag itself moves the DOM node.
    pub fn set_row_detail_width(mut self, id: ConnectionId, width: f64) {
        self.tab_ui.write().entry(id).or_default().row_detail_width = Some(width);
    }

    /// Flips the shortcut cheatsheet overlay (FRE-15, the `?` shortcut).
    pub fn toggle_cheatsheet(mut self) {
        let showing = *self.show_cheatsheet.read();
        self.show_cheatsheet.set(!showing);
    }

    /// Closes the shortcut cheatsheet if it is open (FRE-15, Escape). Kept
    /// idempotent so a stray Escape never churns the signal.
    pub fn close_cheatsheet(mut self) {
        if *self.show_cheatsheet.read() {
            self.show_cheatsheet.set(false);
        }
    }

    /// Sets the theme: updates the resolved `dark` signal immediately and
    /// persists the choice to settings.toml (best-effort — a write failure
    /// only means the choice won't survive a restart, never an error). For
    /// `System` the resolution re-reads the OS preference in the background.
    pub fn set_theme(mut self, theme: Theme) {
        self.theme.set(theme);
        match theme {
            Theme::Light => self.dark.set(false),
            Theme::Dark => self.dark.set(true),
            Theme::System => {
                let mut state = self;
                spawn_forever(async move {
                    let prefers_dark = system_prefers_dark().await;
                    if *state.theme.peek() == Theme::System {
                        state.dark.set(prefers_dark);
                    }
                });
            }
        }
        // Persist off the UI path; no signal borrow is held across the write.
        // `save_theme` merges into the existing file so the window geometry
        // (FRE-30, same settings.toml) isn't clobbered.
        let Some(path) = default_settings_path() else {
            return;
        };
        spawn_forever(async move {
            let _ = save_theme(&path, theme);
        });
    }

    /// Shows or hides the database's own internal objects in the sidebar
    /// (FRE-88) and persists the choice, best-effort like
    /// [`Self::set_theme`].
    pub fn set_show_internal_objects(mut self, show: bool) {
        self.show_internal_objects.set(show);
        let Some(path) = default_settings_path() else {
            return;
        };
        spawn_forever(async move {
            let _ = save_show_internal_objects(&path, show);
        });
    }

    /// Marks a table as selected in one tab's sidebar. Guarded: switching
    /// away from a table with staged edits takes two attempts (see
    /// [`Self::nav_guard`]); the second discards them and switches. While
    /// that table's save is in flight the switch no-ops.
    ///
    /// A manual selection clears the foreign-key Back stack and any pending
    /// focus — the FK trail only makes sense relative to the jumps that built
    /// it, not to an unrelated sidebar pick.
    pub fn select_table(self, id: ConnectionId, table: &TableRef) {
        self.switch_table(id, table, true);
    }

    /// Switches the selected table, running the unsaved-changes guard. Returns
    /// whether the selection actually changed: a no-op (already selected) or a
    /// parked/blocked guard returns `false`. `clear_fk_nav` drops this
    /// connection's FK Back stack and pending focus on a real switch —
    /// foreign-key jumps and Back pass `false` so they manage that stack
    /// themselves.
    fn switch_table(mut self, id: ConnectionId, table: &TableRef, clear_fk_nav: bool) -> bool {
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.selected_table.clone());
        if current.as_ref() == Some(table) {
            return false;
        }
        if let Some(current_table) = &current {
            if self.stage_dirty(id, current_table) {
                if self.stage_saving(id, current_table) {
                    return false;
                }
                if !self.nav_guard_allows(id, NavAction::SelectTable(table.clone())) {
                    return false;
                }
                self.discard_staged(id, current_table);
            }
        }
        self.nav_guard.set(None);
        self.tab_ui.write().entry(id).or_default().selected_table = Some(table.clone());
        if clear_fk_nav {
            self.nav_history.write().remove(&id);
            self.pending_focus.write().remove(&id);
        }
        true
    }

    /// Whether this connection has anywhere to go Back to (a non-empty FK
    /// Back stack). Drives the grid's Back button visibility.
    pub fn can_go_back(&self, id: ConnectionId) -> bool {
        self.nav_history
            .read()
            .get(&id)
            .is_some_and(|stack| !stack.is_empty())
    }

    /// Follows a foreign key from `origin` to the row it references (FRE-29):
    /// resolves the target table, builds the multi-equality filter that pins
    /// the referenced row from `source_row`, records the origin view on the
    /// Back stack, and selects the target (seeding its filter through
    /// [`Self::pending_focus`]).
    ///
    /// A no-op when the jump can't be built — any FK column's source value is
    /// missing or NULL (a NULL foreign key references nothing), or a referenced
    /// column can't be resolved. Guarded like any table switch: leaving a table
    /// with unsaved edits takes the usual second confirming click (a
    /// self-referencing FK only changes the filter, so it is never guarded).
    pub fn navigate_fk(
        mut self,
        id: ConnectionId,
        fk: &ForeignKeyMeta,
        source_row: &HashMap<String, Value>,
        origin: &TableRef,
        origin_filter: Option<Filter>,
    ) {
        let target = TableRef {
            schema: fk.referenced_schema.clone(),
            name: fk.referenced_table.clone(),
        };
        // The target PK is only consulted for FK columns that reference the
        // target's implicit primary key (`referenced_columns[i] == None`).
        let target_pk = self.table_primary_key(id, &target);
        let Some(filter) = build_fk_filter(fk, source_row, &target_pk) else {
            return;
        };
        let focus = FocusTarget {
            table: target.clone(),
            filter: Some(filter),
        };
        let origin_focus = FocusTarget {
            table: origin.clone(),
            filter: origin_filter,
        };
        self.pending_focus.write().insert(id, focus);
        if &target == origin {
            // Self-referencing FK: the grid stays mounted and just refocuses
            // its filter (consumed by its pending-focus effect). Record the
            // origin so Back can restore the prior filter.
            self.nav_history
                .write()
                .entry(id)
                .or_default()
                .push(origin_focus);
        } else if self.switch_table(id, &target, false) {
            self.nav_history
                .write()
                .entry(id)
                .or_default()
                .push(origin_focus);
        }
    }

    /// Returns to the most recent view on the FK Back stack (FRE-29),
    /// restoring its table and filter. A no-op when the stack is empty. The
    /// entry is only popped once the return actually happens, so a staged-edit
    /// guard parking the switch keeps it for the confirming click.
    pub fn navigate_back(mut self, id: ConnectionId) {
        let target = self
            .nav_history
            .read()
            .get(&id)
            .and_then(|stack| stack.last().cloned());
        let Some(target) = target else {
            return;
        };
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.selected_table.clone());
        let dest = target.table.clone();
        self.pending_focus.write().insert(id, target);
        // Same-table restore only refocuses the filter (the grid's effect
        // consumes it); otherwise switch, honoring the unsaved-edits guard.
        let restored = current.as_ref() == Some(&dest) || self.switch_table(id, &dest, false);
        if restored {
            if let Some(stack) = self.nav_history.write().get_mut(&id) {
                stack.pop();
            }
        }
    }

    /// Loads one cell's full value on demand — the grid's expand and
    /// truncated-cell editing path (FRE-33). Clones the pool and this table's
    /// metadata out of the signals before the await (no borrow spans it), then
    /// targets the row through its [`RowLocator`]. Returns a user-facing error
    /// string on any failure. Never `spawn`ed itself — callers drive it from a
    /// `use_resource` so the fetch is tied to the open editor/overlay.
    pub async fn load_cell(
        self,
        id: ConnectionId,
        table: TableRef,
        locator: RowLocator,
        column: String,
    ) -> Result<CellFetch, String> {
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.table_meta(id, &table);
        let (Some(pool), Some(meta)) = (pool, meta) else {
            return Err("connection or schema no longer available".into());
        };
        // A read path: it only needs the row to be addressable, so it asks
        // for the identity alone rather than a full access it has no business
        // reading a write capability off — a read-only connection, whether by
        // engine (FRE-87) or by marking (FRE-111), still expands cells.
        let Some(identity) = pool.backend_row_identity(&meta) else {
            return Err("this table has no usable row identity".into());
        };
        pool.fetch_cell(&meta, &identity, &locator, &column)
            .await
            .map_err(|e| e.to_string())
    }

    /// DDL for one of a table's objects (FRE-108): the table/view itself, or
    /// one of its indexes. Same shape as [`Self::load_cell`] — the pool and
    /// metadata are cloned out of the signals before the await. No capability
    /// gate: reading a definition is a pure read, so a read-only connection
    /// shows DDL like any other.
    pub async fn load_ddl(
        self,
        id: ConnectionId,
        table: TableRef,
        object: DdlObject,
    ) -> Result<Ddl, String> {
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.table_meta(id, &table);
        let (Some(pool), Some(meta)) = (pool, meta) else {
            return Err("connection or schema no longer available".into());
        };
        pool.fetch_ddl(&meta, &object)
            .await
            .map_err(|e| e.to_string())
    }

    /// One table's cheap storage statistics (FRE-118), loaded when the schema
    /// pane opens it. Same shape as [`Self::load_ddl`]: pool and metadata are
    /// cloned out of the signals before the await, and no capability gate —
    /// reading the catalog's own accounting is a pure read.
    ///
    /// Cheap is the contract, not an aspiration: this must never grow into
    /// anything that scans, because it runs on nothing more deliberate than
    /// looking at a table. The expensive number is [`Self::count_table_rows`].
    pub async fn load_table_stats(
        self,
        id: ConnectionId,
        table: TableRef,
    ) -> Result<TableStats, String> {
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.table_meta(id, &table);
        let (Some(pool), Some(meta)) = (pool, meta) else {
            return Err("connection or schema no longer available".into());
        };
        pool.fetch_table_stats(&meta)
            .await
            .map_err(|e| e.to_string())
    }

    /// Exactly how many rows a table holds, by running `COUNT(*)` (FRE-118).
    ///
    /// Only ever from an explicit "count exactly" action — see
    /// [`DbPool::count_table_rows`], which is where the reason lives.
    pub async fn count_table_rows(self, id: ConnectionId, table: TableRef) -> Result<u64, String> {
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.table_meta(id, &table);
        let (Some(pool), Some(meta)) = (pool, meta) else {
            return Err("connection or schema no longer available".into());
        };
        pool.count_table_rows(&meta)
            .await
            .map_err(|e| e.to_string())
    }

    /// The primary-key column names of `table` in key order, from the loaded
    /// schema (empty when the schema isn't ready or the table has no PK).
    fn table_primary_key(&self, id: ConnectionId, table: &TableRef) -> Vec<String> {
        let schemas = self.schemas.read();
        find_table_meta(schemas.get(&id), table)
            .map(|t| t.primary_key().iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    /// One table's metadata from the loaded schema, cloned out of the signal.
    ///
    /// `None` while the schema is still loading, when the load failed, or
    /// when the table is gone — a refresh can drop one out from under an open
    /// tab, and every caller has to survive that.
    ///
    /// Cloning is deliberate: the async paths (cell fetch, DDL, save) must
    /// not hold a signal borrow across their await, so they take the metadata
    /// with them. Render paths that only read it want [`find_table_meta`],
    /// which borrows.
    pub fn table_meta(&self, id: ConnectionId, table: &TableRef) -> Option<TableMeta> {
        find_table_meta(self.schemas.read().get(&id), table).cloned()
    }

    /// Closes a tab: drops it from the registry, closes the pool in the
    /// background, shuts down its SSH tunnel (if any), and leaves the view
    /// somewhere sensible.
    ///
    /// Guarded: closing while ANY table of the connection has staged edits
    /// takes two attempts (see [`Self::nav_guard`]) — closing is the one
    /// navigation that actually destroys them — and no-ops entirely while a
    /// save is in flight. The notice renders in the dirty table's Save bar,
    /// so when another pane or tab is in front the first click can look
    /// inert; the second click still closes. A deliberate simplification,
    /// kept until a global toast/dialog exists. (Closing the OS window is
    /// guarded separately by the discard-and-quit confirmation, FRE-37.)
    pub fn close_connection(mut self, id: ConnectionId) {
        if self.any_stage_dirty(id) {
            if self.any_stage_saving(id) {
                return;
            }
            if !self.nav_guard_allows(id, NavAction::CloseConnection) {
                return;
            }
        }
        self.nav_guard.set(None);
        self.staged.write().remove(&id);
        self.grid_refresh.write().retain(|(conn, _), _| *conn != id);
        let removed = self.registry.write().remove(id);
        if let Some(connection) = removed {
            // spawn_forever so the close isn't cancelled if the calling
            // component unmounts first.
            spawn_forever(async move { connection.pool.close().await });
        }
        // Dropping the Tunnel signals its forward task to shut down.
        self.tunnels.write().remove(&id);
        self.open_locators
            .write()
            .retain(|(open_id, _)| *open_id != id);
        self.schemas.write().remove(&id);
        self.tab_ui.write().remove(&id);
        self.sql_runs.write().retain(|(conn, _), _| *conn != id);
        self.pending_sql.write().retain(|(conn, _), _| *conn != id);
        self.pending_saves
            .write()
            .retain(|(conn, _), _| *conn != id);
        self.connection_colors.write().remove(&id);
        self.saved_status.write().remove(&id);
        self.export_status
            .write()
            .retain(|(conn, _), _| *conn != id);
        self.export_generations
            .write()
            .retain(|(conn, _), _| *conn != id);
        // Cancel an in-flight import before the pool is closed under it.
        // Dropping the future drops its open transaction, so the rows it had
        // written roll back — leaving it running would commit a partial file
        // into a connection the user just closed, and its outcome would be
        // dropped as stale, showing nothing.
        if let Some(task) = self.import_tasks.write().remove(&id) {
            task.cancel();
        }
        self.import_status.write().remove(&id);
        self.import_generations.write().remove(&id);
        self.pending_focus.write().remove(&id);
        self.nav_history.write().remove(&id);
        // Abort every query tab's in-flight run and drop the bookkeeping;
        // bumping nothing is fine — removing the generation entries makes any
        // still-alive task's generation stale.
        let tasks: Vec<Task> = {
            let mut sql_tasks = self.sql_tasks.write();
            let ids: Vec<(ConnectionId, u64)> = sql_tasks
                .keys()
                .filter(|(conn, _)| *conn == id)
                .copied()
                .collect();
            ids.iter().filter_map(|key| sql_tasks.remove(key)).collect()
        };
        for task in tasks {
            task.cancel();
        }
        self.sql_generations
            .write()
            .retain(|(conn, _), _| *conn != id);
        if *self.active.read() == ActiveView::Connection(id) {
            self.active.set(ActiveView::Connections);
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads the OS dark-mode preference through the webview's `matchMedia`.
/// Any failure (eval error, closed channel, non-bool result) is treated as
/// "light" — a safe, legible default.
async fn system_prefers_dark() -> bool {
    let mut eval =
        document::eval("dioxus.send(window.matchMedia('(prefers-color-scheme: dark)').matches);");
    eval.recv::<bool>().await.unwrap_or(false)
}

/// Canonicalizes for dedupe purposes; falls back to the given path when the
/// file is missing (the connect attempt will surface that error).
pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The open-locator form of a saved connection: [`connect_key`] applied to a
/// saved entry. Kept separate rather than delegating, so the SQLite path is
/// canonicalized straight from the `PathBuf` instead of round-tripping
/// through `locator()`'s lossy `display()` first.
pub(crate) fn saved_open_locator(saved: &SavedConnection) -> String {
    match saved {
        SavedConnection::Sqlite { path, .. } => canonical(path).display().to_string(),
        SavedConnection::Postgres { url, .. } | SavedConnection::SqlServer { url, .. } => {
            url.clone()
        }
    }
}

/// Tab label for a database file: the file name, or the whole path when
/// there is no file name component.
pub fn tab_title(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_app_state_signal_is_root_scoped() {
        // FRE-156. A component-scoped field touched from a `spawn_forever`
        // task warns at runtime and can fail once the creating scope drops —
        // and *nothing else in this suite can see it*, because the warning
        // only exists while the app is running. Five were being logged on
        // every connection open, from three fields; three more fields were
        // reachable the same way and simply hadn't been hit yet.
        //
        // Checked against the source rather than the values because there is
        // nothing to inspect at runtime: `Signal` does not expose its owning
        // scope, and building an `AppState` needs a Dioxus runtime. The
        // constructor is one contiguous struct literal, so slicing it suffices.
        let source = include_str!("mod.rs");
        let start = source
            .find("let state = Self {")
            .expect("AppState's constructor must still be one struct literal");
        let end = start
            + source[start..]
                .find("\n        };")
                .expect("...and must still end where it always has");
        let ctor = &source[start..end];
        let strays: Vec<&str> = ctor
            .lines()
            .filter(|line| line.contains("Signal::new("))
            .collect();
        assert!(
            strays.is_empty(),
            "these fields are owned by the component that built the state, so \
             a root task touching them — directly, or through any method it \
             calls — trips __copy_value_hoisted and can fail after that scope \
             drops. Use Signal::new_in_scope(.., ScopeId::ROOT):\n{}",
            strays.join("\n")
        );
        // The slice must actually contain the fields, or the assertion above
        // passes by matching nothing at all.
        assert!(
            ctor.matches("Signal::new_in_scope(").count() > 20,
            "the constructor slice came out too small to be the real one: {ctor}"
        );
    }

    #[test]
    fn tab_title_uses_the_file_name() {
        assert_eq!(tab_title(Path::new("/data/apps/music.db")), "music.db");
        assert_eq!(tab_title(Path::new("plain.sqlite")), "plain.sqlite");
    }

    #[test]
    fn tab_title_falls_back_to_the_full_path() {
        assert_eq!(tab_title(Path::new("/")), "/");
    }

    #[test]
    fn canonical_falls_back_for_missing_files() {
        let missing = Path::new("/definitely/not/here.db");
        assert_eq!(canonical(missing), missing.to_path_buf());
    }

    #[test]
    fn connect_key_canonicalizes_only_sqlite_paths() {
        // A server URL is already the open-locator form: canonicalizing it as
        // a path would mangle it, and the connections list would then never
        // match a row against its own in-flight connect.
        let url = "postgres://user@host:5432/db";
        assert_eq!(connect_key(url, BackendKind::Postgres), url);
        assert_eq!(connect_key(url, BackendKind::SqlServer), url);

        // SQLite keys on the canonical path, matching what `connect` reserves
        // and what `open_locators` records. Uses a real file, since
        // `canonical` falls back verbatim for missing ones.
        let dir = std::env::temp_dir().join("hubro-connect-key-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("demo.db");
        std::fs::write(&db, b"").unwrap();
        let indirect = dir.join(".").join("demo.db");
        assert_eq!(
            connect_key(&indirect.display().to_string(), BackendKind::Sqlite),
            canonical(&db).display().to_string(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Buffers holding text nobody has saved, in tab order.
    fn buffers(texts: &[&str]) -> SqlBuffers {
        let mut buffers = SqlBuffers::default();
        typed(&mut buffers, FIRST_SQL_BUFFER, texts[0]);
        for text in &texts[1..] {
            buffers.open(None, (*text).to_string());
        }
        buffers
    }

    /// The generation the editor is holding for `buffer` — what a message from
    /// it is stamped with while nothing has replaced that document.
    fn generation_of(buffers: &SqlBuffers, buffer: u64) -> u64 {
        buffers
            .list()
            .iter()
            .find(|b| b.id == buffer)
            .map_or(0, |b| b.doc_generation)
    }

    /// Text arriving from an editor that is up to date — the ordinary case.
    /// Tests about *staleness* pass the generation by hand instead.
    fn typed(buffers: &mut SqlBuffers, buffer: u64, text: &str) -> bool {
        let generation = generation_of(buffers, buffer);
        buffers.set_text(buffer, generation, text.to_string())
    }

    #[test]
    fn opening_a_query_never_overwrites_unsaved_editor_text() {
        // The property the whole "open in a new tab" rule exists for
        // (FRE-113): whatever was being written is still there afterwards,
        // and the opened query is what the pane switches to.
        let mut buffers = buffers(&["SELECT half_typed"]);
        let opened = buffers.open(Some("Daily counts".into()), "SELECT count(*)".into());
        assert_eq!(buffers.list().len(), 2);
        assert_eq!(buffers.active(), opened);
        assert_eq!(buffers.text(FIRST_SQL_BUFFER), "SELECT half_typed");
        assert_eq!(buffers.list()[1].title.as_deref(), Some("Daily counts"));
        assert_eq!(buffers.text(opened), "SELECT count(*)");
    }

    #[test]
    fn opening_reuses_an_untitled_blank_buffer() {
        // Nothing to protect, so opening into it beats leaving an empty tab
        // stranded beside the query the user asked for. Whitespace-only
        // counts as blank; a titled buffer never does, even when emptied.
        let mut buffers = buffers(&["  \n "]);
        assert_eq!(
            buffers.open(Some("Counts".into()), "SELECT 1".into()),
            FIRST_SQL_BUFFER
        );
        assert_eq!(buffers.list().len(), 1);
        assert_eq!(buffers.list()[0].title.as_deref(), Some("Counts"));

        // Now that it is named, opening again must not clobber it.
        let second = buffers.open(Some("Other".into()), "SELECT 2".into());
        assert_ne!(second, FIRST_SQL_BUFFER);
        assert_eq!(buffers.list().len(), 2);

        // An inactive blank is left alone too — reusing it would move the
        // user to a tab they weren't in.
        let mut two = buffers_with_blank_first();
        let opened = two.open(None, "SELECT new".into());
        assert_eq!(two.list().len(), 3);
        assert_eq!(two.active(), opened);
    }

    /// A blank, untitled *inactive* first buffer, with the second active.
    fn buffers_with_blank_first() -> SqlBuffers {
        let mut buffers = SqlBuffers::default();
        typed(&mut buffers, FIRST_SQL_BUFFER, "typing");
        let second = buffers.open(None, "SELECT keep_me".into());
        typed(&mut buffers, FIRST_SQL_BUFFER, "");
        assert_eq!(buffers.list().len(), 2);
        assert_eq!(buffers.active(), second);
        buffers
    }

    #[test]
    fn opening_moves_the_document_target_even_when_it_reuses_the_buffer() {
        // What the SQL pane's `setDoc` effect is gated on. The reuse branch
        // returns the *same* buffer id, so if the id were the only dependency
        // the editor would keep showing the old (empty) document while the
        // tab renamed itself — the state layer and the screen disagreeing.
        // This is the feature's first-use path: a fresh SQL pane is exactly
        // one untitled blank buffer.
        let mut buffers = SqlBuffers::default();
        let before = buffers.doc_target();
        let opened = buffers.open(Some("Artists".into()), "SELECT * FROM artists".into());
        assert_eq!(opened, before.0, "this test must exercise the reuse branch");
        assert_ne!(
            buffers.doc_target(),
            before,
            "a reused buffer still needs the editor's document replaced"
        );

        // The non-reuse branch moves it too (its id changes as well).
        let before = buffers.doc_target();
        buffers.open(Some("Albums".into()), "SELECT * FROM albums".into());
        assert_ne!(buffers.doc_target(), before);
    }

    #[test]
    fn typing_does_not_move_the_document_target() {
        // The opposite guard: text arriving *from* the editor must not push a
        // document back into it, or every keystroke would fight the caret.
        let mut buffers = SqlBuffers::default();
        let before = buffers.doc_target();
        assert!(typed(&mut buffers, FIRST_SQL_BUFFER, "SELECT 1"));
        assert_eq!(buffers.doc_target(), before);
        // Nor does naming a buffer after a save, or selecting the buffer that
        // is already active.
        buffers.set_title(FIRST_SQL_BUFFER, "Counts".into());
        buffers.select(FIRST_SQL_BUFFER);
        assert_eq!(buffers.doc_target(), before);
    }

    #[test]
    fn the_tail_of_typing_lands_in_the_buffer_it_was_typed_in() {
        // FRE-154. The editor flushes what it still owes on the way *out* of a
        // buffer, so that message arrives after the switch by design. It has
        // to be filed under the buffer named in it: filing it under whichever
        // buffer is active on arrival would paste the outgoing tab's tail into
        // the incoming one, which is worse than the loss it replaced.
        let mut buffers = buffers(&["SELECT a", "SELECT b"]);
        let ids: Vec<u64> = buffers.list().iter().map(|b| b.id).collect();
        buffers.select(ids[1]);
        assert!(
            typed(&mut buffers, ids[0], "SELECT a_tail"),
            "a switch replaces no document, so the outgoing buffer's \
             generation still matches and its tail is not stale"
        );
        assert_eq!(buffers.text(ids[0]), "SELECT a_tail");
        assert_eq!(
            buffers.text(ids[1]),
            "SELECT b",
            "the tail went nowhere near the buffer being switched to"
        );
    }

    #[test]
    fn a_stale_reply_from_the_editor_cannot_undo_a_load() {
        // The other half of the same change. Flushing means there is now a
        // message in flight describing the document a load is replacing —
        // apply it and the history panel's Load silently reverts a moment
        // after it lands.
        let mut buffers = SqlBuffers::default();
        let stale = generation_of(&buffers, FIRST_SQL_BUFFER);
        buffers.load(FIRST_SQL_BUFFER, "SELECT loaded".into());
        assert!(
            !buffers.set_text(FIRST_SQL_BUFFER, stale, "SELECT half_typed".into()),
            "a reply about the replaced document must be dropped"
        );
        assert_eq!(buffers.text(FIRST_SQL_BUFFER), "SELECT loaded");

        // And once the editor has been told about the load, its next reply is
        // current again — the guard must not wedge the buffer read-only. The
        // generation is named rather than looked up: `typed` reads whatever
        // the buffer is at, so a guard that advanced on every accepted write
        // would keep passing through it while the real editor — which only
        // learns a new generation from a `setDoc` — fell permanently behind.
        assert_eq!(generation_of(&buffers, FIRST_SQL_BUFFER), stale + 1);
        assert!(buffers.set_text(FIRST_SQL_BUFFER, stale + 1, "SELECT loaded, more".into()));
        assert_eq!(buffers.text(FIRST_SQL_BUFFER), "SELECT loaded, more");
        assert!(
            buffers.set_text(
                FIRST_SQL_BUFFER,
                stale + 1,
                "SELECT loaded, more, more".into()
            ),
            "the generation must not move on text arriving *from* the editor, \
             or every keystroke after the first is refused"
        );
    }

    #[test]
    fn opening_elsewhere_does_not_invalidate_another_buffers_tail() {
        // Why the generation is per buffer rather than per tab: opening a
        // saved query replaces the document of the buffer it opens into, and
        // of no other. A tab-wide counter would date-stamp the buffer being
        // typed in as stale as well, throwing away the very text this fix
        // exists to keep.
        let mut buffers = buffers(&["SELECT a"]);
        let first = buffers.list()[0].id;
        let typing_generation = generation_of(&buffers, first);
        let opened = buffers.open(Some("Counts".into()), "SELECT count(*)".into());
        assert_ne!(opened, first, "this test needs the new-buffer branch");
        assert!(buffers.set_text(first, typing_generation, "SELECT a_tail".into()));
        assert_eq!(buffers.text(first), "SELECT a_tail");
    }

    #[test]
    fn loading_moves_the_document_target_so_the_pane_pushes_the_text() {
        // Load has no `setDoc` of its own any more — moving the target *is*
        // how the text reaches CodeMirror. If this stopped holding, the
        // history panel would write the state and leave the screen showing the
        // query that was there before.
        let mut buffers = SqlBuffers::default();
        let before = buffers.doc_target();
        assert!(buffers.load(FIRST_SQL_BUFFER, "SELECT loaded".into()));
        assert_ne!(buffers.doc_target(), before);

        // A buffer that is gone is reported, not silently created: the caller
        // clears a parked write confirmation on the strength of this.
        assert!(!buffers.load(FIRST_SQL_BUFFER + 999, "SELECT nowhere".into()));
    }

    #[test]
    fn buffer_ids_are_never_reused() {
        // A stale run or a parked write confirmation names its buffer by id;
        // handing a closed buffer's id to a new one would re-attach them.
        let mut buffers = buffers(&["a", "b"]);
        let ids: Vec<u64> = buffers.list().iter().map(|b| b.id).collect();
        buffers.close(ids[1]);
        let reopened = buffers.open(None, "c".into());
        assert!(
            !ids.contains(&reopened) || reopened == ids[0],
            "id {reopened} was already used"
        );
        assert_ne!(reopened, ids[1], "a closed buffer's id must not come back");

        // Closing every buffer leaves one scratch buffer, also with a new id.
        let mut seen = vec![ids[0], ids[1], reopened];
        for _ in 0..3 {
            let active = buffers.active();
            let after = buffers.close(active);
            assert_eq!(buffers.list().len().max(1), buffers.list().len());
            if !seen.contains(&after) {
                seen.push(after);
            }
        }
        let unique: HashSet<u64> = seen.iter().copied().collect();
        assert_eq!(unique.len(), seen.len(), "an id was handed out twice");
        assert_eq!(buffers.list().len(), 1, "a tab always keeps one buffer");
    }

    #[test]
    fn closing_moves_the_selection_only_when_it_has_to() {
        let mut buffers = buffers(&["a", "b", "c"]);
        let ids: Vec<u64> = buffers.list().iter().map(|b| b.id).collect();
        buffers.select(ids[2]);
        // Closing an inactive buffer leaves the active one alone.
        assert_eq!(buffers.close(ids[0]), ids[2]);
        // Closing the active last one falls back to the new last.
        assert_eq!(buffers.close(ids[2]), ids[1]);
        // Closing a buffer that is already gone changes nothing.
        assert_eq!(buffers.close(ids[0]), ids[1]);
        assert_eq!(buffers.list().len(), 1);

        // Closing an active middle buffer falls to the one that took its
        // place, not to the start of the list.
        let mut three = buffers_abc();
        let ids: Vec<u64> = three.list().iter().map(|b| b.id).collect();
        three.select(ids[1]);
        assert_eq!(three.close(ids[1]), ids[2]);
    }

    fn buffers_abc() -> SqlBuffers {
        buffers(&["a", "b", "c"])
    }

    #[test]
    fn a_closed_buffers_last_keystroke_cannot_resurrect_it() {
        // The webview can deliver one more doc message after a tab is closed;
        // it must land nowhere rather than re-create the buffer.
        let mut buffers = buffers(&["a", "b"]);
        let ids: Vec<u64> = buffers.list().iter().map(|b| b.id).collect();
        buffers.close(ids[1]);
        assert!(!typed(&mut buffers, ids[1], "late"));
        assert_eq!(buffers.list().len(), 1);
        assert_eq!(buffers.text(ids[1]), "");
        // A select for a buffer that is gone is ignored too, so `active`
        // always names a buffer that exists.
        buffers.select(ids[1]);
        assert_eq!(buffers.active(), ids[0]);
    }

    #[test]
    fn saved_status_names_what_it_did() {
        let (created, _) = SavedStatus::Saved {
            name: "Counts".into(),
            replaced: false,
        }
        .line();
        assert_eq!(created, "Saved “Counts”");
        let (replaced, _) = SavedStatus::Saved {
            name: "Counts".into(),
            replaced: true,
        }
        .line();
        assert_eq!(replaced, "Updated “Counts”");
        let (failed, class) = SavedStatus::Failed("disk full".into()).line();
        assert_eq!(failed, "Save failed: disk full");
        assert!(class.contains("red"));
    }

    #[test]
    fn connect_step_labels_are_distinct_and_lowercase() {
        let steps = [
            ConnectStep::Tunnel,
            ConnectStep::Credentials,
            ConnectStep::SigningIn,
            ConnectStep::Opening,
        ];
        let labels: HashSet<&str> = steps.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), steps.len(), "two steps read the same");
        for step in steps {
            let label = step.label();
            // These render as a status line under the connection name, not as
            // a sentence, so they stay lowercase and unfinished.
            assert!(label.ends_with('…'), "{label:?} lacks a trailing ellipsis");
            assert!(
                !label.starts_with(|c: char| c.is_uppercase()),
                "{label:?} is capitalized"
            );
        }
    }
}
