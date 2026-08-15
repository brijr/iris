#!/bin/sh
# shot installer — https://github.com/brijr/shot
# usage: curl -fsSL https://raw.githubusercontent.com/brijr/shot/main/install.sh | sh
set -eu

REPO="brijr/shot"
DIR="${SHOT_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)          TARGET="aarch64-apple-darwin" ;;
  Darwin-x86_64)         TARGET="x86_64-apple-darwin" ;;
  Linux-x86_64)          TARGET="x86_64-unknown-linux-musl" ;;
  *)
    echo "error: no prebuilt binary for $(uname -s) $(uname -m)" >&2
    echo "build from source instead: cargo install --git https://github.com/$REPO" >&2
    exit 1
    ;;
esac

URL="https://github.com/$REPO/releases/latest/download/shot-$TARGET.tar.gz"
echo "downloading shot ($TARGET)…"
mkdir -p "$DIR"
curl -fsSL "$URL" | tar -xz -C "$DIR"
chmod +x "$DIR/shot"
echo "installed $("$DIR/shot" --version) → $DIR/shot"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "note: $DIR is not on your PATH — add it to your shell profile" ;;
esac
