#!/usr/bin/env bash
#
# Starts, stops and addresses the engine containers the integration tests need
# (FRE-150).
#
# Everything is keyed on a SET number. Set 0 is exactly the layout the test
# headers document, so nothing that already exists has to change; sets 1, 2, …
# are independent stacks on ports offset by 100 per set, which is what lets two
# agents run the same suite at the same time without dropping each other's
# fixtures.
#
#   scripts/test-db.sh up            # set 0, every engine
#   scripts/test-db.sh up 1 pg crdb  # set 1, just those two
#   scripts/test-db.sh env 1         # KEY=VALUE lines for set 1
#   scripts/test-db.sh down 1        # stop, keeping the data
#   scripts/test-db.sh rm 1          # stop and delete — releases the set
#   scripts/test-db.sh status       # which sets are up (0-3)
#
# The usual way to run a suite against a set:
#
#   env $(scripts/test-db.sh env 1) cargo test --test db_cockroach
#
# (fish: `env (scripts/test-db.sh env 1) cargo test …` — no `$`.)
#
# The `docker run` lines in the test headers are deliberately NOT deleted. They
# document what each engine needs and why; this script is the convenience, not
# the source of truth. If they disagree, the header is right and this is stale.
#
# QuestDB is absent on purpose: it has no suite (FRE-94 concluded it needs its
# own backend, FRE-149). So is the SSH-tunnel stack in tests/tunnel.rs, which
# needs a docker network and generated keys.

set -euo pipefail

# Engines, in start order. Slow starters first so their boot overlaps the rest.
ENGINES=(yugabyte mssql citus crdb timescale materialize risingwave pg)

# Host port each engine uses in set 0. Offsets of 100 per set keep these clear
# of each other — the Postgres-family bases are one apart, so a smaller stride
# would collide.
port_base() {
  case "$1" in
    pg) echo 5433 ;;
    mssql) echo 14333 ;;
    timescale) echo 5434 ;;
    citus) echo 5435 ;;
    crdb) echo 26257 ;;
    yugabyte) echo 5436 ;;
    materialize) echo 6875 ;;
    risingwave) echo 4566 ;;
    *) die "unknown engine: $1" ;;
  esac
}

# The port each engine listens on inside its container — the other half of the
# published mapping, and what a probe sharing the container's network namespace
# has to dial. Written out rather than reusing `port_base`, which happens to
# match for two of these and would be a coincidence to depend on.
internal_port() {
  case "$1" in
    pg | timescale | citus) echo 5432 ;;
    mssql) echo 1433 ;;
    crdb) echo 26257 ;;
    yugabyte) echo 5433 ;;
    materialize) echo 6875 ;;
    risingwave) echo 4566 ;;
    *) die "unknown engine: $1" ;;
  esac
}

# Yugabyte also publishes its admin UI; kept off everything else's ports.
YUGABYTE_UI_BASE=15433

die() {
  echo "test-db: $*" >&2
  exit 1
}

port_for() { echo $(($(port_base "$1") + $2 * 100)); }

# Set 0 keeps the names the test headers use, so existing containers are reused
# rather than duplicated.
name_for() {
  local engine=$1 set=$2
  if [ "$set" -eq 0 ]; then
    echo "hubro-${engine}-test"
  else
    echo "hubro-${engine}-test-${set}"
  fi
}

env_var() {
  case "$1" in
    pg) echo HUBRO_PG_TEST_URL ;;
    mssql) echo HUBRO_MSSQL_TEST_URL ;;
    timescale) echo HUBRO_TIMESCALE_TEST_URL ;;
    citus) echo HUBRO_CITUS_TEST_URL ;;
    crdb) echo HUBRO_CRDB_TEST_URL ;;
    yugabyte) echo HUBRO_YUGABYTE_TEST_URL ;;
    materialize) echo HUBRO_MATERIALIZE_TEST_URL ;;
    risingwave) echo HUBRO_RISINGWAVE_TEST_URL ;;
    *) die "unknown engine: $1" ;;
  esac
}

