//! What a connection — and each object on it — can actually do (FRE-87).
//!
//! Not every engine is editable, transactional and offset-pageable, and not
//! every object on an editable engine is writable. [`Capabilities`] states a
//! connection's defaults; [`TableAccess::resolve`] narrows them for one
//! object and reports *why* it narrowed, so the UI can disable an affordance
//! with an explanation instead of leaving a silently dead button.
//!
//! The layering only ever narrows. A resolved capability is never wider than
//! the connection default, so a backend declaring `mutate: false` can't have
//! an object argue its way back into being writable.
//!
//! Per-connection write protection ([`WriteProtection`], FRE-111) is user
//! intent layered onto this same resolution: it narrows the [`Capabilities`]
//! passed to [`TableAccess::resolve`] and supplies its own [`Restriction`],
//! rather than becoming a second, parallel way to disable editing.

use serde::{Deserialize, Serialize};

use super::rowkey::{detect_row_identity, RowIdentity};
use super::schema::{TableKind, TableMeta};
use super::sql::Dialect;

/// What a connection supports. Backends declare these at connect time; one
/// object's effective set is this, narrowed by [`TableAccess::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Can run `SELECT` at all. Deliberately separate from [`Self::mutate`]:
    /// a read-only analytics engine still needs a working SQL pane, so "can
    /// query" and "can write" must not collapse into one flag.
    pub read_query: bool,
    /// `INSERT` / `UPDATE` / `DELETE`.
    pub mutate: bool,
    /// `CREATE` / `ALTER` / `DROP` / `TRUNCATE`.
    pub ddl: bool,
    /// Multi-statement atomicity: a real
    /// [`ScriptTx`](super::registry::ScriptTx) rather than autocommit-only.
    pub transactions: bool,
    /// Paging with `LIMIT`/`OFFSET` (see
    /// [`PageRequest`](super::page::PageRequest)) rather than a cursor.
    pub offset_paging: bool,
}

impl Capabilities {
    /// Everything supported — what a full-featured OLTP engine declares.
    pub const FULL: Capabilities = Capabilities {
        read_query: true,
        mutate: true,
        ddl: true,
        transactions: true,
        offset_paging: true,
    };

    /// Queryable but not writable: the shape a read-only backend declares,
    /// and what per-connection write protection (FRE-111) will narrow to.
    pub const fn read_only(self) -> Capabilities {
        Capabilities {
            mutate: false,
            ddl: false,
            ..self
        }
    }
}

/// Why an object's resolved capabilities are narrower than its connection's
/// defaults. Carries the user-facing explanation so every gated affordance in
/// the UI states one reason from one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restriction {
    /// A view: its rows have no identity to write through.
    View,
    /// A materialized view: browsable like a table, but `UPDATE`/`DELETE`
    /// can't reach its stored rows.
    MaterializedView,
    /// A table with neither a primary key nor a usable unique index, so no
    /// `WHERE` clause addresses exactly one row (see
    /// [`detect_row_identity`]).
    NoRowIdentity,
    /// The backend or the connection itself refuses writes, with its own
    /// explanation — a read-only engine, an object kind the driver knows is
    /// not writable (a DuckDB view over a Parquet file, a StarRocks
    /// duplicate-key table), or the user's own read-only marking (FRE-111).
    Declared(&'static str),
}

impl Restriction {
    /// The sentence the UI shows in place of the disabled affordance.
    pub fn message(self) -> &'static str {
        match self {
            Restriction::View => "Views are read-only.",
            Restriction::MaterializedView => "Materialized views are read-only.",
            Restriction::NoRowIdentity => {
                "This table has no primary key or usable unique index — editing will be disabled."
            }
            Restriction::Declared(message) => message,
        }
    }
}

/// Why a gated path refused, one sentence per missing capability. Kept
/// together so a disabled button, a refused script and a rejected save all
/// say the same thing.
pub const NO_QUERY: &str = "This connection can't run queries.";
pub const NO_MUTATE: &str = "This connection is read-only.";
pub const NO_DDL: &str = "This connection doesn't allow schema changes.";
pub const NO_OFFSET_PAGING: &str = "This connection can't page through rows with LIMIT/OFFSET.";

