#!/bin/sh
# Release gate 2: the files the build jobs produced are exactly the expected ones, are what the build jobs say they
# are, and describe the version of the tag.
#   verify-assets.sh <dir>             <dir> holds the seven built files and nothing else. Their SHA-256 must equal the
#                                      EXPECT_SHA256_* values (the hashes the build jobs computed), and the rpm, the
#                                      deb and the Arch package carry the inner identities the file names claim (name
#                                      kuickshare, the version of the tag, release/revision, the machine). Writes
#                                      <dir>/SHA256SUMS.
#   verify-assets.sh --recheck <dir>   <dir> holds the eight release files: checks them against SHA256SUMS and the
#                                      structure again (the publish job runs this on what it downloaded).
# Environment:
#   RELEASE_TAG   the tag, vMAJOR.MINOR.PATCH
#   GITHUB_SHA    the commit of the run; HEAD must be exactly it
#   EXPECT_SHA256_APPIMAGE, EXPECT_SHA256_ZSYNC, EXPECT_SHA256_TARBALL, EXPECT_SHA256_FLATPAK,
#   EXPECT_SHA256_RPM, EXPECT_SHA256_DEB, EXPECT_SHA256_PKG                        (first mode only)
#   KUICKSHARE_ROOT  (optional) the checkout whose HEAD is compared, default the one this script is in
# Needs sh, tar, sha256sum, sha1sum, python3, zstd (the runner has it preinstalled; package-info.py uses it to
# decompress a zstd control.tar or .PKGINFO). Exit codes: 0 ok, 1 a check failed, 2 bad input.
set -eu
root=${KUICKSHARE_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
# package-info.py belongs to the tree this script runs from, not to KUICKSHARE_ROOT (a simulated or otherwise
# pinned checkout there only documents the commit; it has no build/packaging/).
packaging=$(cd "$(dirname "$0")/../packaging" && pwd)
fail() { echo "verify-assets: $*" >&2; exit 1; }

recheck=0
if [ "${1:-}" = --recheck ]; then
    recheck=1
    shift
fi
[ $# -eq 1 ] || { echo "usage: verify-assets.sh [--recheck] <dir>" >&2; exit 2; }
[ -d "$1" ] || { echo "verify-assets: $1 is not a directory" >&2; exit 2; }
dir=$(cd "$1" && pwd)
: "${RELEASE_TAG:?set RELEASE_TAG to the tag}"
: "${GITHUB_SHA:?set GITHUB_SHA to the commit}"
printf '%s\n' "$RELEASE_TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || { echo "verify-assets: the tag is not vMAJOR.MINOR.PATCH" >&2; exit 2; }
version=${RELEASE_TAG#v}
[ "$(git -C "$root" rev-parse HEAD)" = "$GITHUB_SHA" ] || fail "HEAD is not $GITHUB_SHA"

appimage=KuickShare-x86_64.AppImage
zsync=$appimage.zsync
tarball=KuickShare-x86_64.tar.gz
flatpak=KuickShare-x86_64.flatpak
rpm=KuickShare-x86_64.rpm
deb=KuickShare-x86_64.deb
pkg=KuickShare-x86_64.pkg.tar.zst
built_names() { printf '%s\n' "$appimage" "$flatpak" "$rpm" "$deb" "$pkg" "$tarball" "$zsync"; }

# The exact set: nothing missing, nothing extra, only regular files.
want=$(built_names | sort)
[ "$recheck" = 0 ] || want=$( { built_names; echo SHA256SUMS; } | sort)
have=$(find "$dir" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)
[ "$have" = "$want" ] || fail "wrong set of files in $dir. Expected: $(echo "$want" | tr '\n' ' ')Found: $(echo "$have" | tr '\n' ' ')"
for f in "$appimage" "$zsync" "$tarball" "$flatpak" "$rpm" "$deb" "$pkg"; do
    [ -f "$dir/$f" ] && [ ! -L "$dir/$f" ] || fail "$f is not a regular file"
    [ -s "$dir/$f" ] || fail "$f is empty"
done

if [ "$recheck" = 0 ]; then
    for file in "$appimage" "$zsync" "$tarball" "$flatpak" "$rpm" "$deb" "$pkg"; do
        case $file in
            "$appimage") var=EXPECT_SHA256_APPIMAGE expect=${EXPECT_SHA256_APPIMAGE:-} ;;
            "$zsync") var=EXPECT_SHA256_ZSYNC expect=${EXPECT_SHA256_ZSYNC:-} ;;
            "$tarball") var=EXPECT_SHA256_TARBALL expect=${EXPECT_SHA256_TARBALL:-} ;;
            "$flatpak") var=EXPECT_SHA256_FLATPAK expect=${EXPECT_SHA256_FLATPAK:-} ;;
            "$rpm") var=EXPECT_SHA256_RPM expect=${EXPECT_SHA256_RPM:-} ;;
            "$deb") var=EXPECT_SHA256_DEB expect=${EXPECT_SHA256_DEB:-} ;;
            *) var=EXPECT_SHA256_PKG expect=${EXPECT_SHA256_PKG:-} ;;
        esac
        [ -n "$expect" ] || { echo "verify-assets: $var is not set" >&2; exit 2; }
        actual=$(sha256sum "$dir/$file" | cut -d' ' -f1)
        [ "$actual" = "$expect" ] || fail "$file has SHA-256 $actual, the build job reported $expect"
    done
else
    (cd "$dir" && sha256sum --check --strict --quiet SHA256SUMS) || fail "SHA256SUMS does not match the files"
    [ "$(sed 's/^[0-9a-f]*  //' "$dir/SHA256SUMS" | sort)" = "$(built_names | sort)" ] || fail "SHA256SUMS does not list exactly the seven files"
