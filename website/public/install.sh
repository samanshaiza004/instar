#!/usr/bin/env sh
# Stable website entry point for the cargo-dist generated Instar installer.
set -eu

REPOSITORY="samanshaiza004/instar"
INSTALLER="instar-shell-installer.sh"
BASE_URL="${INSTAR_INSTALLER_GITHUB_BASE_URL:-https://github.com}"
INSTALLER_URL="${BASE_URL}/${REPOSITORY}/releases/latest/download/${INSTALLER}"

say() {
  printf '%s\n' "$*" >&2
}

need() {
  command -v "$1" >/dev/null 2>&1 || {
    say "instar installer: required command not found: $1"
    exit 1
  }
}

case "$(uname -s 2>/dev/null || true)" in
  Darwin|Linux) ;;
  MINGW*|MSYS*|CYGWIN*)
    say "instar installer: use the Windows PowerShell installer:"
    say "  irm https://github.com/${REPOSITORY}/releases/latest/download/instar-shell-installer.ps1 | iex"
    exit 1
    ;;
  *)
    say "instar installer: this installer supports macOS and Linux."
    exit 1
    ;;
esac

need curl

tmp_dir=$(mktemp -d 2>/dev/null || mktemp -d -t instar-install)
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT HUP INT TERM

say "instar installer: fetching the latest release installer..."
http_code=$(curl --proto '=https' --tlsv1.2 -sSLo "$tmp_dir/$INSTALLER" \
  -w '%{http_code}' "$INSTALLER_URL") || {
  say "instar installer: could not reach GitHub Releases."
  say "Check your network, then retry."
  exit 1
}

if [ "$http_code" = "404" ]; then
  say "instar installer: no tagged Instar binary release exists yet."
  say "Build the current developer preview from source:"
  say ""
  say "  git clone https://github.com/${REPOSITORY}.git"
  say "  cd instar"
  say "  cargo install --locked --path crates/instar-shell"
  say ""
  say "Guide: https://instar.samanshaiza.com/docs/development/build-from-source.html"
  exit 1
fi

if [ "$http_code" -lt 200 ] || [ "$http_code" -ge 300 ]; then
  say "instar installer: GitHub returned HTTP $http_code for $INSTALLER_URL"
  exit 1
fi

chmod +x "$tmp_dir/$INSTALLER"
say "instar installer: handing off to the release-pinned cargo-dist installer."
sh "$tmp_dir/$INSTALLER" "$@"
