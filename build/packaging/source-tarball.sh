#!/bin/sh
# The source tarball the three package recipes build from:
#     dist/tmp/packaging/kuickshare-$version.tar.gz
# Written from HEAD with `git archive` (the recipe builds run from a clean checkout: a dirty tree would
# make a tarball a CI checkout never sees) and gzip -n, so the same commit gives the same bytes and the
# SHA-256 the rendering puts into the PKGBUILD holds. It is not a release asset in phases 1-4; phases
# 5-7 add the release one (see dist/packaging-design.md).
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)
[ -n "$version" ] || { echo "source-tarball: no version in src/shell/CMakeLists.txt" >&2; exit 1; }

mkdir -p dist/tmp/packaging
out=dist/tmp/packaging/kuickshare-$version.tar.gz
# The orphan from another version must not survive a version bump.
rm -f dist/tmp/packaging/kuickshare-*.tar.gz
# Fixed tar entry times (no --mtime of the commit date): the same HEAD always gives the same bytes.
git archive --format=tar --prefix=kuickshare-"$version"/ --mtime='2026-01-01 00:00Z' HEAD | gzip -n > "$out"
echo "$out"
