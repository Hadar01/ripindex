#!/usr/bin/env bash
# Fill the sha256 placeholders in packaging/homebrew/ripindex.rb from a
# published release's SHA256SUMS.
#
#   scripts/update-homebrew-formula.sh v0.1.0
#
# Then copy the result into your tap repo as Formula/ripindex.rb.
set -euo pipefail

TAG="${1:-}"
if [ -z "$TAG" ]; then
  echo "usage: $0 <tag>   (e.g. $0 v0.1.0)" >&2
  exit 1
fi
case "$TAG" in v*) ;; *) TAG="v$TAG" ;; esac
VERSION="${TAG#v}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FORMULA="$REPO_ROOT/packaging/homebrew/ripindex.rb"
OWNER_REPO="$(grep -m1 -oE 'github\.com/[^/]+/ripindex' "$FORMULA" | head -1 | sed 's|github.com/||')"
SUMS_URL="https://github.com/$OWNER_REPO/releases/download/$TAG/SHA256SUMS"

echo "==> fetching $SUMS_URL"
SUMS="$(curl -fsSL "$SUMS_URL")" || {
  echo "error: could not fetch SHA256SUMS for $TAG." >&2
  echo "       Has the release workflow finished? Check:" >&2
  echo "       https://github.com/$OWNER_REPO/releases/tag/$TAG" >&2
  exit 1
}

# The formula also carries the version, so a re-run retargets it wholesale.
sed -i.bak "s|^  version \".*\"|  version \"$VERSION\"|" "$FORMULA"

missing=0
for target in aarch64-apple-darwin x86_64-apple-darwin \
              aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  asset="ripindex-${VERSION}-${target}.tar.gz"
  sha="$(printf '%s\n' "$SUMS" | awk -v a="$asset" '$2 ~ a { print $1; exit }' | tr -d '*')"
  placeholder="REPLACE_WITH_SHA256_$(printf '%s' "$target" | tr '-' '_')"
  if [ -z "$sha" ]; then
    echo "  !! no checksum for $asset in the release" >&2
    missing=1
    continue
  fi
  # Replace either the original placeholder or a previous run's checksum, so
  # this is idempotent across releases.
  if grep -q "$placeholder" "$FORMULA"; then
    sed -i "s|$placeholder|$sha|" "$FORMULA"
  else
    # Rewrite the sha256 line that follows this target's url line.
    sed -i "\|$target\.tar\.gz|,+1 s|^\(\s*\)sha256 \".*\"|\1sha256 \"$sha\"|" "$FORMULA"
  fi
  echo "  $target  ${sha:0:16}..."
done
rm -f "$FORMULA.bak"

echo
if [ "$missing" -ne 0 ]; then
  echo "Some checksums were missing - the formula is incomplete." >&2
  exit 1
fi
if grep -q 'REPLACE_WITH_SHA256' "$FORMULA"; then
  echo "Placeholders still remain:" >&2
  grep -n 'REPLACE_WITH_SHA256' "$FORMULA" >&2
  exit 1
fi
echo "$FORMULA is complete for $TAG."
echo "Next: copy it to your tap repo as Formula/ripindex.rb, commit, push."
echo "Then: brew install $(printf '%s' "$OWNER_REPO" | cut -d/ -f1)/tap/ripindex"
