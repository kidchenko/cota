#!/usr/bin/env bash
#
# Builds Cota for Linux: the counterpart to build.sh (macOS) and build.ps1,
# and what build.sh runs when it finds itself on Linux.
#
#   ./build.sh                # test + release binary
#   ./build.sh --package      # ... then dist/: a .deb, an .rpm and a .tar.gz
#   ./build.sh --skip-tests
#
# The packages use the system's GTK rather than carrying their own: every
# desktop has it, and it is what makes the menu and the panel look at home.
# Their dependencies are read off the binary, plus the tray library, which is
# opened at run time and so is not on that list.
#
# Packaging needs dpkg-deb and dpkg-shlibdeps (dpkg-dev) for the .deb and
# rpmbuild (rpm) and file for the .rpm; each is skipped, with a warning, if
# missing.
set -euo pipefail
cd "$(dirname "$0")/../.."

package=0; skip_tests=0
for arg in "$@"; do
  case "$arg" in
    --package) package=1 ;;
    --skip-tests) skip_tests=1 ;;
    *) echo "unknown option: $arg" >&2; exit 1 ;;
  esac
done

cyan() { printf '\n\033[36m==> %s\033[0m\n' "$1"; }
ok()   { printf '    \033[32m%s\033[0m\n' "$1"; }
warn() { printf '    \033[33m%s\033[0m\n' "$1"; }

version="$(awk '/^\[/{p=($0=="[package]")} p&&/^version[[:space:]]*=/{split($0,a,"\"");print a[2];exit}' Cargo.toml)"
[ -n "$version" ] || { echo 'could not read [package].version from Cargo.toml' >&2; exit 1; }
target_dir="${CARGO_TARGET_DIR:-target}"

if [ "$skip_tests" -eq 0 ]; then
  cyan 'Running tests'
  cargo test --quiet
  ok 'tests passed'
fi

cyan 'Building release binary'
cargo build --release
exe="$(realpath "$target_dir/release/cota")"
ok "$(printf 'cota   %.2f MB' "$(echo "scale=2; $(stat -c%s "$exe")/1048576" | bc)")"

[ "$package" -eq 1 ] || exit 0

arch="$(uname -m)"
case "$arch" in
  x86_64) deb_arch=amd64 ;;
  aarch64) deb_arch=arm64 ;;
  *) deb_arch="$arch" ;;
esac

# The installed tree, as it lands under /usr. The icon is the ring itself, as
# on the other two: the SVG for every size, and the PNG for menus that will
# not draw an SVG.
cyan 'Staging'
root="$(mktemp -d)"
install -Dm755 "$exe" "$root/usr/bin/cota"
install -Dm644 packaging/linux/cota.desktop "$root/usr/share/applications/cota.desktop"
install -Dm644 assets/icon.svg "$root/usr/share/icons/hicolor/scalable/apps/cota.svg"
install -Dm644 assets/icon-256.png "$root/usr/share/icons/hicolor/256x256/apps/cota.png"
install -Dm644 LICENSE "$root/usr/share/doc/cota/copyright"
ok "$(find "$root" -type f | wc -l) files"
mkdir -p dist

description="Cota puts your Claude plan's usage limits in the tray: a ring that fills
as you use them, a panel with every limit and when it resets, and a
notification before you run out."

cyan 'Building .tar.gz'
tarball="dist/cota-$version-$arch-linux.tar.gz"
tar -C "$root/usr" -czf "$tarball" --owner=0 --group=0 bin share
# Same name every release, for /releases/latest/download/, as Cota.dmg.
cp -f "$tarball" "dist/cota-$arch-linux.tar.gz"
ok "$tarball (unpack into /usr/local or ~/.local)"

cyan 'Building .deb'
if command -v dpkg-deb >/dev/null && command -v dpkg-shlibdeps >/dev/null; then
  work="$(mktemp -d)"
  mkdir -p "$work/debian"
  echo 'Source: cota' > "$work/debian/control"
  # Its one warning is that the binary is not yet in a package's tree.
  shlibs="$(cd "$work" && dpkg-shlibdeps -O "$exe" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
  [ -n "$shlibs" ] || { echo 'dpkg-shlibdeps found no dependencies' >&2; exit 1; }
  mkdir -p "$root/DEBIAN"
  cat > "$root/DEBIAN/control" <<CONTROL
Package: cota
Version: $version
Architecture: $deb_arch
Maintainer: kidchenko <kidchenko@gmail.com>
Depends: $shlibs, libayatana-appindicator3-1 | libappindicator3-1
Recommends: libnotify-bin
Section: utils
Priority: optional
Homepage: https://josebarbosa.dev/cota/
Description: Your Claude usage limits, in the tray
$(sed 's/^/ /' <<<"$description")
CONTROL
  deb="dist/cota_${version}_$deb_arch.deb"
  dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
  cp -f "$deb" "dist/cota_$deb_arch.deb"
  rm -rf "$root/DEBIAN"
  ok "$deb"
  ok "depends: $shlibs"
else
  warn 'dpkg-deb or dpkg-shlibdeps missing (apt install dpkg-dev); skipping the .deb'
fi

cyan 'Building .rpm'
if command -v rpmbuild >/dev/null && command -v file >/dev/null; then
  top="$(mktemp -d)"
  mkdir -p "$top/SPECS"
  # The libraries the binary links are found by rpm itself; the tray library
  # is asked for by its soname, which Fedora and openSUSE both provide.
  cat > "$top/SPECS/cota.spec" <<SPEC
Name:       cota
Version:    $version
Release:    1
Summary:    Your Claude usage limits, in the tray
License:    MIT
URL:        https://josebarbosa.dev/cota/
Requires:   libayatana-appindicator3.so.1()(64bit)
Recommends: libnotify

%description
$description

%install
cp -a $root/. %{buildroot}/

%files
/usr/bin/cota
/usr/share/applications/cota.desktop
/usr/share/icons/hicolor/scalable/apps/cota.svg
/usr/share/icons/hicolor/256x256/apps/cota.png
%license /usr/share/doc/cota/copyright
SPEC
  rpmbuild --quiet -bb --define "_topdir $top" --define "_build_id_links none" \
    "$top/SPECS/cota.spec" >/dev/null
  rpm_file="$(ls "$top"/RPMS/*/*.rpm)"
  cp "$rpm_file" dist/
  cp -f "$rpm_file" "dist/cota.$arch.rpm"
  ok "dist/$(basename "$rpm_file")"
else
  warn 'rpmbuild or file missing (apt install rpm file); skipping the .rpm'
fi