/// The connection-level explanation when a backend declares `mutate: false`.
pub const CONNECTION_READ_ONLY: Restriction = Restriction::Declared(NO_MUTATE);

/// What the user chose, as opposed to what the backend imposes — worded so the
/// two are never confused. A backend that can't write says "is read-only"; a
/// connection the user marked says "you marked".
pub const MARKED_READ_ONLY: &str = "You marked this connection read-only.";

/// The connection-level explanation when [`WriteProtection::ReadOnly`] — not
/// the backend — is what forbids the write.
pub const USER_READ_ONLY: Restriction = Restriction::Declared(MARKED_READ_ONLY);

/// How much the user wants this connection to resist writes (FRE-111).
///
/// Three states rather than a boolean, because a boolean forces a production
/// connection to be either unprotected or unusable for writes — and people
/// then leave protection off. [`Confirm`](Self::Confirm) is the state that
/// actually gets used: you do occasionally need to write to production, just
/// never by accident.
///
/// This is user intent, not a backend fact, so it *narrows* the connection's
/// declared [`Capabilities`] rather than sitting beside them as a second
/// check — see [`Self::apply`] and [`TableAccess::resolve_protected`].
///
/// **The variant order is load-bearing.** `Ord` is derived, so it runs
/// least-to-most protective, and `a.max(b)` is "the stricter of the two" —
/// which is how a merge of two markings resolves. Reordering the variants
/// would silently invert that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteProtection {
    /// No extra friction: the backend's own capabilities decide. The default
    /// for new connections, and what a config file with no `protection` key
    /// deserializes as.
    #[default]
    Open,
    /// Writes still run, but each one is confirmed first, and the prompt names
    /// the connection — the point is to make you read which database you are
    /// about to change.
    Confirm,
    /// Writes are refused at the `db/` layer.
    ///
    /// Enforcement is by statement classification (see
    /// [`script::statement_needs`](super::script::statement_needs)), not by
    /// the engine, so a write reached through a function call inside a
    /// `SELECT` is not caught. Refusing those needs a server-side read-only
    /// transaction per backend; until then this is a guard against mistakes,
    /// not against a determined user.
    ReadOnly,
}

impl WriteProtection {
    /// Whether this is the default state — lets the config skip writing a
    /// `protection` key for ordinary connections (back-compat).
    pub fn is_open(&self) -> bool {
        matches!(self, WriteProtection::Open)
    }

    /// Whether a write through this connection must be confirmed first.
    ///
    /// False for [`ReadOnly`](Self::ReadOnly): there is nothing to confirm
    /// when the write is refused outright.
    pub fn confirms(self) -> bool {
        matches!(self, WriteProtection::Confirm)
    }

    /// `defaults` narrowed by this protection — the connection's *effective*
    /// capabilities.
    ///
    /// [`Confirm`](Self::Confirm) narrows nothing: it interposes a prompt
    /// rather than removing the capability, so every affordance stays enabled
    /// and the confirmation is what the user meets.
    pub fn apply(self, defaults: Capabilities) -> Capabilities {
        match self {
            WriteProtection::Open | WriteProtection::Confirm => defaults,
            WriteProtection::ReadOnly => defaults.read_only(),
        }
    }

    /// The short label the UI shows on a connection carrying this protection,
    /// or `None` when there is nothing to say.
    pub fn badge(self) -> Option<&'static str> {
        match self {
            WriteProtection::Open => None,
            WriteProtection::Confirm => Some("confirm writes"),
            WriteProtection::ReadOnly => Some("read-only"),
        }
    }
}

/// One object's resolved capabilities: the connection defaults narrowed by
/// what this particular object supports, plus how its rows are addressed.
///
/// [`Self::restriction`] is `Some` exactly when [`Self::caps`]`.mutate` is
/// false, naming the most specific reason found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableAccess {
    pub caps: Capabilities,
    /// How a single row is addressed for reads that need to pin one (cell
    /// fetch) and for writes. `None` when no such addressing exists.
    pub identity: Option<RowIdentity>,
    pub restriction: Option<Restriction>,
}

