# Demo databases for manual testing

Throwaway databases with the same small shop schema on all three backends, for
poking at the app by hand. Nothing here is used by `cargo test` — the
integration tests build their own fixtures and run against their own containers
on different ports (5433 / 14333), so seeding never disturbs a test run.

```sh
scripts/seed/seed-sqlite.sh       # → scripts/seed/demo.db (gitignored)
scripts/seed/seed-postgres.sh     # → postgres://hubro:hubropass@localhost:5434/demo
scripts/seed/seed-sqlserver.sh    # → mssql://sa:Str0ng!Passw0rd@localhost:14334/demo
```

Each script is idempotent: re-running rebuilds the schema from scratch. The two
server scripts start their container via `docker-compose.yml` first, so a plain
`docker` install is all you need — no host `psql` or `sqlcmd`.

```sh
docker compose -f scripts/seed/docker-compose.yml stop     # keep the data
docker compose -f scripts/seed/docker-compose.yml down -v  # forget everything
```

## What's in the schema

The same domain everywhere — `customers`, `products`, `orders`, `order_items`,
`events` — with the details chosen to exercise the viewer rather than to be
realistic:

- **Row identity**: integer PK, text PK, composite PK, a keyless table backed
  only by a unique index (`sensor_readings`), and a keyless table with neither
  (`events`, addressed by ctid/rowid). Plus a partial/filtered unique index on
  `orders` that row identity must *not* mistake for a key.
- **Read-only columns**: identity columns (overridable and not), stored
  generated/computed columns, and SQL Server's `rowversion`.
- **Foreign keys**: single-column, composite (`analytics.sensor_alerts` →
  `sensor_readings`), and one from a keyless table.
- **Schemas and object kinds**: an `analytics` schema alongside the default one,
  a view, and (Postgres) a materialized view.
- **Values**: NULLs everywhere, unicode and emoji, apostrophes, ~10 kB text
  cells, 8 kB blobs, and an `all_types` table with one ordinary row, one
  all-NULL row, and one row of extremes — including types with no dedicated
  decoder (`sql_variant`, `hierarchyid`, `tsvector`, `point`, `xml`) so the
  display fallbacks show up.
- **Volume**: `events` has 5 000 rows for paging, sorting and filtering.

Backend-specific extras: Postgres adds enum and array columns, `infinity`
timestamps and a materialized view; SQLite adds a `WITHOUT ROWID` table, a
`VIRTUAL` generated column, a table with no declared column types, quoted
column names with spaces and reserved words, and values that contradict their
column's declared affinity.

## Apple Silicon

There is no arm64 SQL Server image. Uncomment `platform: linux/amd64` in
`docker-compose.yml` to run it emulated, or test SQL Server on another machine.
