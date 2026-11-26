#!/bin/sh
# Local development Postgres, using Apple container. Usage: scripts/dev-db.sh [start|stop]
set -e
case "${1:-start}" in
  start)
    container run -d --rm --name lbbot-db -p 127.0.0.1:55432:5432 \
      -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=lbbot postgres:17-alpine >/dev/null
    until container exec lbbot-db pg_isready -U postgres >/dev/null 2>&1; do sleep 1; done
    echo "DATABASE_URL=postgres://postgres:dev@127.0.0.1:55432/lbbot"
    ;;
  stop) container stop lbbot-db ;;
esac
