#!/bin/sh
# Installs the latest mac-monitor release into ~/.local/bin and starts it at login.
#   curl -fsSL https://raw.githubusercontent.com/sakshamgupta05/mac-monitor/main/install.sh | sh
# Options (environment): PREFIX (default ~/.local), VERSION (default latest), REPO.
set -eu

REPO="${REPO:-sakshamgupta05/mac-monitor}"
PREFIX="${PREFIX:-$HOME/.local}"
VERSION="${VERSION:-latest}"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "mac-monitor needs an Apple Silicon Mac (found $(uname -s) $(uname -m))." >&2
  exit 1
fi

if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)"
fi
[ -n "$VERSION" ] || { echo "Could not determine the latest version." >&2; exit 1; }

NAME="mac-monitor-$VERSION-aarch64-apple-darwin"
URL="https://github.com/$REPO/releases/download/v$VERSION/$NAME.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $URL"
curl -fsSL "$URL" -o "$TMP/$NAME.tar.gz"
curl -fsSL "$URL.sha256" -o "$TMP/$NAME.tar.gz.sha256"
(cd "$TMP" && shasum -a 256 -c "$NAME.tar.gz.sha256")
tar -C "$TMP" -xzf "$TMP/$NAME.tar.gz"

mkdir -p "$PREFIX/bin"
install -m 755 "$TMP/$NAME/mac-monitor" "$PREFIX/bin/mac-monitor"
echo "Installed $PREFIX/bin/mac-monitor"
"$PREFIX/bin/mac-monitor" install