fi

# Structure.
[ "$(head -c 4 "$dir/$appimage" | od -An -c | tr -d ' ')" = '177ELF' ] || fail "$appimage is not an ELF file"

top=KuickShare-$version-x86_64
list=$(mktemp)
trap 'rm -f "$list"' EXIT
tar -tzf "$dir/$tarball" > "$list" || fail "$tarball is not a readable gzip tar archive"
[ -s "$list" ] || fail "$tarball is empty"
if grep -Ev "^$top/" "$list" | grep -qvx "$top"; then fail "$tarball has entries outside $top/"; fi
if grep -Eq '(^|/)\.\.(/|$)' "$list"; then fail "$tarball has a path with .."; fi
grep -qx "$top/install.sh" "$list" || fail "$tarball has no $top/install.sh"
inner=$(tar -xzOf "$dir/$tarball" "$top/usr/share/kuickshare/VERSION") || fail "$tarball has no $top/usr/share/kuickshare/VERSION"
[ "$inner" = "$version" ] || fail "the VERSION file in $tarball says '$inner', the tag says '$version'"

# The zsync header must describe this AppImage: otherwise AppImageUpdate gets a wrong delta or refuses it.
sha1=$(sha1sum "$dir/$appimage" | cut -d' ' -f1)
size=$(wc -c < "$dir/$appimage" | tr -d ' ')
python3 -I - "$dir/$zsync" "$appimage" "$sha1" "$size" <<'PY' || fail "the .zsync file does not describe $appimage"
import sys

path, name, sha1, size = sys.argv[1:5]
with open(path, "rb") as f:
    data = f.read(65536)
head, sep, _ = data.partition(b"\n\n")
if not sep:
    sys.exit("zsync: no end of header")
fields = {}
for line in head.decode("ascii", "replace").split("\n"):
    key, _, value = line.partition(": ")
    fields[key] = value
if "zsync" not in fields:
    sys.exit("zsync: not a zsync header")
for key, want in (("SHA-1", sha1), ("Length", size)):
    if fields.get(key) != want:
        sys.exit("zsync: %s is %r, the AppImage has %r" % (key, fields.get(key), want))
if fields.get("Filename") != name:
    sys.exit("zsync: Filename is %r, not %r" % (fields.get("Filename"), name))
PY

# The inner identities: what the package itself says about kuickshare (build/packaging/package-info.py, python
# only: the verify job has no rpm and no dpkg tools). All three are built by our recipes in this release; their
# other fields are smoke tested in CI (build/packaging/test-*.sh).

# rpm: name and version are the tag's, Release is 1 or 1.<dist> (the %{?dist} the image's macros add), and it is
# built for this machine.
info=$("$packaging/package-info.py" rpm "$dir/$rpm") || fail "$rpm is not a readable rpm (package-info says why above)"
rpm_name=${info%% *}; info=${info#* }
rpm_version=${info%% *}; info=${info#* }
rpm_release=${info%% *}; info=${info#* }
rpm_arch=${info#*}
[ "$rpm_name" = kuickshare ] || fail "$rpm is named '$rpm_name' inside, not kuickshare"
[ "$rpm_version" = "$version" ] || fail "the version inside $rpm is '$rpm_version', the tag says '$version'"
printf '%s\n' "$rpm_release" | grep -Eq '^1(\.[a-z][a-z0-9]*)?$' || fail "the release inside $rpm is '$rpm_release', not 1 or 1.<dist>"
[ "$rpm_arch" = x86_64 ] || fail "the arch inside $rpm is '$rpm_arch', not x86_64"

# deb: the revision -1 is part of the version; the architecture is amd64, to Debian's name for this machine.
info=$("$packaging/package-info.py" deb "$dir/$deb") || fail "$deb is not a readable deb (package-info says why above)"
deb_name=${info%% *}; info=${info#* }
deb_version=${info%% *}; info=${info#* }
deb_arch=${info##* }
[ "$deb_name" = kuickshare ] || fail "$deb is named '$deb_name' inside, not kuickshare"
[ "$deb_version" = "$version-1" ] || fail "the version inside $deb is '$deb_version', the tag says '$version-1'"
[ "$deb_arch" = amd64 ] || fail "the arch inside $deb is '$deb_arch', not amd64"

# pkg: the package's own naming, pkgver with the -1 revision, and the arch of the machine.
info=$("$packaging/package-info.py" pkg "$dir/$pkg") || fail "$pkg is not a readable pkg (package-info says why above)"
pkg_name=${info%% *}; info=${info#* }
pkg_version=${info%% *}; info=${info#* }
pkg_arch=${info##* }
[ "$pkg_name" = kuickshare ] || fail "$pkg is named '$pkg_name' inside, not kuickshare"
[ "$pkg_version" = "$version-1" ] || fail "the version inside $pkg is '$pkg_version', the tag says '$version-1'"
[ "$pkg_arch" = x86_64 ] || fail "the arch inside $pkg is '$pkg_arch', not x86_64"

if [ "$recheck" = 0 ]; then
    (cd "$dir" && sha256sum "$appimage" "$zsync" "$rpm" "$deb" "$pkg" "$tarball" "$flatpak" > SHA256SUMS)
    echo "verify-assets: $RELEASE_TAG ($GITHUB_SHA): seven files verified, SHA256SUMS written"
    (cd "$dir" && cat SHA256SUMS)
else
    echo "verify-assets: $RELEASE_TAG: eight files verified against SHA256SUMS"
fi
