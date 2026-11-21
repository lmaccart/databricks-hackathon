#!/bin/sh
# Adapter for bench_pair.py: the clock includes docker run (not image build).
# DATA_DIR must exist; OUTPUT_DIR and SCRATCH_DIR are fresh per run.
set -eu
: "${SS_BENCH_IMAGE:?}" "${DATA_DIR:?}" "${OUTPUT_DIR:?}" "${SCRATCH_DIR:?}" "${NCPU:?}"
mem=${SS_BENCH_MEMORY_GB:-24}
chmod 777 "$OUTPUT_DIR" "$SCRATCH_DIR"
exec docker run --rm --platform linux/amd64 --network none --read-only \
    --user 1000:1000 --cap-drop ALL --security-opt no-new-privileges \
    --tmpfs /tmp:rw,size=64m,mode=1777 --pids-limit 4096 \
    --cpus "$NCPU" --memory "${mem}g" --memory-swap "${mem}g" \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output -e HOME=/scratch \
    -e SCRATCH_DIR=/scratch -e NCPU="$NCPU" -e MEM_GB="$mem" \
    -v "$DATA_DIR:/data:ro" -v "$OUTPUT_DIR:/output" -v "$SCRATCH_DIR:/scratch" \
    "$SS_BENCH_IMAGE"