url_for() {
  local engine=$1 port
  port=$(port_for "$engine" "$2")
  case "$engine" in
    pg) echo "postgres://tester:testpass@localhost:${port}/demo" ;;
    # Self-signed cert in the stock image, hence trustServerCertificate.
    mssql) echo "mssql://sa:Str0ng!Passw0rd@localhost:${port}/master?encrypt=on&trustServerCertificate=true" ;;
    timescale) echo "postgres://postgres:hubro@localhost:${port}/demo" ;;
    # sslmode=disable: the image's X.509 v1 certificate is one rustls refuses
    # to parse (FRE-89), so every other sslmode fails to connect at all.
    citus) echo "postgres://postgres:hubro@localhost:${port}/demo?sslmode=disable" ;;
    # sslmode=disable: --insecure serves no TLS. A secure cluster is ordinary.
    crdb) echo "postgres://root@localhost:${port}/demo?sslmode=disable" ;;
    yugabyte) echo "postgres://yugabyte@localhost:${port}/demo" ;;
    materialize) echo "postgres://materialize@localhost:${port}/materialize" ;;
    risingwave) echo "postgres://root@localhost:${port}/dev" ;;
  esac
}

# `container inspect`, not plain `inspect`: the latter also matches images, so
# a name that happens to collide with one would read as an existing container.
container_exists() { docker container inspect "$1" >/dev/null 2>&1; }
container_running() { [ "$(docker container inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = "true" ]; }

# A container created earlier keeps whatever ports it was created with, and
# `docker start` will not change them. Without this, reusing one built on a
# different port reports — and prints a URL for — the port this script *would*
# have used, which is a connection refused at best and the wrong database at
# worst.
check_port() {
  local name=$1 expected=$2 actual
  actual=$(docker container port "$name" | sed -n 's/.*:\([0-9]*\)$/\1/p' | sort -u | tr '\n' ' ')
  case " $actual " in
    *" $expected "*) ;;
    *) die "$name publishes port(s) '${actual% }', not $expected — remove it first ('rm' the set) or free the port" ;;
  esac
}

# Creates the container if it is absent, starts it if it is merely stopped.
create() {
  local engine=$1 set=$2 name port
  name=$(name_for "$engine" "$set")
  port=$(port_for "$engine" "$set")

  if container_running "$name"; then
    check_port "$name" "$port"
    echo "  $name already running (port $port)"
    return 0
  fi
  if container_exists "$name"; then
    echo "  $name exists, starting (port $port)"
    docker start "$name" >/dev/null
    check_port "$name" "$port"
    return 0
  fi

  echo "  creating $name on port $port"
  case "$engine" in
    pg)
      docker run -d --name "$name" -e POSTGRES_PASSWORD=testpass \
        -e POSTGRES_USER=tester -e POSTGRES_DB=demo -p "${port}:5432" \
        postgres:17-alpine >/dev/null
      ;;
    mssql)
      docker run -d --name "$name" -e ACCEPT_EULA=Y \
        -e 'MSSQL_SA_PASSWORD=Str0ng!Passw0rd' -p "${port}:1433" \
        mcr.microsoft.com/mssql/server:2022-latest >/dev/null
      ;;
    timescale)
      docker run -d --name "$name" -p "${port}:5432" \
        -e POSTGRES_PASSWORD=hubro timescale/timescaledb:latest-pg17 >/dev/null
      ;;
    citus)
      docker run -d --name "$name" -p "${port}:5432" \
        -e POSTGRES_PASSWORD=hubro citusdata/citus:latest >/dev/null
      ;;
    crdb)
      docker run -d --name "$name" -p "${port}:26257" \
        cockroachdb/cockroach:latest start-single-node --insecure >/dev/null
      ;;
    yugabyte)
      docker run -d --name "$name" -p "${port}:5433" \
        -p "$((YUGABYTE_UI_BASE + set * 100)):15433" \
        yugabytedb/yugabyte:latest bin/yugabyted start --background=false >/dev/null
      ;;
    materialize)
      docker run -d --name "$name" -p "${port}:6875" \
        materialize/materialized:latest >/dev/null
      ;;
    risingwave)
      docker run -d --name "$name" -p "${port}:4566" \
        risingwavelabs/risingwave:latest single_node >/dev/null
      ;;
  esac
}

