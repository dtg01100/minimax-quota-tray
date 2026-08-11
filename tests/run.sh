#!/usr/bin/env bash
#
# tests/run.sh — run the minimax-quota-tray unit test suite.
#
# MINIMAX_QUOTA_TEST=1 tells the app module, when imported, to skip main()
# so the harness can drive the scheduler without booting the tray.
#
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

status=0
for t in tests/*.test.js; do
  echo "== $t =="
  MINIMAX_QUOTA_TEST=1 gjs -m "$t" || status=1
  echo
done
exit $status
