#!/usr/bin/env bash
# Fetch the offline world basemap Solar bundles for the Places map. Like the
# face models (fetch-models.sh), these are NOT committed to git — run this once
# after cloning, before `npm run tauri dev|build`.
#
# What it fetches:
#   1. go-pmtiles CLI            — extraction tool (BSD-3, protomaps/go-pmtiles)
#   2. src-tauri/basemap/world.pmtiles
#        a z0-N world extract of the latest Protomaps daily basemap build
#        (ODbL, © OpenStreetMap contributors) — bundled as a Tauri resource so
#        the map works fully offline, no tile server ever contacted
#   3. public/basemap-assets/    — fonts (glyphs) + sprites for offline labels
#        (protomaps/basemaps-assets)
#
# SOLAR_BASEMAP_MAXZOOM (default 6) trades detail for size: 6 ≈ region/city
# scale (tens of MB), 7 is ~4× larger.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASEDIR="$ROOT/src-tauri/basemap"
ASSETS="$ROOT/public/basemap-assets"
TOOLS="$ROOT/scripts/.tools"
mkdir -p "$BASEDIR" "$ASSETS" "$TOOLS"

MAXZOOM="${SOLAR_BASEMAP_MAXZOOM:-6}"

# --- 1. go-pmtiles CLI ---
PM="$TOOLS/pmtiles"
if [ -x "$PM" ]; then
  echo "✓ pmtiles CLI already present"
else
  echo "↓ go-pmtiles CLI"
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) asset="Darwin_arm64" ;;
    Darwin-x86_64) asset="Darwin_x86_64" ;;
    Linux-x86_64) asset="Linux_x86_64" ;;
    *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
  url=$(curl -fsSL https://api.github.com/repos/protomaps/go-pmtiles/releases/latest \
    | grep browser_download_url | grep "$asset" | grep -o 'https[^"]*' | head -1)
  [ -n "$url" ] || { echo "could not find a go-pmtiles release asset" >&2; exit 1; }
  archive="$TOOLS/pmtiles-dl"
  curl -fL --retry 3 -o "$archive" "$url"
  case "$url" in
    *.zip) unzip -oq "$archive" -d "$TOOLS" pmtiles ;;
    *) tar -xzf "$archive" -C "$TOOLS" pmtiles ;;
  esac
  rm -f "$archive"
  chmod +x "$PM"
fi

# --- 2. world extract from the latest daily build ---
# -s (non-empty): the repo build uses a 0-byte placeholder so the Tauri resource
# check passes before this script has run; the app treats size 0 as "no map".
OUT="$BASEDIR/world.pmtiles"
if [ -s "$OUT" ]; then
  echo "✓ world.pmtiles already present ($(du -h "$OUT" | cut -f1))"
else
  ok=""
  for i in 1 2 3 4 5 6 7; do
    d=$(date -v-"${i}"d +%Y%m%d 2>/dev/null || date -d "-${i} days" +%Y%m%d)
    src="https://build.protomaps.com/${d}.pmtiles"
    echo "… trying daily build $d (maxzoom $MAXZOOM)"
    if "$PM" extract "$src" "$OUT" --maxzoom="$MAXZOOM" --download-threads=4; then
      ok=1
      break
    fi
    rm -f "$OUT"
  done
  [ -n "$ok" ] || { echo "could not fetch any recent daily build" >&2; exit 1; }
  echo "✓ world.pmtiles ($(du -h "$OUT" | cut -f1))"
fi

# --- 3. fonts + sprites (labels, offline) ---
if [ -d "$ASSETS/fonts" ] && [ -d "$ASSETS/sprites" ]; then
  echo "✓ basemap assets already present"
else
  echo "↓ fonts + sprites"
  tarball="$TOOLS/basemaps-assets.tar.gz"
  curl -fL --retry 3 -o "$tarball" \
    https://github.com/protomaps/basemaps-assets/archive/refs/heads/main.tar.gz
  tmp=$(mktemp -d)
  tar -xzf "$tarball" -C "$tmp"
  src="$tmp/basemaps-assets-main"
  mkdir -p "$ASSETS/fonts"
  # Only the stacks the light/dark flavors reference — the full set is ~40MB.
  for f in "Noto Sans Regular" "Noto Sans Medium" "Noto Sans Italic"; do
    [ -d "$src/fonts/$f" ] && cp -R "$src/fonts/$f" "$ASSETS/fonts/"
  done
  cp -R "$src/sprites" "$ASSETS/sprites"
  rm -rf "$tmp" "$tarball"
  echo "✓ basemap assets"
fi

echo "Done. Basemap in $BASEDIR, assets in $ASSETS"
