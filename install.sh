#!/bin/sh
# iris installer — https://github.com/brijr/iris
# usage: curl -fsSL https://raw.githubusercontent.com/brijr/iris/main/install.sh | sh
set -eu

REPO="brijr/iris"
DIR="${IRIS_INSTALL_DIR:-$HOME/.local/bin}"

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

URL="https://github.com/$REPO/releases/latest/download/iris-$TARGET.tar.gz"
echo "downloading iris ($TARGET)…"
mkdir -p "$DIR"
curl -fsSL "$URL" | tar -xz -C "$DIR"
chmod +x "$DIR/iris"
echo "installed $("$DIR/iris" --version) → $DIR/iris"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "note: $DIR is not on your PATH — add it to your shell profile" ;;
esac
