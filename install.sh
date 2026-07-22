#!/bin/sh
set -eu

repository="algonacci/kamui"
install_dir="${KAMUI_INSTALL_DIR:-$HOME/.local/bin}"
os=$(uname -s)
arch=$(uname -m)

case "$os-$arch" in
  Linux-x86_64|Linux-amd64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-x86_64|Darwin-amd64) target="x86_64-apple-darwin" ;;
  Darwin-arm64|Darwin-aarch64) target="aarch64-apple-darwin" ;;
  *) echo "Unsupported platform: $os $arch" >&2; exit 1 ;;
esac

archive="kamui-$target.tar.gz"
release_url="https://github.com/$repository/releases/latest/download"
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT INT TERM

download() {
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    echo "curl or wget is required." >&2
    exit 1
  fi
}

download "$release_url/$archive" "$temp_dir/$archive"
download "$release_url/$archive.sha256" "$temp_dir/$archive.sha256"

expected=$(awk '{print $1}' "$temp_dir/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$temp_dir/$archive" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$temp_dir/$archive" | awk '{print $1}')
fi

if [ "$expected" != "$actual" ]; then
  echo "Checksum verification failed." >&2
  exit 1
fi

mkdir -p "$install_dir"
tar -xzf "$temp_dir/$archive" -C "$temp_dir"
install -m 755 "$temp_dir/kamui" "$install_dir/kamui"

echo "Kamui installed to $install_dir/kamui"
case ":$PATH:" in
  *":$install_dir:"*) echo "Run: kamui" ;;
  *)
    echo "$install_dir is not currently in PATH."
    echo "Add this line to your shell profile:"
    echo "  export PATH=\"$install_dir:\$PATH\""
    ;;
esac
