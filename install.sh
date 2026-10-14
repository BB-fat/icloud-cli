#!/bin/sh
# icloud-cli installer for macOS and Linux. Non-interactive.
#   curl -fsSL https://github.com/BB-fat/icloud-cli/releases/latest/download/icloud-cli-installer.sh | sh
# Env: ICLOUD_VERSION (e.g. v0.1.0, default latest), ICLOUD_INSTALL_DIR (default ~/.local/bin)
set -eu
REPO="BB-fat/icloud-cli"
VERSION="${ICLOUD_VERSION:-latest}"
DIR="${ICLOUD_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s); arch=$(uname -m)
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "icloud-cli: unsupported architecture $arch" >&2; exit 1 ;;
esac
case "$os" in
  Darwin) target="$arch-apple-darwin" ;;
  Linux) target="$arch-unknown-linux-musl" ;;
  *) echo "icloud-cli: unsupported OS $os (use install.ps1 on Windows)" >&2; exit 1 ;;
esac

asset="icloud-cli-$target.tar.gz"
if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
dl() { if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"; else wget -q "$1" -O "$2"; fi; }
dl "$base/$asset" "$tmp/$asset"
dl "$base/$asset.sha256" "$tmp/$asset.sha256"
want=$(cut -d' ' -f1 "$tmp/$asset.sha256")
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1); else got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
[ "$want" = "$got" ] || { echo "icloud-cli: checksum mismatch" >&2; exit 1; }
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$DIR"
install -m 755 "$tmp/icloud-cli-$target/icloud" "$DIR/icloud" 2>/dev/null || { cp "$tmp/icloud-cli-$target/icloud" "$DIR/icloud"; chmod 755 "$DIR/icloud"; }
echo "{\"ok\":true,\"installed\":\"$DIR/icloud\",\"target\":\"$target\",\"version\":\"$VERSION\"}"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "note: add $DIR to PATH" >&2 ;; esac
