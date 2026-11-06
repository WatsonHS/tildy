#!/bin/sh
# Self-test helper: behaves like bench/fake-backend.sh, except that while the
# file named by TILDY_SELFTEST_FLAG exists, its content (e.g.
# "TILDY_FAKE_HALF_FRAME=1") is exported first. Lets a test change backend
# behaviour between two requests of the same prompt cycle (e.g. on resize).
if [ -n "${TILDY_SELFTEST_FLAG:-}" ] && [ -f "$TILDY_SELFTEST_FLAG" ]; then
  IFS= read -r kv < "$TILDY_SELFTEST_FLAG"
  export "$kv"
fi
exec "$(dirname "$0")/../../../bench/fake-backend.sh" "$@"
