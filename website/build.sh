#!/usr/bin/env bash
# Build the Instar landing page and mdBook guide into website/dist/.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

MDBOOK_VERSION="0.5.4"

if ! command -v mdbook >/dev/null 2>&1; then
  echo "mdbook not found on PATH; downloading v${MDBOOK_VERSION} for the build image..."
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  curl -fsSL \
    "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
    -o "$tmp_dir/mdbook.tar.gz"
  tar -xzf "$tmp_dir/mdbook.tar.gz" -C "$tmp_dir"
  export PATH="$tmp_dir:$PATH"
fi

mdbook build docs/book

rm -rf website/dist
mkdir -p website/dist
cp -R target/mdbook website/dist/docs
cp -R website/public/. website/dist/

echo "Instar website built at website/dist/"