impl TableAccess {
    /// Resolves `defaults` for one object: connection capabilities first,
    /// then the object's own [`TableMeta::restriction`], then its
    /// [`TableKind`], then whether its rows can be addressed at all. The
    /// first of those that forbids writes supplies the reason.
    pub fn resolve(defaults: Capabilities, table: &TableMeta, dialect: Dialect) -> TableAccess {
        // Identity is resolved even for objects that can't be written: the
        // grid's cell-fetch path pins a row by the same key.
        let identity = detect_row_identity(table, dialect);
        let restriction = if !defaults.mutate {
            Some(CONNECTION_READ_ONLY)
        } else if let Some(declared) = table.restriction {
            Some(declared)
        } else {
            match table.kind {
                TableKind::View => Some(Restriction::View),
                TableKind::MaterializedView => Some(Restriction::MaterializedView),
                TableKind::Table if identity.is_none() => Some(Restriction::NoRowIdentity),
                TableKind::Table => None,
            }
        };
        TableAccess {
            caps: Capabilities {
                mutate: restriction.is_none() && defaults.mutate,
                ..defaults
            },
            identity,
            restriction,
        }
    }

    /// [`Self::resolve`] with the user's own write protection folded in
    /// (FRE-111) — the entry point every gated path should use, so protection
    /// and backend capability produce one effective answer instead of two
    /// checks that can disagree.
    ///
    /// The marking takes the blame **only when it is what changed the
    /// answer** ([`USER_READ_ONLY`]). If the object was already unwritable —
    /// a view, a key-less table — or the backend already refused, that reason
    /// stands. Telling someone "you marked this read-only" about a view would
    /// send them to unmark it and find the write still refused.
    ///
    /// Note the asymmetry with the backend's own refusal, which [`Self::resolve`]
    /// checks *before* the object's reason and so still wins over it. That
    /// predates this and is left alone; the rule here is only about which
    /// reason is actionable.
    pub fn resolve_protected(
        defaults: Capabilities,
        protection: WriteProtection,
        table: &TableMeta,
        dialect: Dialect,
    ) -> TableAccess {
        // Resolved against the backend's own capabilities, so the object's
        // verdict is known independently of the marking.
        let mut access = TableAccess::resolve(defaults, table, dialect);
        let effective = protection.apply(defaults);
        if access.restriction.is_none() && !effective.mutate {
            access.restriction = Some(USER_READ_ONLY);
        }
        access.caps = Capabilities {
            mutate: access.restriction.is_none() && effective.mutate,
            ..effective
        };
        access
    }

    /// Whether rows of this object can be edited, inserted or deleted.
    pub fn can_mutate(&self) -> bool {
        self.caps.mutate
    }

    /// The reason editing is unavailable, or `None` when it is available.
    pub fn read_only_notice(&self) -> Option<&'static str> {
        self.restriction.map(Restriction::message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{ColumnMeta, Generated, IndexMeta, TypeDetail};

    fn col(name: &str, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "int".into(),
            nullable: false,
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
            type_detail: TypeDetail::Plain,
        }
    }

