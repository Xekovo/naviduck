#!/usr/bin/env bash
# Dependency direction (A-02): the domain model, the drivers, the SQL layer and the pure helpers
# must not link GPUI. Only the application-state, session and UI crates may, and
# `naviduck-headless` proves model + drivers build and run on their own.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

trees="$(mktemp -d)"
trap 'rm -rf "$trees"' EXIT

for crate in naviduck-model naviduck-db naviduck-sql naviduck-schema naviduck-util naviduck-sql-log naviduck-headless naviduck-mysql naviduck-postgres naviduck-sqlite naviduck-dameng naviduck-mssql naviduck-redis naviduck-mongodb; do
    # Read the whole tree into a file first: `grep -q` in a pipeline stops reading early, and
    # with `pipefail` a cargo tree killed by SIGPIPE would make the check pass silently.
    if ! cargo tree -p "$crate" -e normal --prefix none >"$trees/$crate"; then
        echo "dependency check: cargo tree failed for $crate" >&2
        fail=1
    elif grep -qE '^gpui([- ]|$)' "$trees/$crate"; then
        echo "dependency check: $crate links gpui" >&2
        fail=1
    fi
done

for manifest in \
    crates/naviduck-model/Cargo.toml \
    crates/naviduck-db/Cargo.toml \
    crates/naviduck-sql/Cargo.toml \
    crates/naviduck-schema/Cargo.toml \
    crates/naviduck-util/Cargo.toml \
    crates/naviduck-headless/Cargo.toml \
    crates/naviduck-mysql/Cargo.toml \
    crates/naviduck-postgres/Cargo.toml \
    crates/naviduck-sqlite/Cargo.toml \
    crates/naviduck-dameng/Cargo.toml \
    crates/naviduck-mssql/Cargo.toml \
    crates/naviduck-redis/Cargo.toml \
    crates/naviduck-mongodb/Cargo.toml; do
    if grep -q 'gpui' "$manifest"; then
        echo "dependency check: $manifest depends on gpui" >&2
        fail=1
    fi
done

# App state moved to `naviduck-app-state`; a returning `naviduck-core` means a pane started
# reaching for GPUI through the model again.
if [ -d crates/naviduck-core ]; then
    echo "dependency check: crates/naviduck-core must stay gone (app state lives in naviduck-app-state)" >&2
    fail=1
fi

if [ "$fail" -eq 0 ]; then
    echo "dependency check passed"
fi
exit "$fail"
