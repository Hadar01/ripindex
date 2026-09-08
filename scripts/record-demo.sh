#!/usr/bin/env bash
# Drives the README demo at a watchable pace so the GIF can be captured in one
# take. On Windows/PowerShell use scripts/record-demo.ps1 instead - it needs no
# bash, and Start-Sleep being a cmdlet makes its pacing smoother.
# take with any screen recorder (ScreenToGif, N-Studio, LICEcap, peek...),
# without typing on camera or fumbling a command.
#
#   scripts/demo-corpus.sh cpython     # fetch the corpus (once)
#   scripts/record-demo.sh             # start the recorder, then run this
#
# Recommended: a ~110x28 terminal, font size 16-18, recording the terminal
# region only. Default pacing gives a ~25s recording; SPEED=1.0 is slower and
# more deliberate, SPEED=0.5 is brisk.
#
# Env:
#   CORPUS=<dir>   corpus to search (default: demo-corpus.sh's cpython path)
#   SPEED=<mult>   0.7 default; 1.0 slower and more deliberate, 0.5 brisk
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${RIPINDEX_BIN:-$REPO_ROOT/target/release/ripindex}"
[ -x "$BIN" ] || BIN="$BIN.exe"
CORPUS="${CORPUS:-${TMPDIR:-/tmp}/ripindex-corpora/cpython-v3.12.0}"
SPEED="${SPEED:-0.7}"

if [ ! -x "$BIN" ]; then
  echo "error: no release binary. Run: cargo build --release" >&2; exit 1
fi
if [ ! -d "$CORPUS" ]; then
  echo "error: no corpus at $CORPUS" >&2
  echo "       Run: scripts/demo-corpus.sh cpython" >&2
  echo "       Or:  CORPUS=/path/to/a/big/repo $0" >&2
  exit 1
fi

# Result paths are absolute, so a long corpus path wraps and dominates the
# frame. Warn rather than silently produce an ugly GIF.
if [ "${#CORPUS}" -gt 40 ]; then
  echo "note: CORPUS is ${#CORPUS} characters:" >&2
  echo "        $CORPUS" >&2
  echo "      Result paths are absolute, so this will wrap badly in the GIF." >&2
  echo "      Consider re-cloning somewhere short, e.g.:" >&2
  echo "        git clone --depth 1 -b v3.12.0 https://github.com/python/cpython ~/cpython" >&2
  echo "        CORPUS=~/cpython $0" >&2
  printf '      Continue anyway? [y/N] ' >&2
  read -r reply
  case "$reply" in [yY]*) ;; *) exit 1 ;; esac
fi

# Show `ripindex ...` in the typed commands, not an absolute build path.
PATH="$(dirname "$BIN"):$PATH"; export PATH

# Precompute delays once. Calling awk per character costs more than the sleep
# itself on Windows, where each process spawn is milliseconds.
CHUNK=3
CHUNK_DELAY="$(awk -v s="$SPEED" -v c="$CHUNK" 'BEGIN{printf "%.3f", 0.028*s*c}')"
scaled() { awk -v a="$1" -v s="$SPEED" 'BEGIN{printf "%.2f", a*s}'; }
GREEN=$'\033[1;32m'; DIM=$'\033[2m'; RESET=$'\033[0m'

type_cmd() {
  printf '%s' "${GREEN}\$${RESET} "
  local i
  # Emit CHUNK characters per sleep rather than one. `sleep` is an external
  # process costing ~70ms per call on Windows, which dominated the recording:
  # a 55-character command spent ~4s in process spawns alone. Three characters
  # per tick looks the same at this speed and cuts that to a third.
  for (( i=0; i<${#1}; i+=CHUNK )); do
    printf '%s' "${1:i:CHUNK}"
    sleep "$CHUNK_DELAY"
  done
  printf '\n'
}

say() { printf '%s%s%s\n' "$DIM" "$1" "$RESET"; }
run() { type_cmd "$1"; sleep "$(scaled 0.3)"; eval "$1" || true; sleep "$(scaled "${2:-2.2}")"; }

# Index off-camera: the GIF shows the warm path, which is what daily use feels
# like. The first-run cost is stated honestly in the README benchmark table
# rather than hidden here.
printf 'preparing (indexing off-camera, a few seconds)...\r'
ripindex index "$CORPUS" >/dev/null 2>&1 || true
printf '%*s\r' 60 ''

clear
files=$(find "$CORPUS" -type f -not -path '*/.git/*' 2>/dev/null | wc -l | tr -d ' ')
say "# CPython source: ${files} files, ~90 MiB. Indexed once, now warm."
sleep "$(scaled 1.1)"

run "ripindex search --root $CORPUS PyUnicode_FromString -n 4" 1.8

clear
say "# Boolean queries, phrases and negation - not just literals:"
sleep "$(scaled 0.7)"
run "ripindex search --root $CORPUS 'asyncio AND subprocess' -n 4" 1.8

clear
run "ripindex search --root $CORPUS '\"reference count\"' -n 4" 1.8

clear
say "# A daemon keeps it current and answers every query:"
sleep "$(scaled 0.7)"
run "ripindex status" 2.5
