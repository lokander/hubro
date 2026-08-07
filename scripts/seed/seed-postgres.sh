#!/usr/bin/env bash
# Start the throwaway Postgres container and apply the demo schema in
# postgres.sql.
#
#   scripts/seed/seed-postgres.sh
#
# The container (hubro-pg-dev, port 5434) is separate from the integration-test
# one on 5433, so this never disturbs a test run. Re-running is safe: the SQL
# drops and recreates everything it owns.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
compose=$here/docker-compose.yml

if ! docker compose version >/dev/null 2>&1; then
    echo "error: docker compose is not available" >&2
    exit 1
fi

echo "starting hubro-pg-dev..."
docker compose -f "$compose" up -d --wait postgres

echo "applying postgres.sql..."
docker compose -f "$compose" exec -T postgres \
    psql -v ON_ERROR_STOP=1 -q -U hubro -d demo < "$here/postgres.sql"

echo
echo "seeded postgres://hubro:hubropass@localhost:5434/demo"
echo "stop it with:  docker compose -f $compose stop postgres"
echo "wipe it with:  docker compose -f $compose down -v"
