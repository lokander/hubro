# Engine tests and the support matrix

Rules for `tests/`. The project-wide guidance is in the root `CLAUDE.md` — in particular that **a green engine-test run can mean nothing ran**, which is the first thing to check about anything here.

## Which engine needs which variable

Postgres integration tests skip unless `HUBRO_PG_TEST_URL` is set; SQL Server tests skip unless `HUBRO_MSSQL_TEST_URL` is set; TimescaleDB tests skip unless `HUBRO_TIMESCALE_TEST_URL` is set; Citus tests skip unless `HUBRO_CITUS_TEST_URL` is set; CockroachDB tests skip unless `HUBRO_CRDB_TEST_URL` is set; YugabyteDB tests skip unless `HUBRO_YUGABYTE_TEST_URL` is set; Materialize tests skip unless `HUBRO_MATERIALIZE_TEST_URL` is set; RisingWave tests skip unless `HUBRO_RISINGWAVE_TEST_URL` is set; SSH-tunnel tests need `HUBRO_SSH_TEST` (+ `HUBRO_SSH_TEST_KEY`/`_ENC_KEY`).

**`scripts/test-db.sh` starts and addresses them** (FRE-150): `./scripts/test-db.sh up` brings up every engine, waits for it, and runs its one-time setup; `env $(./scripts/test-db.sh env) cargo test` runs the suite against them without anyone retyping a URL. The `docker run` commands stay in the test-file headers as the source of truth — the script is the convenience.

Set 0 is the layout the headers document: `hubro-pg-test` (host port 5433), `hubro-mssql-test` (14333), `hubro-timescale-test` (5434), `hubro-citus-test` (5435), `hubro-crdb-test` (26257), `hubro-yugabyte-test` (5436), `hubro-materialize-test` (6875), `hubro-risingwave-test` (4566), `hubro-ssh-test` (2222). The exact `docker run` commands live in the test-file headers (`db_postgres.rs`, `db_sqlserver.rs`, `db_timescale.rs`, `db_citus.rs`, `db_cockroach.rs`, `db_yugabyte.rs`, `db_materialize.rs`, `db_risingwave.rs`, `tunnel.rs`).

Per-engine quirks: the Citus URL needs `?sslmode=disable` — that image ships an X.509 v1 certificate rustls won't parse (FRE-89) — and so does the CockroachDB one, which runs `--insecure` and serves no TLS at all. Pointing the shared suite at YugabyteDB needs `-- --test-threads=1`: that engine refuses concurrent DDL, so parallel tests fail on each other's fixtures rather than on anything real (FRE-91).

## Verifying a new engine

Engine-verification issues (FRE-88 onwards) have **two possible outcomes, and only one of them produces a test file.**

- *The engine works through an existing backend.* It gets one `tests/db_<engine>.rs` behind its own `HUBRO_<ENGINE>_TEST_URL`, covering only what that engine does differently — the shared surface is verified by pointing the existing suite at the same container. The header records that engine's findings (what needed fixing, what is absent, what is a known gap), and the engine gets a row in the support matrix.
- *The engine needs a backend of its own.* There is nothing to test, so there is no test file — writing one that cannot pass would be worse than none. QuestDB is the worked example (FRE-94): no `OFFSET`, so every paged read fails, and the conclusion was "that is a backend, not a verification" (FRE-149). Record it under **Tested and not supported** in the matrix with the version tried and the blocker, and file the follow-up issue. The verification still succeeded — a "no" that stops the next person re-testing it is a result.

## The support matrix

**The README's "Supported databases" matrix (FRE-96) is the published record**, assembled from those headers. Adding a row is the last step of a successful verification, with the exact version run and the date it passed both taken from an actual run rather than from the image tag — `SELECT version()` and friends, since the `:latest` tags in the test headers say nothing. The Browse/Edit/Script columns restate what `backend_capabilities` and `TableAccess::resolve` already decide, so a row that disagrees with the app is a bug in the row. `tests/support_matrix.rs` enforces the parts that can be checked offline: every `PgFlavor` and `Dialect` variant has a row, and no row records an image tag in place of a version.

## Fixtures

Fixture files go in `tests/fixtures/`, which `.gitattributes` marks `-text` so git never rewrites their bytes. `tests/line_endings.rs` names the files whose tests depend on that.
