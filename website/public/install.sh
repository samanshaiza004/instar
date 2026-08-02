#!/usr/bin/env sh
# Placeholder for `curl -fsSL https://youth.samanshaiza.com/install | sh`.
#
# Youth has not cut its first packaged release yet -- no prebuilt binaries,
# no `dist`-published installer. That's planned after the Scratchpad MVP
# ships and 0.1.0 release prep is done (see docs/DISTRIBUTION.md in the
# repository). This script deliberately does not attempt an install; it
# exits non-zero so nothing downstream mistakes it for a success.
set -eu

cat <<'BANNER'
Youth CLI -- no packaged install yet
=====================================

This script is a placeholder. Youth doesn't have a tagged release with
prebuilt binaries yet.

For now, build from source (requires a Rust toolchain with the
wasm32-wasip2 target -- run `youth doctor` after installing to check):

  cargo install youth-cli --git https://github.com/samanshaiza004/youth

Docs: https://youth.samanshaiza.com/docs/
Repo: https://github.com/samanshaiza004/youth

Once the first packaged release ships, this script will install a
prebuilt binary for your platform, the same way rustup or Homebrew do.
BANNER

exit 1