# Blocks until the engine answers, then runs its one-time setup. Both are here
# rather than in `create` because a container that already existed still has to
# be waited for, and the setup steps are all idempotent.
ready() {
  local engine=$1 set=$2 name
  name=$(name_for "$engine" "$set")

  local deadline
  deadline=$((SECONDS + ${TEST_DB_WAIT_SECONDS:-240}))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if probe "$engine" "$name"; then
      echo "  $name ready"
      return 0
    fi
    sleep 2
  done
  die "$name did not become ready in ${TEST_DB_WAIT_SECONDS:-240}s — check 'docker logs $name'"
}

# One engine's liveness check *and* its initialisation, in the same step: for
# most of these the setup is a `CREATE DATABASE` that only succeeds once the
# server is up, so a separate readiness probe would just be the same call twice.
probe() {
  local engine=$1 name=$2
  case "$engine" in
    pg)
      docker exec "$name" pg_isready -U tester -d demo >/dev/null 2>&1
      ;;
    mssql)
      docker exec "$name" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
        -U sa -P 'Str0ng!Passw0rd' -Q 'SELECT 1' >/dev/null 2>&1
      ;;
    timescale)
      docker exec "$name" pg_isready -U postgres >/dev/null 2>&1 || return 1
      docker exec "$name" psql -U postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname = 'demo'" 2>/dev/null | grep -q 1 ||
        docker exec "$name" psql -U postgres -c 'CREATE DATABASE demo' >/dev/null 2>&1
      ;;
    citus)
      docker exec "$name" pg_isready -U postgres >/dev/null 2>&1 || return 1
      docker exec "$name" psql -U postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname = 'demo'" 2>/dev/null | grep -q 1 ||
        docker exec "$name" psql -U postgres -c 'CREATE DATABASE demo' >/dev/null 2>&1
      # The coordinator must be allowed to hold shards, or create_distributed_table
      # fails with "replication_factor (1) exceeds number of worker nodes (0)"
      # on a single-node cluster (FRE-89).
      docker exec "$name" psql -U postgres -d demo \
        -c 'CREATE EXTENSION IF NOT EXISTS citus' \
        -c "SELECT citus_set_coordinator_host('localhost', 5432)" \
        -c "SELECT citus_set_node_property('localhost', 5432, 'shouldhaveshards', true)" \
        >/dev/null 2>&1
      ;;
    crdb)
      docker exec "$name" ./cockroach sql --insecure \
        -e 'CREATE DATABASE IF NOT EXISTS demo' >/dev/null 2>&1
      ;;
    yugabyte)
      # The bash -c wrapper is load-bearing: ysqlsh binds the container's own
      # address, and an unwrapped $(hostname -i) would expand on the host.
      docker exec "$name" bash -c \
        '/home/yugabyte/bin/ysqlsh -h $(hostname -i) -U yugabyte -tAc "SELECT 1"' \
        >/dev/null 2>&1 || return 1
      docker exec "$name" bash -c \
        '/home/yugabyte/bin/ysqlsh -h $(hostname -i) -U yugabyte -tAc "SELECT 1 FROM pg_database WHERE datname = '"'"'demo'"'"'" | grep -q 1 ||
         /home/yugabyte/bin/ysqlsh -h $(hostname -i) -U yugabyte -c "CREATE DATABASE demo"' \
        >/dev/null 2>&1
      ;;
    materialize | risingwave)
      # Neither image ships a client, so the probe comes from outside.
      #
      # NOT a bare TCP connect: with Docker's default userland proxy,
      # `docker-proxy` accepts the published port the moment the container is
      # created, so a `/dev/tcp` open succeeds while the engine is still
      # starting and `up` reports ready seconds — on a cold start, a minute —
      # before a client can connect. `pg_isready` speaks enough of the protocol
      # to tell the difference; both engines answer it (exit 0) once really up,
      # and an unused port gives exit 2.
      #
      # Joins the container's *own* network namespace rather than using
      # `--network host`: host networking is unavailable or off by default on
      # Docker Desktop, which the macOS and Windows dev machines run. Dialling
      # the internal port from inside also skips `docker-proxy` entirely, which
      # is the thing that was answering early.
      docker run --rm --network "container:${name}" postgres:17-alpine \
        pg_isready -h 127.0.0.1 -p "$(internal_port "$engine")" >/dev/null 2>&1
      ;;
  esac
}

