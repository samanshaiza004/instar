#!/usr/bin/env bash
# Builds youth.samanshaiza.com: the mdbook developer guide (docs/book/) plus
# this directory's static landing page and install script, combined into
# website/dist/, which netlify.toml publishes.
#
# Local testing: run from anywhere (`bash website/build.sh`), or via the
# Netlify CLI (`netlify dev`), which runs this same command through
# netlify.toml's [build] section.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Pin the version so this build is reproducible outside CI, which installs
# mdbook via taiki-e/install-action without a pin. Bump deliberately; there
# is no requirement this track CI's version exactly.
MDBOOK_VERSION="0.5.4"

if ! command -v mdbook >/dev/null 2>&1; then
  echo "mdbook not found on PATH; downloading v${MDBOOK_VERSION} for the build image..."
  tmp_dir="$(mktemp -d)"
  curl -fsSL \
    "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
    -o "$tmp_dir/mdbook.tar.gz"
  tar -xzf "$tmp_dir/mdbook.tar.gz" -C "$tmp_dir"
  export PATH="$tmp_dir:$PATH"
fi

mdbook build docs/book

rm -rf website/dist
mkdir -p website/dist
cp -r target/mdbook website/dist/docs
cp -r website/public/. website/dist/

echo "Website build complete: website/dist/ (landing page at /, docs at /docs/, install script at /install.sh)"
