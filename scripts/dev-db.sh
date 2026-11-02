#!/usr/bin/env bash
# Start (or reuse) a throwaway Postgres for development and tests, on port 5433.
#
#   ./scripts/dev-db.sh        start it
#   ./scripts/dev-db.sh stop   stop and remove it (the data goes with it)
#
# DATABASE_URL=postgres://store:store@127.0.0.1:5433/store
set -euo pipefail

NAME=store-pg
case "${1:-start}" in
  stop) docker rm -f "$NAME" >/dev/null 2>&1 && echo "stopped $NAME" || echo "$NAME was not running"; exit 0 ;;
  start) ;;
  *) echo "usage: $0 [start|stop]" >&2; exit 2 ;;
esac

if docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  echo "$NAME is already running"
else
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" -p 5433:5432 \
    -e POSTGRES_USER=store -e POSTGRES_PASSWORD=store -e POSTGRES_DB=store postgres:17 >/dev/null
fi
for _ in $(seq 1 40); do
  docker exec "$NAME" pg_isready -U store >/dev/null 2>&1 && { echo "ready: postgres://store:store@127.0.0.1:5433/store"; exit 0; }
  sleep 1
done
echo "postgres did not become ready" >&2
exit 1
