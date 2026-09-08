#!/usr/bin/env bash
# Replace the OWNER placeholder with a real GitHub owner (user or org) across
# every file that embeds a repository URL: Cargo.toml, README, CHANGELOG, the
# release workflow, the Homebrew formula, and both installers.
#
#   scripts/set-repo-owner.sh my-github-username
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <github-owner>" >&2
  exit 1
fi
OWNER="$1"
case "$OWNER" in
  OWNER) echo "error: that's the placeholder, not a real owner" >&2; exit 1 ;;
  *[!A-Za-z0-9-]*) echo "error: '$OWNER' is not a valid GitHub owner name" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."
FILES=$(grep -rl 'hadar01/ripindex' \
  --include='*.toml' --include='*.md' --include='*.yml' \
  --include='*.rb' --include='*.sh' --include='*.ps1' --include='*.lua' \
  . 2>/dev/null | grep -v '^./target' || true)

if [ -z "$FILES" ]; then
  echo "nothing to do: no hadar01/ripindex placeholders left"
  exit 0
fi

echo "$FILES" | while read -r f; do
  [ -n "$f" ] || continue
  sed -i "s|hadar01/ripindex|$hadar01/ripindex|g" "$f"
  echo "  updated $f"
done
echo
echo "Done. Remaining references to the placeholder (should be none):"
grep -rn 'hadar01/ripindex' --include='*' . 2>/dev/null | grep -v '^./target' | grep -v set-repo-owner || echo "  (none)"
