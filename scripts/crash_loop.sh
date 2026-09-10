#!/usr/bin/env bash
# Kill-9 crash-consistency loop. Usage: scripts/crash_loop.sh [iterations] [seed] [delay_us]
set -euo pipefail
ITER="${1:-2000}"; SEED="${2:-1}"; DELAY="${3:-400}"
DIR="${TMPDIR:-/tmp}/ripindex-crash"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$REPO/Cargo.toml" --bin crash-harness --features crash-harness
exec "$REPO/target/release/crash-harness" run --dir "$DIR" --iterations "$ITER" --seed "$SEED" --delay-us "$DELAY"
