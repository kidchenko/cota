#!/usr/bin/env bash
#
# Builds Cota.app for macOS. The counterpart to build.ps1.
#
#   ./build.sh                # test + release binary + Cota.app
#   ./build.sh --run          # ... then launch it in the menu bar
#   ./build.sh --panel        # ... then show the panel for 20s (pinned)
#   ./build.sh --shot         # ... then render docs/img/panel.png
#   ./build.sh --icons        # ... then render the icon faces to a contact sheet
#   ./build.sh --dmg          # ... then wrap Cota.app in a .dmg
#   ./build.sh --skip-tests   # binary + app only
#   ./build.sh --skip-bundle  # binary only
#
# --theme dark|light and --scale N tune --panel/--shot (defaults: dark, 3).
set -euo pipefail
cd "$(dirname "$0")"

run=0; panel=0; shot=0; icons=0; dmg=0; skip_tests=0; skip_bundle=0
theme=dark; scale=3
while [ $# -gt 0 ]; do
  case "$1" in
    --run) run=1 ;;
    --panel) panel=1 ;;
    --shot) shot=1 ;;
    --icons) icons=1 ;;
    --dmg) dmg=1 ;;
    --skip-tests) skip_tests=1 ;;
    --skip-bundle) skip_bundle=1 ;;
    --theme) theme="$2"; shift ;;
    --scale) scale="$2"; shift ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
  shift
done

cyan() { printf '\n\033[36m==> %s\033[0m\n' "$1"; }
ok()   { printf '    \033[32m%s\033[0m\n' "$1"; }
warn() { printf '    \033[33m%s\033[0m\n' "$1"; }

# Resolve cargo by PATH first, then its known install locations: a shell opened
# before rustup ran will not have it on PATH. rustup honours a custom CARGO_HOME,
# hence the ~/.local/share/cargo fallback alongside the default.
cargo="$(command -v cargo || true)"
for c in "$HOME/.local/share/cargo/bin/cargo" "$HOME/.cargo/bin/cargo"; do
  [ -n "$cargo" ] && break
  [ -x "$c" ] && cargo="$c"
done
if [ -z "$cargo" ]; then
  echo "cargo not found. Install the Rust toolchain:" >&2
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  exit 1
fi

if [ "$skip_tests" -eq 0 ]; then
  cyan 'Running tests'
  "$cargo" test --quiet
  ok 'tests passed'
fi

cyan 'Building release binary'
"$cargo" build --release
exe="target/release/cota"
ok "$(printf 'cota   %.2f MB' "$(echo "scale=2; $(stat -f%z "$exe")/1048576" | bc)")"

version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
[ -n "$version" ] || { echo 'could not read version from Cargo.toml' >&2; exit 1; }

if [ "$icons" -eq 1 ]; then
  cyan 'Rendering icon faces'
  dir='target/icons'
  "$exe" --dump-icons "$dir"
  if command -v python3 >/dev/null; then
    python3 assets/preview.py "$dir" && ok "contact sheets in $dir"
  else
    warn "python3 not found; raw .rgba files are in $dir"
  fi
fi

app="dist/Cota.app"
if [ "$skip_bundle" -eq 0 ]; then
  cyan 'Assembling Cota.app'
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$exe" "$app/Contents/MacOS/cota"
  sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
  printf 'APPL????' > "$app/Contents/PkgInfo"

  # A basic .icns from the 256px art. Only Finder and the About box show it —
  # LSUIElement keeps it out of the Dock — so a single-size conversion is plenty.
  if command -v sips >/dev/null && [ -f assets/icon-256.png ]; then
    if sips -s format icns assets/icon-256.png --out "$app/Contents/Resources/Cota.icns" >/dev/null 2>&1; then
      ok 'Cota.icns'
    else
      warn 'sips could not build Cota.icns; the app will use a default icon'
    fi
  fi
  # Ad-hoc sign so Gatekeeper and the keychain treat it as one stable identity
  # rather than reprompting on every rebuild. Harmless if codesign is absent.
  if command -v codesign >/dev/null; then
    codesign --force --sign - "$app" >/dev/null 2>&1 && ok 'ad-hoc signed' || warn 'codesign failed; unsigned'
  fi
  ok "$app"
fi

if [ "$dmg" -eq 1 ]; then
  cyan 'Building disk image'
  if command -v hdiutil >/dev/null; then
    dmg_path="dist/Cota-$version.dmg"
    rm -f "$dmg_path"
    hdiutil create -quiet -volname Cota -srcfolder "$app" -ov -format UDZO "$dmg_path"
    ok "$(printf 'Cota-%s.dmg   %.2f MB' "$version" "$(echo "scale=2; $(stat -f%z "$dmg_path")/1048576" | bc)")"
    ok "sha256  $(shasum -a 256 "$dmg_path" | cut -d' ' -f1)"
    # Unversioned alias, so github.com/.../releases/latest/download/Cota.dmg
    # keeps resolving without editing the download button each release. The
    # landing page links to exactly this name.
    cp -f "$dmg_path" dist/Cota.dmg
    ok 'Cota.dmg (unversioned alias for the website download button)'
  else
    warn 'hdiutil not found; skipping the disk image'
  fi
fi

if [ "$icons" -eq 0 ] && [ "$panel" -eq 1 ]; then
  cyan "Showing the panel ($theme) for 20 seconds"
  "$exe" --preview-panel "$theme" &
  ok 'pinned on screen - it will not dismiss when it loses focus'
fi

if [ "$shot" -eq 1 ]; then
  cyan "Rendering the landing-page shot ($theme, ${scale}x)"
  if command -v python3 >/dev/null; then
    raw='target/panel.rgba'
    "$exe" --render-panel "$raw" --theme "$theme" --scale "$scale"
    python3 assets/rgba2png.py "$raw" docs/img/panel.png && ok 'docs/img/panel.png'
  else
    warn 'python3 not found; cannot convert the raw output to PNG'
  fi
fi

if [ "$run" -eq 1 ]; then
  cyan 'Launching'
  open "$app"
  ok 'running in the menu bar'
fi
