#!/bin/sh
# Release gate 1: the tag names the version the sources have, and the commit is the one the tag points to.
# Everything comes from the environment (the workflow passes event data through env:, never into the script text):
#   RELEASE_TAG   the pushed tag (github.ref_name), must be vMAJOR.MINOR.PATCH
#   GITHUB_SHA    the commit the run is for; HEAD must be exactly it
#   KUICKSHARE_MAIN_REF  (optional) the ref the commit must be an ancestor of, default refs/remotes/origin/main
#   KUICKSHARE_ROOT      (optional) the checkout to check, default the one this script is in
# Anti-downgrade: RELEASE_TAG must also be the newest vMAJOR.MINOR.PATCH tag of the clone (version order, git tag --sort=-v:refname).
# Other v* tags (a prerelease such as v1.6.0-rc.1 can sort above v1.6.0 in that order) are not releases here and do not count.
# Needs all tags and origin/main in the clone (actions/checkout with fetch-depth: 0 and fetch-tags: true).
# Writes "version=X.Y.Z" to $GITHUB_OUTPUT when it is set. Exit codes: 0 ok, 1 a check failed, 2 bad input.
set -eu
root=${KUICKSHARE_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
main_ref=${KUICKSHARE_MAIN_REF:-refs/remotes/origin/main}
: "${RELEASE_TAG:?set RELEASE_TAG to the tag}"
: "${GITHUB_SHA:?set GITHUB_SHA to the commit}"
fail() { echo "check-version: $*" >&2; exit 1; }

printf '%s\n' "$RELEASE_TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || { echo "check-version: the tag is not vMAJOR.MINOR.PATCH" >&2; exit 2; }
printf '%s\n' "$GITHUB_SHA" | grep -Eq '^[0-9a-f]{40}$' || { echo "check-version: GITHUB_SHA is not a full commit id" >&2; exit 2; }
version=${RELEASE_TAG#v}

cargo=$(sed -n 's/^version = "\(.*\)"/\1/p;/^\[dependencies\]/q' "$root/Cargo.toml" | head -1)
cmake=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' "$root/src/shell/CMakeLists.txt")
metainfo=$(sed -n 's/^ *<release version="\([^"]*\)".*/\1/p' "$root"/data/io.github.the_beper.KuickShare.metainfo.xml | head -1)
[ "$cargo" = "$version" ] || fail "Cargo.toml says '$cargo', the tag says '$version'"
[ "$cmake" = "$version" ] || fail "src/shell/CMakeLists.txt says '$cmake', the tag says '$version'"
[ "$metainfo" = "$version" ] || fail "the first <release> of the metainfo is '$metainfo', the tag says '$version'"

head=$(git -C "$root" rev-parse HEAD)
[ "$head" = "$GITHUB_SHA" ] || fail "HEAD is $head, the run is for $GITHUB_SHA"
tagged=$(git -C "$root" rev-parse --verify --quiet "refs/tags/$RELEASE_TAG^{commit}") || fail "the tag $RELEASE_TAG is not in the clone"
[ "$tagged" = "$GITHUB_SHA" ] || fail "the tag $RELEASE_TAG points to $tagged, the run is for $GITHUB_SHA"
git -C "$root" merge-base --is-ancestor "$GITHUB_SHA" "$main_ref" || fail "$GITHUB_SHA is not on main: push the release commit to main first"
newest=$(git -C "$root" tag -l 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1)
[ "$newest" = "$RELEASE_TAG" ] || fail "the newest release tag is $newest, not $RELEASE_TAG: a release is never made below an existing one (cut the next version instead)"

echo "check-version: $RELEASE_TAG is version $version, commit $GITHUB_SHA, on main"
if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "version=$version" >> "$GITHUB_OUTPUT"; fi
