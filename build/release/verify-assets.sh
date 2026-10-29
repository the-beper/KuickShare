#!/bin/sh
# Release gate 2: the files the build jobs produced are exactly the expected ones, are what the build jobs say they
# are, and describe the version of the tag.
#   verify-assets.sh <dir>             <dir> holds the four built files and nothing else. Their SHA-256 must equal the
#                                      EXPECT_SHA256_* values (the hashes the build jobs computed). Writes <dir>/SHA256SUMS.
#   verify-assets.sh --recheck <dir>   <dir> holds the five release files: checks them against SHA256SUMS and the structure
#                                      again (the publish job runs this on what it downloaded).
# Environment:
#   RELEASE_TAG   the tag, vMAJOR.MINOR.PATCH
#   GITHUB_SHA    the commit of the run; HEAD must be exactly it
#   EXPECT_SHA256_APPIMAGE, EXPECT_SHA256_ZSYNC, EXPECT_SHA256_TARBALL, EXPECT_SHA256_FLATPAK   (first mode only)
#   KUICKSHARE_ROOT  (optional) the checkout whose HEAD is compared, default the one this script is in
# Needs sh, tar, sha256sum, sha1sum, python3. Exit codes: 0 ok, 1 a check failed, 2 bad input.
set -eu
root=${KUICKSHARE_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
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
built_names() { printf '%s\n' "$appimage" "$flatpak" "$tarball" "$zsync"; }

# The exact set: nothing missing, nothing extra, only regular files.
want=$(built_names | sort)
[ "$recheck" = 0 ] || want=$( { built_names; echo SHA256SUMS; } | sort)
have=$(find "$dir" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)
[ "$have" = "$want" ] || fail "wrong set of files in $dir. Expected: $(echo "$want" | tr '\n' ' ')Found: $(echo "$have" | tr '\n' ' ')"
for f in "$appimage" "$zsync" "$tarball" "$flatpak"; do
    [ -f "$dir/$f" ] && [ ! -L "$dir/$f" ] || fail "$f is not a regular file"
    [ -s "$dir/$f" ] || fail "$f is empty"
done

if [ "$recheck" = 0 ]; then
    for file in "$appimage" "$zsync" "$tarball" "$flatpak"; do
        case $file in
            "$appimage") var=EXPECT_SHA256_APPIMAGE expect=${EXPECT_SHA256_APPIMAGE:-} ;;
            "$zsync") var=EXPECT_SHA256_ZSYNC expect=${EXPECT_SHA256_ZSYNC:-} ;;
            "$tarball") var=EXPECT_SHA256_TARBALL expect=${EXPECT_SHA256_TARBALL:-} ;;
            *) var=EXPECT_SHA256_FLATPAK expect=${EXPECT_SHA256_FLATPAK:-} ;;
        esac
        [ -n "$expect" ] || { echo "verify-assets: $var is not set" >&2; exit 2; }
        actual=$(sha256sum "$dir/$file" | cut -d' ' -f1)
        [ "$actual" = "$expect" ] || fail "$file has SHA-256 $actual, the build job reported $expect"
    done
else
    (cd "$dir" && sha256sum --check --strict --quiet SHA256SUMS) || fail "SHA256SUMS does not match the files"
    [ "$(sed 's/^[0-9a-f]*  //' "$dir/SHA256SUMS" | sort)" = "$(built_names | sort)" ] || fail "SHA256SUMS does not list exactly the four files"
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

if [ "$recheck" = 0 ]; then
    (cd "$dir" && sha256sum "$appimage" "$zsync" "$tarball" "$flatpak" > SHA256SUMS)
    echo "verify-assets: $RELEASE_TAG ($GITHUB_SHA): four files verified, SHA256SUMS written"
    (cd "$dir" && cat SHA256SUMS)
else
    echo "verify-assets: $RELEASE_TAG: five files verified against SHA256SUMS"
fi
