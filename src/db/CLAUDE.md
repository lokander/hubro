# Backends, introspection, and capabilities

Rules for `src/db/`. The project-wide guidance is in the root `CLAUDE.md`.

## Engine flavors

- Postgres-wire engines that aren't PostgreSQL are identified once at connect by `PgFlavor` (`src/db/postgres.rs`, FRE-90), read from the `version()` call that doubles as the liveness check. Keep any flavor branching inside the backend so a new engine is handled in one place; `DbPool::pg_flavor()` exists to report the answer, not for callers to branch on. **Prefer a catalog fact over the flavor whenever one exists** — CockroachDB's reserved catalog schemas are found via `table_type = 'SYSTEM VIEW'`, not via its name, which keeps the FRE-88 rule intact and needs no engine check at all. Likewise, anything varying by *version* within one engine belongs in a catalog query, which reports what the server has rather than what its version implies. Where no catalog fact exists, the flavor picks which catalog to *ask* rather than deciding the answer itself — Materialize's reserved schemas come from `mz_schemas.id` (FRE-92), a query only Materialize can answer.

## Introspection SQL

- Must not lean on PostgreSQL internals it doesn't need: `NULL::text` rather than `NULL::name` (`name` is a PostgreSQL-internal type), and no explicit `LATERAL` on an `unnest` in `FROM` (Postgres implies it; RisingWave's parser rejects it). Both cost nothing on Postgres and were the difference between a working schema tree and none (FRE-93).
- Must survive a server that lacks a column they select — one absent column fails the whole statement and empties the schema tree. `src/db/postgres.rs` handles this by trying the rich query and falling back to a portable shape that selects the missing pieces as the constants they would decode to (FRE-92); the fallback degrades on *any* missing column rather than on a list of known ones. Equally, don't assume a UNION's two halves are disjoint because they are on PostgreSQL: Materialize reports materialized views in both `information_schema` and `relkind = 'm'`, so each half now takes only what the other did not.

## Retrying

- A failure the *server* declared retryable is `DbError::Transient`, classified on SQLSTATE `40001` alone and never on message text (FRE-147) — YugabyteDB's stale catalog snapshot and CockroachDB's transaction conflicts share the code and share nothing in their wording. Only the multi-statement catalog reads act on it: `DbPool::introspect` and `fetch_ddl` run **once** more via `retry_transient`, because a schema change landing between their queries is nobody's mistake and nothing the user can act on. Never retry a write on it, and never loop — a second failure isn't a racing schema change, and a schema tree that hangs is worse than one that reports an error.

## Capabilities

- Capabilities are declared per *server*, not just per driver (FRE-87/FRE-93): `DbPool::backend_capabilities` consults `PgFlavor` because RisingWave speaks the Postgres protocol but has no read-write transactions. Declaring `transactions: false` is what makes the script tab stop wrapping batches and editing refuse with a reason — hubro's row-count write guard *is* a transaction, so a backend without one is not offered unguarded editing.
- **Prefer narrowing a claim to clearing a capability.** CockroachDB and YugabyteDB hold real transactions that roll DML back and let DDL escape, so they declare `transactions: true` with `transactional_ddl: false` (FRE-146) — declaring them non-transactional would be the worse lie, disabling the script tab's atomicity for the case where it does hold. `transactional_ddl` is the one capability that gates nothing: it only decides whether a failed script reports `Rollback::Full` or `Rollback::ExceptSchemaChanges`. When engines disagree on the details of a gap (Cockroach also commits writes staged *before* a DDL; Yugabyte rolls those back), say only what holds on both — claiming less than is known beats claiming more.

## Object metadata

- Objects that are the database's own bookkeeping (extension schemas and tables, child partitions) are declared per backend as `TableMeta::internal` during introspection — never inferred from name patterns (FRE-88). The sidebar hides them behind one toggle and the SQL editor demotes them in completion ranking, so every new backend inherits both by filling in that one field. `TableMeta::kind_label` is the matching hook for engine-specific vocabulary (`hypertable`, `continuous aggregate`), rendered as a badge that refines `TableKind` rather than replacing it.
- An object with **no rows at all** — a RisingWave sink, which writes outward and stores nothing — is declared `Restriction::NoRows` (FRE-148), the one restriction that narrows *reading* as well as writing. `TableAccess::resolve` derives the read gate from the object's own declaration and never from the write chain, which short-circuits on a read-only connection; `resolve_protected` has to carry it over explicitly, since rebuilding `caps` from the marking would hand the read capability back. Every other restriction (view, materialized view, key-less table) leaves browsing alone — they lack a way to address one row for writing, not rows to show.
