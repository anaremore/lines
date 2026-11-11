#!/usr/bin/env bash
# Install the system libraries GPUI links against on Linux CI and release runners
# (a subset of script/linux at the pinned Zed rev).
#
# Resilient to Ubuntu mirror outages, which blocked every PR for ~2 h on 2026-11-11
# (`Temporary failure resolving 'archive.ubuntu.com'`):
#   - apt retries each download and the whole install a few times, with backoff;
#   - every .deb it downloads is kept in $APT_CACHE_DIR, which the workflow saves with
#     actions/cache, so a later run whose mirrors are unreachable installs from that cache.
set -uo pipefail

PKGS=(
  build-essential pkg-config
  libfontconfig-dev libfreetype-dev
  libwayland-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev
  libvulkan1 libglib2.0-dev libzstd-dev libasound2-dev
)
CACHE="${APT_CACHE_DIR:-$HOME/apt-cache}"
APT_OPTS=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30
          -o Dir::Cache::archives="$CACHE")

mkdir -p "$CACHE/partial"
sudo chmod -R a+rwX "$CACHE"

installed() {
  for p in "${PKGS[@]}"; do
    dpkg -s "$p" >/dev/null 2>&1 || return 1
  done
}

done_ok() {
  # Let actions/cache read (and later restore) what apt wrote as root.
  sudo chown -R "$(id -u):$(id -g)" "$CACHE"
  rm -f "$CACHE/lock"
  exit 0
}

for attempt in 1 2 3; do
  sudo apt-get "${APT_OPTS[@]}" update || true
  if sudo apt-get "${APT_OPTS[@]}" install -y --no-install-recommends "${PKGS[@]}" && installed; then
    done_ok
  fi
  echo "apt install failed (attempt $attempt); retrying after $((attempt * 20)) s" >&2
  sleep $((attempt * 20))
done

# Mirrors unreachable: install the cached packages, dependencies included, without downloading.
if compgen -G "$CACHE/*.deb" >/dev/null; then
  echo "Installing from the cached .deb files in $CACHE" >&2
  sudo dpkg -i "$CACHE"/*.deb || sudo apt-get "${APT_OPTS[@]}" install -y -f --no-download
  if installed; then
    done_ok
  fi
fi

echo "Could not install the GPUI system libraries (mirrors unreachable and no usable cache)." >&2
exit 1
