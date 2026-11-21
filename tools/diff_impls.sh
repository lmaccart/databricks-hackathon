#!/usr/bin/env bash
# Differential test: reference pipeline vs. an implementation binary on many seeds.
#   tools/diff_impls.sh BIN [SEEDS] [EVENTS]
#   e.g. tools/diff_impls.sh fast/target/release/shopping_sessions 20 30000
# Uses tools/mini_gen.py (kit-made dirty data). Also run the official generator.
set -euo pipefail
KIT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$(realpath "$1")"; SEEDS="${2:-10}"; EVENTS="${3:-20000}"
BASE=$(mktemp -d "${SCRATCH_DIR:-${TMPDIR:-/tmp}}/ss-diff.XXXXXXXX")
WORK="$BASE/case"
cleanup() {
  if [[ $? == 0 ]]; then rm -rf "$BASE"; else echo "Retained failing case: $WORK" >&2; fi
}
trap cleanup EXIT
fail=0
for seed in $(seq "${FIRST_SEED:-1}" $(( ${FIRST_SEED:-1} + SEEDS - 1 ))); do
  rm -rf "$WORK"; mkdir -p "$WORK"
  python3 "$KIT/tools/mini_gen.py" --seed "$seed" --events "$EVENTS" --out "$WORK/data"
  python3 "$KIT/reference/pipeline.py" "$WORK/data" "$WORK/ref" 2>/dev/null
  DATA_DIR="$WORK/data" OUTPUT_DIR="$WORK/impl" "$BIN" 2>/dev/null
  if python3 "$KIT/tools/compare_outputs.py" "$WORK/ref" "$WORK/impl" > "$WORK/cmp.txt"; then
    echo "seed $seed: MATCH"
  else
    echo "seed $seed: MISMATCH"; cat "$WORK/cmp.txt"; fail=1; break
  fi
done
exit $fail
