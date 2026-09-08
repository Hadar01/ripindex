#!/usr/bin/env bash
# Reproduce the benchmark numbers in README.md against a pinned public corpus.
#
#   scripts/demo-corpus.sh                # CPython v3.12.0 (default)
#   scripts/demo-corpus.sh linux          # Linux v6.6 (much larger; Unix only)
#   scripts/demo-corpus.sh cpython /tmp/c # choose where the corpus is cloned
#
# "Fast on my machine against my files" is not a claim anyone can check. This
# pins an exact tag of an exact public repository and prints every command it
# runs, so the numbers can be repeated or contradicted.
set -euo pipefail

CORPUS_NAME="${1:-cpython}"
WORKDIR="${2:-${TMPDIR:-/tmp}/ripindex-corpora}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${RIPINDEX_BIN:-$REPO_ROOT/target/release/ripindex}"
[ -x "$BIN" ] || BIN="$BIN.exe"

case "$CORPUS_NAME" in
  cpython)
    URL="https://github.com/python/cpython.git"; TAG="v3.12.0"
    QUERIES=(-q "PyObject" -q "PyUnicode_FromString" -q "asyncio AND subprocess" -q '"reference count"')
    RG_TERMS=("PyObject" "PyUnicode_FromString" "reference count")
    ;;
  linux)
    URL="https://github.com/torvalds/linux.git"; TAG="v6.6"
    QUERIES=(-q "kmalloc" -q "netif_receive_skb" -q "spinlock AND irq" -q '"memory barrier"')
    RG_TERMS=("kmalloc" "netif_receive_skb" "memory barrier")
    # The kernel tree contains filenames Windows rejects (aux.c, and paths that
    # collide case-insensitively), so the checkout cannot complete there.
    case "$(uname -s)" in
      MINGW*|MSYS*|CYGWIN*)
        echo "error: the Linux kernel tree cannot be checked out on Windows" >&2
        echo "       (filenames like 'aux.c' are reserved; case-only collisions too)." >&2
        echo "       Use: $0 cpython" >&2
        exit 1 ;;
    esac
    ;;
  *) echo "usage: $0 [cpython|linux] [workdir]" >&2; exit 1 ;;
esac

if [ ! -x "$BIN" ]; then
  echo "==> building release binary"
  ( cd "$REPO_ROOT" && cargo build --release --locked --bin ripindex )
  BIN="$REPO_ROOT/target/release/ripindex"; [ -x "$BIN" ] || BIN="$BIN.exe"
fi

DEST="$WORKDIR/$CORPUS_NAME-${TAG}"
mkdir -p "$WORKDIR"
if [ ! -d "$DEST/.git" ]; then
  echo "==> git clone --depth 1 --branch $TAG $URL"
  git clone --depth 1 --branch "$TAG" --single-branch "$URL" "$DEST"
else
  echo "==> reusing existing corpus at $DEST"
fi

FILES=$(find "$DEST" -type f -not -path '*/.git/*' | wc -l | tr -d ' ')
echo
echo "corpus:   $URL @ $TAG"
echo "path:     $DEST"
echo "files:    $FILES (excluding .git)"
echo

# Start from a cold index so the reported build time is a true first run.
rm -rf "$DEST/.ripindex"

echo "==> $BIN bench $DEST ${QUERIES[*]} --iterations 20"
echo
"$BIN" bench "$DEST" "${QUERIES[@]}" --iterations 20 2>/dev/null

# The comparison that matters is warm-vs-warm: by now the corpus is in page
# cache for both tools. ripgrep is only comparable on plain terms and phrases -
# boolean queries have no ripgrep equivalent.
if command -v rg >/dev/null 2>&1; then
  echo
  echo "== ripgrep $(rg --version | head -1 | awk '{print $2}') on the same corpus (warm, best of 5) =="
  printf '%-30s %10s %8s\n' "query" "best" "files"
  for term in "${RG_TERMS[@]}"; do
    best=""
    for _ in 1 2 3 4 5; do
      start=$(date +%s%N)
      count=$(rg -l "$term" "$DEST" 2>/dev/null | wc -l | tr -d ' ')
      elapsed=$(( ($(date +%s%N) - start) / 1000000 ))
      if [ -z "$best" ] || [ "$elapsed" -lt "$best" ]; then best="$elapsed"; fi
    done
    printf '%-30s %9sms %8s\n' "$term" "$best" "$count"
  done
  echo
  echo "Note: ripgrep matches substrings, ripindex matches whole tokens and their"
  echo "camelCase/snake_case parts, so the file counts differ by design. See the"
  echo "'Query semantics' section of README.md."
else
  echo
  echo "(ripgrep not on PATH - skipping the comparison)"
fi
