#!/usr/bin/env bash
# Start the throwaway SQL Server container and apply the demo schema in
# sqlserver.sql.
#
#   scripts/seed/seed-sqlserver.sh
#
# The container (hubro-mssql-dev, port 14334) is separate from the
# integration-test one on 14333, so this never disturbs a test run. Re-running
# is safe: the SQL drops and recreates the whole `demo` database.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
compose=$here/docker-compose.yml
password='Str0ng!Passw0rd'

if ! docker compose version >/dev/null 2>&1; then
    echo "error: docker compose is not available" >&2
    exit 1
fi

echo "starting hubro-mssql-dev..."
docker compose -f "$compose" up -d sqlserver

# The image ships sqlcmd under one of two paths depending on its tools
# version, and only the 18 build takes -C (trust the self-signed certificate).
sqlcmd=
for candidate in /opt/mssql-tools18/bin/sqlcmd /opt/mssql-tools/bin/sqlcmd; do
    if docker compose -f "$compose" exec -T sqlserver test -x "$candidate" 2>/dev/null; then
        sqlcmd=$candidate
        break
    fi
done
if [[ -z $sqlcmd ]]; then
    echo "error: no sqlcmd found in the container" >&2
    exit 1
fi
# -I (quoted identifiers on) is not sqlcmd's default, but filtered indexes and
# computed columns refuse to be created without it.
args=(-S localhost -U sa -P "$password" -b -I -f 65001)
[[ $sqlcmd == *tools18* ]] && args+=(-C)

# SQL Server takes a while to accept connections after the container starts.
echo -n "waiting for sql server"
for _ in $(seq 60); do
    if docker compose -f "$compose" exec -T sqlserver \
        "$sqlcmd" "${args[@]}" -Q "SELECT 1" >/dev/null 2>&1; then
        ready=1
        break
    fi
    echo -n .
    sleep 2
done
echo
if [[ ${ready:-} != 1 ]]; then
    echo "error: sql server did not become ready in 120s" >&2
    echo "check: docker compose -f $compose logs sqlserver" >&2
    exit 1
fi

echo "applying sqlserver.sql..."
docker compose -f "$compose" exec -T sqlserver "$sqlcmd" "${args[@]}" < "$here/sqlserver.sql"

echo
echo "seeded mssql://sa:${password}@localhost:14334/demo?encrypt=on&trustServerCertificate=true"
echo "(in the connection form: host localhost, port 14334, database demo, user sa,"
echo " that password, and tick the trust-certificate box)"
echo "stop it with:  docker compose -f $compose stop sqlserver"
echo "wipe it with:  docker compose -f $compose down -v"
