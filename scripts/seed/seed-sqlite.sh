#!/usr/bin/env bash
# Build a throwaway SQLite database with the demo schema in sqlite.sql.
#
#   scripts/seed/seed-sqlite.sh [path/to/demo.db]
#
# Defaults to scripts/seed/demo.db, which is gitignored. The file is deleted
# and rebuilt from scratch on every run.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
db=${1:-$here/demo.db}

if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "error: sqlite3 is not installed" >&2
    exit 1
fi

rm -f -- "$db" "$db-wal" "$db-shm"
sqlite3 "$db" < "$here/sqlite.sql"

echo
echo "seeded $db"
echo "open it in hubro with connection type SQLite and that path."
