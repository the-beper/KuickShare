#!/bin/sh
# Renders the three package recipes from the templates into <out-root> (or one of the recipe targets
# used by build-rpm.sh / build-deb.sh / build-arch.sh):
#     build/packaging/render.sh <out-root>
# The version comes only from src/shell/CMakeLists.txt (checked against the tag by check-version.sh);
# the recipes are never edited by hand, version-bump.sh stays untouched. @SHA256@ is the checksum of
# the source tarball made by source-tarball.sh, which the recipes build from.
# Needs: sh, sed, sha256sum.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
[ $# -eq 1 ] && [ -n "$1" ] || { echo "usage: render.sh <out-root>" >&2; exit 2; }
out=$1

version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)
[ -n "$version" ] || { echo "render: no version in src/shell/CMakeLists.txt" >&2; exit 1; }
tarball=dist/tmp/packaging/kuickshare-$version.tar.gz
[ -f "$tarball" ] || "$root/build/packaging/source-tarball.sh" >/dev/null
sha=$(sha256sum "$tarball" | cut -d' ' -f1)
# Package build date for the rpm changelog and the deb header: English on purpose, the parsers
# (rpmbuild and dpkg) only take the "Mon Nov 01 2026" / RFC 2822 shapes (a translated locale breaks both).
changelog_date=$(LC_ALL=C date '+%a %b %d %Y')
# RFC 2822 time shape for debian/changelog.
deb_date=$(LC_ALL=C date -R -u)

pkg=kuickshare-$version
tmpl() { sed -e "s|@VERSION@|$version|g" -e "s|@SHA256@|$sha|g" -e "s|@TARBALL@|$pkg.tar.gz|g" \
             -e "s|@DATE_RPM@|$changelog_date|g" -e "s|@DATE_DEB@|$deb_date|g" "$1"; }

mkdir -p "$out/debian/source" "$out"
tmpl build/packaging/kuickshare.spec.in > "$out/kuickshare.spec"
tmpl build/packaging/PKGBUILD.in > "$out/PKGBUILD"
tmpl build/packaging/debian/control.in > "$out/debian/control"
tmpl build/packaging/debian/changelog.in > "$out/debian/changelog"
install -m 755 build/packaging/debian/rules "$out/debian/rules"
install -m 644 build/packaging/debian/copyright "$out/debian/copyright"
install -m 644 build/packaging/debian/source/format "$out/debian/source/format"
