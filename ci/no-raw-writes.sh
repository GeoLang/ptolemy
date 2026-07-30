#!/usr/bin/env bash
# Refuse a mutation in ptolemy-api that has not been through the write ladder.
#
# The types make the guarded path the default: a write lives in
# ptolemy-storage's `writes` module, takes a `&WriteGrant`, and reads the id it
# writes under off that grant, so it cannot run without the ladder and cannot be
# aimed anywhere else. Nothing in the type system stops a handler from going
# around all of that with raw SQL on the read pool, and that is exactly how the
# 39 unguarded routes happened. This is what closes it.
#
# What it covers:
#   - write SQL (INSERT/UPDATE/DELETE/TRUNCATE/DDL) anywhere in ptolemy-api/src
#   - `.execute(`, which runs a statement for effect rather than for rows
#   - `unguarded_pool`, the CLI and test-fixture handle
#   - `WriteGrant::unenforced`, the dev-mode constructor that skips the ladder
#   - `Writer::Unenforced`, the writer value that makes the ladder pass
#
# What it cannot cover: a mutating Postgres function called through SELECT.
# `SELECT topology.CreateTopology(...)` and `SELECT topology.AddFace(...)` in
# topology.rs both write and read as ordinary queries here. Those routes are
# instance-admin-only in auth.rs, which is their gate; see the note there.

set -uo pipefail

cd "$(dirname "$0")/.."

src=crates/ptolemy-api/src
status=0

# ─── Allowlist ──────────────────────────────────────────────────────
#
# One entry per line, `path|exact source line, trimmed`. Matching on the text
# rather than a line number means a moved line still passes and an edited one
# fails, which is the review we want. Keep this short.
allowed_lines() {
  cat <<'ENTRIES'
crates/ptolemy-api/src/routes.rs|match sqlx::query("SELECT 1").execute(state.read_pool()).await {
crates/ptolemy-api/src/grpc.rs|&ptolemy_storage::Writer::Unenforced,
ENTRIES
}
# The two entries above:
#   routes.rs   the readiness probe. `SELECT 1` run for its effect rather than
#               its row, which is what a liveness check is.
#   grpc.rs     bulk_import commits with no caller identity. Nothing constructs
#               or mounts this service, which is why it is here rather than
#               fixed; the comment above the line says an Actor has to be wired
#               through before it is mounted. Mounting it without doing that
#               reopens the whole bug class over gRPC.

# hits: `file:line:text` on stdin, one per line. Prints and fails anything the
# allowlist does not name.
report() {
  local label=$1 hit file rest line text trimmed
  while IFS= read -r hit; do
    [ -z "$hit" ] && continue
    file=${hit%%:*}
    rest=${hit#*:}
    line=${rest%%:*}
    text=${rest#*:}
    trimmed=$(printf '%s' "$text" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    # a comment naming one of these cannot run one
    case $trimmed in //*) continue ;; esac
    if allowed_lines | grep -qxF "$file|$trimmed"; then
      continue
    fi
    printf '%s:%s: %s\n    %s\n' "$file" "$line" "$label" "$trimmed"
    status=1
  done
}

# Write SQL. Matched case-insensitively and on word boundaries so a column named
# `updated_at` or a Rust `Vec::truncate` does not trip it.
report 'write SQL in ptolemy-api. Move it to ptolemy-storage::writes behind a &WriteGrant.' < <(
  grep -rniE '\b(insert +into|update +[a-z_"]+ +set|delete +from|truncate +table|drop +(table|schema|index)|alter +table|create +(table|index|schema))\b' \
    "$src" --include='*.rs'
)

# A statement run for its effect. Reads use fetch_one/fetch_all/fetch_optional.
report 'executes a statement. A write belongs in ptolemy-storage::writes behind a &WriteGrant.' < <(
  grep -rn '\.execute(' "$src" --include='*.rs'
)

# The escape hatches, each valid somewhere and none of them here.
report 'unguarded_pool is for the CLI and test fixtures, not for a request handler.' < <(
  grep -rn 'unguarded_pool' "$src" --include='*.rs'
)

report 'only the write layer decides that a request needs no ladder.' < <(
  grep -rn 'WriteGrant::unenforced' "$src" --include='*.rs' \
    | grep -v '^crates/ptolemy-api/src/visibility\.rs:'
)

report 'Writer::Unenforced makes the ladder pass. Only Actor::writer may build one.' < <(
  grep -rn 'Writer::Unenforced' "$src" --include='*.rs' \
    | grep -v '^crates/ptolemy-api/src/auth\.rs:'
)

if [ "$status" -ne 0 ]; then
  echo
  echo 'ptolemy-api must not write directly. See crates/ptolemy-storage/src/grant.rs.'
fi
exit "$status"
