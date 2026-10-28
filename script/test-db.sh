#!/usr/bin/env bash
# Runs the ignored PostgreSQL, MySQL and MariaDB driver tests against the servers
# from `mise run db:up` (or the CI services) and fails if a filter ran no test.
set -euo pipefail

cd "$(dirname "$0")/.."

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

for filter in db::postgres:: db::mysql::; do
  cargo test --locked --lib "$filter" -- --ignored 2>&1 | tee "$log"
  if ! grep -Eq 'test result: ok\. [1-9][0-9]* passed' "$log"; then
    echo "error: no $filter test ran" >&2
    exit 1
  fi
done
