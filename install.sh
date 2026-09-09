#!/bin/sh
# ripindex installer for macOS and Linux.
#
#   curl -LsSf https://github.com/Hadar01/ripindex/releases/latest/download/ripindex-installer.sh | sh
#
# Options (flags or environment):
#   --version X.Y.Z      install a specific release      (RIPINDEX_VERSION)
#   --to DIR             install into DIR                 (RIPINDEX_INSTALL_DIR)
#   --no-verify          skip SHA-256 verification (not recommended)
#
# Downloads are verified against the SHA-256 published alongside them. Any
# failure aborts without touching the install directory.
set -eu

REPO="Hadar01/ripindex"
BIN="ripindex"
VERSION="${RIPINDEX_VERSION:-}"
DEST="${RIPINDEX_INSTALL_DIR:-}"
VERIFY=1
TMP=""

say()  { printf '%s\n' "$*"; }
err()  { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "this installer needs '$1' on PATH"; }

cleanup() { [ -n "$TMP" ] && rm -rf "$TMP" || true; }
trap cleanup EXIT INT TERM

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; [ -n "$VERSION" ] || err "--version needs a value"; shift 2 ;;
    --to)      DEST="${2:-}";    [ -n "$DEST" ]    || err "--to needs a value";      shift 2 ;;
    --no-verify) VERIFY=0; shift ;;
    -h|--help) sed -n '2,14p' "$0" 2>/dev/null || say "see https://github.com/$REPO"; exit 0 ;;
    *) err "unknown option: $1" ;;
  esac
done

# --- target triple -----------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) err "unsupported OS '$os'. ripindex ships Linux and macOS builds; build from source with 'cargo install $BIN'." ;;
esac
case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) err "unsupported architecture '$arch'. Build from source with 'cargo install $BIN'." ;;
esac
TARGET="${arch_part}-${os_part}"

# --- fetch helper ------------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  fetch()      { curl -fsSL "$1" -o "$2"; }
  fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch()      { wget -qO "$2" "$1"; }
  fetch_stdout() { wget -qO- "$1"; }
else
  err "this installer needs 'curl' or 'wget' on PATH"
fi
need tar

# --- resolve version ---------------------------------------------------------
if [ -z "$VERSION" ]; then
  # Follow the /latest redirect rather than parsing the API, so this keeps
  # working unauthenticated even when API rate limits are exhausted.
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/v${VERSION#v}"
fi

TMP="$(mktemp -d 2>/dev/null || mktemp -d -t ripindex)"

# The asset name embeds the version, which we may not know yet. When it isn't
# pinned, read it from the release's SHA256SUMS - one small file that names
# every asset in the release.
if [ -z "$VERSION" ]; then
  fetch "$base/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null \
    || err "could not reach the latest release. Check https://github.com/$REPO/releases, or pass --version X.Y.Z."
  asset="$(awk -v t="$TARGET" '$2 ~ t && $2 ~ /\.tar\.gz$/ { print $2; exit }' "$TMP/SHA256SUMS" | tr -d '*')"
  [ -n "$asset" ] || err "the latest release has no build for $TARGET"
else
  asset="${BIN}-${VERSION#v}-${TARGET}.tar.gz"
fi

say "installing $asset"
fetch "$base/$asset" "$TMP/$asset" || err "download failed: $base/$asset"

# --- verify ------------------------------------------------------------------
if [ "$VERIFY" -eq 1 ]; then
  if command -v sha256sum >/dev/null 2>&1;   then sum="$(sha256sum "$TMP/$asset" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1;    then sum="$(shasum -a 256 "$TMP/$asset" | cut -d' ' -f1)"
  else sum=""; say "warning: no sha256sum/shasum found; skipping verification"
  fi
  if [ -n "$sum" ]; then
    [ -f "$TMP/SHA256SUMS" ] || fetch "$base/SHA256SUMS" "$TMP/SHA256SUMS" || true
    if [ -f "$TMP/SHA256SUMS" ]; then
      want="$(awk -v a="$asset" '$2 ~ a { print $1; exit }' "$TMP/SHA256SUMS" | tr -d '*')"
      [ -n "$want" ] || err "no checksum published for $asset"
      [ "$sum" = "$want" ] || err "checksum mismatch for $asset (expected $want, got $sum). Refusing to install."
      say "checksum ok"
    else
      err "could not fetch SHA256SUMS to verify the download (pass --no-verify to skip)"
    fi
  fi
fi

# --- install -----------------------------------------------------------------
tar xzf "$TMP/$asset" -C "$TMP" || err "could not extract $asset"
src="$(find "$TMP" -type f -name "$BIN" -perm -u+x 2>/dev/null | head -n1)"
[ -n "$src" ] || src="$(find "$TMP" -type f -name "$BIN" | head -n1)"
[ -n "$src" ] || err "archive did not contain a '$BIN' binary"

if [ -z "$DEST" ]; then
  # Prefer a dir already on PATH; otherwise the XDG-ish default.
  for d in "$HOME/.local/bin" "$HOME/bin"; do
    case ":$PATH:" in *":$d:"*) DEST="$d"; break ;; esac
  done
  [ -n "$DEST" ] || DEST="$HOME/.local/bin"
fi
mkdir -p "$DEST" || err "could not create $DEST"
install -m 0755 "$src" "$DEST/$BIN" 2>/dev/null || { cp "$src" "$DEST/$BIN" && chmod 0755 "$DEST/$BIN"; } \
  || err "could not install into $DEST"

say ""
say "installed $BIN -> $DEST/$BIN"
"$DEST/$BIN" --version 2>/dev/null || true
case ":$PATH:" in
  *":$DEST:"*) say "" ; say "Try:  $BIN search --root . TODO" ;;
  *) say ""
     say "$DEST is not on your PATH. Add it:"
     say "  echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.profile && . ~/.profile" ;;
esac