# Resolves the engine list into SELECTED, validating each name *in the calling
# shell*.
#
# Deliberately a global rather than something read through `mapfile < <(…)`:
# that runs the producer in a process substitution, where `die`'s `exit 1`
# kills only the subshell. `mapfile` still exits 0, `set -e` sees nothing, and
# a typo'd engine name silently drops itself and everything after it from the
# list — which for `env` means the suite runs with that variable unset, the
# engine's tests skip, and the run reads as a pass. That is the exact failure
# this script exists to remove, so the validation must be able to stop it.
SELECTED=()
select_engines() {
  if [ "$#" -eq 0 ]; then
    SELECTED=("${ENGINES[@]}")
    return
  fi
  local engine
  for engine in "$@"; do
    port_base "$engine" >/dev/null # dies here, in the caller's shell
  done
  SELECTED=("$@")
}

cmd_up() {
  local set=$1
  shift
  select_engines "$@"
  echo "starting set $set: ${SELECTED[*]}"
  local engine
  for engine in "${SELECTED[@]}"; do create "$engine" "$set"; done
  # Waited for in a second pass so every container's boot overlaps.
  for engine in "${SELECTED[@]}"; do ready "$engine" "$set"; done
  echo
  echo "run a suite against it with:"
  echo "  env \$($0 env $set) cargo test"
}

cmd_down() {
  local set=$1
  shift
  local engine name
  select_engines "$@"
  for engine in "${SELECTED[@]}"; do
    name=$(name_for "$engine" "$set")
    if container_running "$name"; then
      docker stop "$name" >/dev/null
      echo "  stopped $name"
    fi
  done
}

# Stops *and deletes*. `down` keeps a container so restarting it is instant,
# which is what you want for set 0; a throwaway set an agent owned wants to go
# away entirely, and leaving it stopped would quietly accumulate stacks.
cmd_rm() {
  local set=$1
  shift
  local engine name
  select_engines "$@"
  for engine in "${SELECTED[@]}"; do
    name=$(name_for "$engine" "$set")
    if container_exists "$name"; then
      docker rm -f "$name" >/dev/null
      echo "  removed $name"
    fi
  done
}

cmd_env() {
  local set=$1
  shift
  local engine
  select_engines "$@"
  for engine in "${SELECTED[@]}"; do
    # Only engines that are actually up: an unset variable makes that engine's
    # tests skip, which is the honest outcome. Printing a URL for a container
    # that is not running would make them fail instead, and look like the
    # engine broke.
    if container_running "$(name_for "$engine" "$set")"; then
      # Checked here as well as in `create`: this output is what gets handed to
      # cargo, and a URL for a container that is actually published somewhere
      # else fails as a connection error the suite reports as the engine's
      # fault. `create` catches it earlier, but only when `up` ran at all.
      check_port "$(name_for "$engine" "$set")" "$(port_for "$engine" "$set")"
      echo "$(env_var "$engine")=$(url_for "$engine" "$set")"
    fi
  done
}

cmd_status() {
  local set engine name
  for set in 0 1 2 3; do
    local running=()
    for engine in "${ENGINES[@]}"; do
      name=$(name_for "$engine" "$set")
      container_running "$name" && running+=("$engine")
    done
    if [ "${#running[@]}" -gt 0 ]; then
      echo "set $set: ${running[*]}"
    fi
  done
}

usage() {
  sed -n '/^# Starts,/,/^$/p' "$0" | sed 's|^# \{0,1\}||'
  exit 1
}

main() {
  command -v docker >/dev/null || die "docker is not on PATH"
  local command=${1:-}
  shift || true
  case "$command" in
    up | down | env)
      local set=0
      if [ "${1:-}" ] && [[ ${1} =~ ^[0-9]+$ ]]; then
        set=$1
        shift
      fi
      "cmd_$command" "$set" "$@"
      ;;
    rm)
      # No default set here, unlike every other command. Defaulting to 0 would
      # make a bare `rm` — the obvious way to type "clean up" — delete the
      # canonical containers the test headers document, with no confirmation,
      # in a tool whose whole point is that several people each own a set.
      # Everything else defaults to 0 harmlessly; this one has to be said.
      [ "${1:-}" ] && [[ ${1} =~ ^[0-9]+$ ]] ||
        die "rm needs the set number to delete, e.g. 'rm 1' (there is no default — set 0 is the shared one)"
      local set=$1
      shift
      cmd_rm "$set" "$@"
      ;;
    status) cmd_status ;;
    *) usage ;;
  esac
}

main "$@"