    fn table(kind: TableKind, columns: Vec<ColumnMeta>) -> TableMeta {
        TableMeta {
            schema: None,
            name: "t".into(),
            kind,
            columns,
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    fn keyed_table() -> TableMeta {
        table(TableKind::Table, vec![col("id", Some(1))])
    }

    #[test]
    fn keyed_table_on_a_full_backend_resolves_to_full_capability() {
        let access = TableAccess::resolve(Capabilities::FULL, &keyed_table(), Dialect::Postgres);
        assert_eq!(access.caps, Capabilities::FULL);
        assert_eq!(access.restriction, None);
        assert!(access.can_mutate());
        assert_eq!(access.read_only_notice(), None);
        assert!(access.identity.is_some());
    }

    #[test]
    fn views_and_matviews_are_not_mutable_but_stay_readable() {
        for (kind, expected) in [
            (TableKind::View, Restriction::View),
            (TableKind::MaterializedView, Restriction::MaterializedView),
        ] {
            let t = table(kind, vec![col("id", Some(1))]);
            let access = TableAccess::resolve(Capabilities::FULL, &t, Dialect::Postgres);
            assert!(!access.can_mutate(), "{kind:?} must not be mutable");
            assert_eq!(access.restriction, Some(expected));
            // Reading is untouched: only the write flag narrows.
            assert!(access.caps.read_query);
            assert!(access.caps.offset_paging);
        }
    }

    #[test]
    fn keyless_table_reports_the_missing_identity_as_the_reason() {
        let t = table(TableKind::Table, vec![col("data", None)]);
        let access = TableAccess::resolve(Capabilities::FULL, &t, Dialect::Postgres);
        assert!(!access.can_mutate());
        assert_eq!(access.restriction, Some(Restriction::NoRowIdentity));
        assert_eq!(access.identity, None);
    }

    #[test]
    fn sqlite_falls_back_to_rowid_so_a_keyless_table_stays_editable() {
        let t = table(TableKind::Table, vec![col("data", None)]);
        let access = TableAccess::resolve(Capabilities::FULL, &t, Dialect::Sqlite);
        assert!(access.can_mutate());
        assert_eq!(access.restriction, None);
    }

    #[test]
    fn a_read_only_connection_blocks_writes_on_an_ordinary_table() {
        let defaults = Capabilities::FULL.read_only();
        let access = TableAccess::resolve(defaults, &keyed_table(), Dialect::Postgres);
        assert!(!access.can_mutate());
        assert!(!access.caps.ddl);
        assert_eq!(access.restriction, Some(CONNECTION_READ_ONLY));
        // read_query: true with mutate: false still permits SELECT.
        assert!(access.caps.read_query);
        // The row is still addressable, so cell fetch keeps working.
        assert!(access.identity.is_some());
    }

    #[test]
    fn the_connection_reason_wins_over_the_objects_own() {
        let t = table(TableKind::View, vec![col("id", Some(1))]);
        let access = TableAccess::resolve(Capabilities::FULL.read_only(), &t, Dialect::Postgres);
        assert_eq!(access.restriction, Some(CONNECTION_READ_ONLY));
    }

    #[test]
    fn a_declared_object_restriction_overrides_the_connection_default() {
        let mut t = keyed_table();
        t.restriction = Some(Restriction::Declared("Duplicate-key tables are read-only."));
        let access = TableAccess::resolve(Capabilities::FULL, &t, Dialect::Postgres);
        assert!(!access.can_mutate());
        assert_eq!(
            access.read_only_notice(),
            Some("Duplicate-key tables are read-only.")
        );
    }

    #[test]
    fn open_protection_leaves_the_backends_answer_untouched() {
        let table = keyed_table();
        assert_eq!(
            TableAccess::resolve_protected(
                Capabilities::FULL,
                WriteProtection::Open,
                &table,
                Dialect::Postgres
            ),
            TableAccess::resolve(Capabilities::FULL, &table, Dialect::Postgres)
        );
    }

    #[test]
    fn confirm_protection_narrows_nothing_and_only_interposes_a_prompt() {
        // Confirm must leave the affordance enabled — the confirmation is
        // what the user meets, not a disabled button.
        let access = TableAccess::resolve_protected(
            Capabilities::FULL,
            WriteProtection::Confirm,
            &keyed_table(),
            Dialect::Postgres,
        );
        assert!(access.can_mutate());
        assert_eq!(access.restriction, None);
        assert!(WriteProtection::Confirm.confirms());
        assert!(!WriteProtection::ReadOnly.confirms());
        assert!(!WriteProtection::Open.confirms());
    }

    #[test]
    fn marking_a_connection_read_only_refuses_writes_and_says_who_refused() {
        let access = TableAccess::resolve_protected(
            Capabilities::FULL,
            WriteProtection::ReadOnly,
            &keyed_table(),
            Dialect::Postgres,
        );
        assert!(!access.can_mutate());
        assert!(!access.caps.ddl);
        assert_eq!(access.restriction, Some(USER_READ_ONLY));
        // Blames the marking, not the engine — the user can change one of them.
        assert_eq!(access.read_only_notice(), Some(MARKED_READ_ONLY));
        // Reading is untouched, and the row stays addressable for cell fetch.
        assert!(access.caps.read_query);
        assert!(access.identity.is_some());
    }

    #[test]
    fn an_objects_own_reason_survives_the_marking() {
        // "You marked this read-only" about a view would send the user to
        // unmark it and find the write still refused. The marking only takes
        // the blame when it is what changed the answer.
        for (kind, expected) in [
            (TableKind::View, Restriction::View),
            (TableKind::MaterializedView, Restriction::MaterializedView),
        ] {
            let t = table(kind, vec![col("id", Some(1))]);
            let access = TableAccess::resolve_protected(
                Capabilities::FULL,
                WriteProtection::ReadOnly,
                &t,
                Dialect::Postgres,
            );
            assert_eq!(access.restriction, Some(expected));
            assert!(!access.can_mutate());
        }
        // Same for a table with no addressable row.
        let keyless = table(TableKind::Table, vec![col("data", None)]);
        let access = TableAccess::resolve_protected(
            Capabilities::FULL,
            WriteProtection::ReadOnly,
            &keyless,
            Dialect::Postgres,
        );
        assert_eq!(access.restriction, Some(Restriction::NoRowIdentity));
    }

    #[test]
    fn a_read_only_backend_is_still_blamed_for_itself_whatever_the_marking() {
        // The backend already refused, so the marking didn't change anything
        // and must not claim the credit.
        for protection in [
            WriteProtection::Open,
            WriteProtection::Confirm,
            WriteProtection::ReadOnly,
        ] {
            let access = TableAccess::resolve_protected(
                Capabilities::FULL.read_only(),
                protection,
                &keyed_table(),
                Dialect::Postgres,
            );
            assert_eq!(
                access.restriction,
                Some(CONNECTION_READ_ONLY),
                "{protection:?} must not reattribute the backend's own refusal"
            );
        }
    }

    #[test]
    fn protection_defaults_to_open_and_serializes_as_kebab_case() {
        assert_eq!(WriteProtection::default(), WriteProtection::Open);
        assert!(WriteProtection::Open.is_open());
        assert!(!WriteProtection::Confirm.is_open());
        // The on-disk spelling is part of the config format (FRE-111).
        let round_trip = |p: WriteProtection| {
            let text = toml::to_string(&Wrapper { protection: p }).unwrap();
            let back: Wrapper = toml::from_str(&text).unwrap();
            (text, back.protection)
        };
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            protection: WriteProtection,
        }
        for (p, spelling) in [
            (WriteProtection::Open, "open"),
            (WriteProtection::Confirm, "confirm"),
            (WriteProtection::ReadOnly, "read-only"),
        ] {
            let (text, back) = round_trip(p);
            assert!(
                text.contains(spelling),
                "{p:?} should write {spelling:?}: {text}"
            );
            assert_eq!(back, p);
        }
    }

    #[test]
    fn resolution_only_ever_narrows() {
        // A unique index over NOT NULL columns stands in for a PK, but a
        // read-only connection still refuses writes.
        let mut t = table(TableKind::Table, vec![col("email", None)]);
        t.indexes.push(IndexMeta {
            name: "t_email_key".into(),
            unique: true,
            partial: false,
            columns: vec!["email".into()],
        });
        assert!(TableAccess::resolve(Capabilities::FULL, &t, Dialect::Postgres).can_mutate());
        assert!(
            !TableAccess::resolve(Capabilities::FULL.read_only(), &t, Dialect::Postgres)
                .can_mutate()
        );
    }
}
