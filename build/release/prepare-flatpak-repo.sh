#!/bin/sh
# Release gate 3a, keyless. Puts the unsigned commits the build job exported (export-flatpak-commits.sh) on top of a fresh
# clone of the published repository, finishes the repository without signing it (build/flatpak/publish-repo.sh: summary,
# appstream data, .flatpakrepo), and lists what the signing job must sign. This is where the data of the build job is
# parsed (archive, ostree objects, appstream data), so the signing key is never present. Nothing is pushed.
#   prepare-flatpak-repo.sh <flatpak-commits.tar> <out-dir>      writes <out-dir>/prepared.tar and <out-dir>/sign-list.txt
# prepared.tar holds site/, the site as it would be published but without .git and repo/tmp, the commits unsigned.
# sign-list.txt has one `ref <TAB> commit` line for every ref that differs from the published repository: the refs of the
# build (those of refs.txt, same commits) and the appstream refs that build-update-repo made.
# Environment:
#   EXPECT_COMMITS_SHA256    SHA-256 of the tar, as the build job reported it
#   EXPECT_REFS_SHA256       SHA-256 of the refs.txt inside it, as the build job reported it
#   SITE_URL                 where Pages serves the site (goes into the .flatpakrepo)
#   GITHUB_SHA               the commit of the run; HEAD of the checkout must be exactly it
#   GITHUB_REPOSITORY        owner/name, required, to list the releases; also finds gh-pages unless PAGES_REMOTE (the git URL) is set
#   GH_TOKEN                 for gh, which lists the releases (a read-only token is enough)
#   RUNNER_TEMP              where the scratch directory goes (default $TMPDIR, then /tmp)
#   SIGNING_PUBKEY           (optional) the public key of the repository, default build/flatpak/signing-key.asc
#   KUICKSHARE_ROOT          (optional) the checkout whose HEAD is compared
# Refuses a gh-pages whose signed app head is older than the newest published release, and no gh-pages while one is published
# (see the comments where they are checked). GH_TOKEN is read at the start and given to gh only.
# Refuses to run when FLATPAK_GPG_PRIVATE_KEY is set. The GPGKey of the .flatpakrepo is SIGNING_PUBKEY, dearmored; a published
# one with other bytes is refused (installed clients would stop updating, and finish requires these bytes).
# Writes prepared_sha256=, signlist_sha256= and base_sha= to $GITHUB_OUTPUT when it is set.
# Needs flatpak, ostree, gpg 2.2+, git, tar, base64, gh (GH_TOKEN, to list the releases) and python3. Exit codes: 0 ok, 1 a check failed, 2 bad input or the secret is set,
# 3 gh-pages is not where the build left it ("stale base").
# shellcheck disable=SC2154 # $base and $tab come from flatpak-lib.sh
set -eu
tools=$(cd "$(dirname "$0")/../.." && pwd)
root=${KUICKSHARE_ROOT:-$tools}
pubkey=${SIGNING_PUBKEY:-$tools/build/flatpak/signing-key.asc}
me=prepare-flatpak-repo
scratch=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
tmp=$(mktemp -d "$scratch/prepare.XXXXXX")
cleanup() {
    set +e
    gpgconf --homedir "$tmp/gnupg" --kill all 2> /dev/null
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
# shellcheck source=/dev/null
. "$tools/build/release/flatpak-lib.sh"

no_secret
# The token is for gh alone: flatpak, ostree and git never see it.
gh_token=${GH_TOKEN:-}
unset GH_TOKEN
[ $# -eq 2 ] || bad_input "usage: prepare-flatpak-repo.sh <flatpak-commits.tar> <out-dir>"
: "${GITHUB_SHA:?}" "${EXPECT_COMMITS_SHA256:?}" "${EXPECT_REFS_SHA256:?}" "${SITE_URL:?}"
remote=${PAGES_REMOTE:-https://github.com/${GITHUB_REPOSITORY:?}.git}
[ "$(git -C "$root" rev-parse HEAD)" = "$GITHUB_SHA" ] || fail "HEAD is not $GITHUB_SHA"
[ -f "$1" ] || bad_input "$1 does not exist"
tarfile=$(readlink -f "$1")
mkdir -p "$2"
out=$(cd "$2" && pwd)
mkdir -m 700 "$tmp/gnupg"
GNUPGHOME=$tmp/gnupg
export GNUPGHOME

# ---- the commits ----
check_sha256 "$tarfile" "$EXPECT_COMMITS_SHA256" "the commits archive"
tar_safe "$tarfile" repo refs.txt
mkdir "$tmp/commits"
tar -xf "$tarfile" -C "$tmp/commits" --no-same-owner
refs=$tmp/commits/refs.txt
[ -f "$refs" ] || bad_input "the commits archive has no refs.txt"
check_sha256 "$refs" "$EXPECT_REFS_SHA256" "the refs.txt of the commits archive"
read_refs "$refs"
# shellcheck disable=SC2046 # refs have no spaces (read_refs)
set -- $(cat "$tmp/ref.names")

# ---- the public key ----
want_fpr=$(key_fpr "$pubkey")
printf '%s\n' "$want_fpr" | grep -Eq '^[0-9A-F]{40}$' || bad_input "cannot read the fingerprint of $pubkey"

# Releases that are published: the anchor outside gh-pages for the check against a rolled-back gh-pages below. The newest
# non-draft vX.Y.Z release that has the Flatpak bundle (a release made by hand has no files and was never on Pages).
GH_TOKEN=$gh_token gh api --paginate "repos/${GITHUB_REPOSITORY:?}/releases?per_page=100" > "$tmp/releases.json" || bad_input "cannot list the releases of $GITHUB_REPOSITORY (gh needs GH_TOKEN)"
published=$(python3 -I - "$tmp/releases.json" << 'PY'
import json
import re
import sys

text = open(sys.argv[1]).read()
decoder = json.JSONDecoder()
found = []
pos = 0
while True:
    while pos < len(text) and text[pos].isspace():
        pos += 1
    if pos >= len(text):
        break
    page, pos = decoder.raw_decode(text, pos)
    for release in page if isinstance(page, list) else [page]:
        tag = release.get("tag_name", "")
        has_bundle = any(a.get("name") == "KuickShare-x86_64.flatpak" and a.get("state", "uploaded") == "uploaded" for a in release.get("assets", []))
        if re.fullmatch(r"v\d+\.\d+\.\d+", tag) and not release.get("draft") and not release.get("prerelease") and has_bundle:
            found.append(tag)
print(max(found, key=lambda t: tuple(int(n) for n in t[1:].split("."))) if found else "")
PY
) || bad_input "cannot read the list of releases"

# ---- the published repository ----
heads=$(git ls-remote --heads "$remote" gh-pages | cut -f1)
site=$tmp/site
if [ "$base" = - ]; then
    [ -z "$heads" ] || stale "the build saw no gh-pages, but it exists now ($heads)"
    [ -z "$published" ] || fail "there is no gh-pages, but $published is published: gh-pages was deleted. Restore it first (CONTRIBUTING.md, Releases, When a run fails)"
    mkdir "$site"
else
    [ "$heads" = "$base" ] || stale "gh-pages is $heads, the build started from $base"
    git clone --quiet --depth 1 --branch gh-pages "$remote" "$site"
    [ "$(git -C "$site" rev-parse HEAD)" = "$base" ] || stale "the clone of gh-pages is not at $base"
fi
repo=$site/repo
if [ ! -d "$repo" ]; then ostree --repo="$repo" init --mode=archive-z2; fi
# git keeps no empty directory, and ostree (Ubuntu's at least) fails without these.
mkdir -p "$repo/refs/heads" "$repo/refs/mirrors" "$repo/refs/remotes" "$repo/tmp"

# The key every installed client trusts must not change by accident: $pubkey dearmored, which must be the published bytes.
GPG_KEY_B64=$(gpg --batch --dearmor < "$pubkey" | base64 -w0)
old_gpgkey=
if [ -f "$site/kuickshare.flatpakrepo" ]; then old_gpgkey=$(sed -n 's/^GPGKey=//p' "$site/kuickshare.flatpakrepo"); fi
if [ -n "$old_gpgkey" ]; then
    [ "$old_gpgkey" = "$GPG_KEY_B64" ] \
        || bad_input "the GPGKey of the published .flatpakrepo is not $(basename "$pubkey") ($want_fpr) byte for byte: installed clients would stop updating"
    echo "$me: the GPGKey is the published one, the key $want_fpr"
else
    echo "$me: there was no published .flatpakrepo: the GPGKey is made from $(basename "$pubkey")"
fi
export GPG_KEY_B64

# What this build is put on must be what was signed: the published summary and the head commits of the published refs
# verify with the pinned key (a pull with signature checks from the clone), so a gh-pages that was altered is not re-signed.
if [ "$base" != - ]; then
    gpg --batch --dearmor < "$pubkey" > "$tmp/pinned.gpg"
    published_refs=$(ostree --repo="$repo" refs | LC_ALL=C sort)
    [ -n "$published_refs" ] || bad_input "gh-pages has a repository without a ref"
    ostree --repo="$tmp/verify" init --mode=archive
    ostree --repo="$tmp/verify" remote add --set=gpg-verify=true --set=gpg-verify-summary=true --gpg-import="$tmp/pinned.gpg" published "file://$repo"
    # shellcheck disable=SC2086 # refs have no spaces
    ostree --repo="$tmp/verify" pull --untrusted published $published_refs > "$tmp/verify.log" 2>&1 || {
        sed 's/^/    /' "$tmp/verify.log" >&2
        bad_input "the published repository does not verify with the pinned key $want_fpr: gh-pages was altered, or is not signed by that key"
    }
    # The pull takes the commit of a ref from the verified summary, not from refs/heads of the clone: an attacker who writes
    # gh-pages can move refs/heads/X and keep the old, signed summary (tried with ostree 2024.5). So the refs of the clone, the
    # refs and commits of the verified summary and what was pulled must be the same.
    ostree --repo="$tmp/verify" remote summary published 2> /dev/null | awk '/^\* / { r = $2 } /Latest Commit/ { getline; print r, $1 }' | LC_ALL=C sort > "$tmp/summary.refs"
    ref_map "$repo" > "$tmp/heads.refs"
    [ -s "$tmp/summary.refs" ] && cmp -s "$tmp/summary.refs" "$tmp/heads.refs" \
        || bad_input "the refs of gh-pages are not the refs and commits of its signed summary: gh-pages was altered"
    while read -r ref commit; do
        [ "$(ostree --repo="$tmp/verify" rev-parse "published:$ref" 2> /dev/null || ostree --repo="$tmp/verify" rev-parse "$ref" 2> /dev/null)" = "$commit" ] \
            || bad_input "what was pulled for $ref is not the commit $commit of the signed summary"
    done < "$tmp/summary.refs"
    echo "$me: the published summary and heads verify with the pinned key"
    # gh-pages must not be older than the last published release: someone who can push only to gh-pages could bring back an
    # older, validly signed state, and a new version would be built on it. The version is read from the signed app head (the
    # first <release> of its metainfo, in the repository that was just pulled and verified), not from anything gh-pages says.
    if [ -n "$published" ]; then
        app_ref=app/io.github.the_beper.KuickShare/x86_64/stable
        metainfo=/files/share/metainfo/io.github.the_beper.KuickShare.metainfo.xml
        app_version=$({ ostree --repo="$tmp/verify" cat "published:$app_ref" "$metainfo" 2> /dev/null || ostree --repo="$tmp/verify" cat "$app_ref" "$metainfo" 2> /dev/null; } \
            | awk '/<release / && !d { v = $0; sub(/.*version="/, "", v); sub(/".*/, "", v); print v; d = 1 }') || app_version=
        printf '%s\n' "$app_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || fail "cannot read the version of the signed app head of gh-pages (metainfo, first <release>), and $published is published"
        [ "$(printf '%s\n%s\n' "v$app_version" "$published" | sort -V | tail -n 1)" = "v$app_version" ] \
            || fail "the app of gh-pages is at $app_version, but $published is published: gh-pages was rolled back or never updated. Restore it first (CONTRIBUTING.md, Releases, When a run fails)"
        echo "$me: the signed app of gh-pages is at $app_version, the newest published release is $published"
    fi
fi

# Each ref moves from its published commit to the new one: no commit is skipped or replaced.
while IFS=$tab read -r ref commit parent; do
    [ "$ref" != base ] || continue
    if current=$(ostree --repo="$repo" rev-parse "$ref" 2> /dev/null); then :; else current=-; fi
    [ "$current" = "$parent" ] || stale "$ref is at $current in the published repository, the new commit $commit has parent $parent"
done < "$refs"
ref_map "$repo" > "$tmp/refs.before"
# --untrusted: the objects come from a build job, so ostree checks every object against its checksum.
ostree --repo="$repo" pull-local --untrusted "$tmp/commits/repo" "$@"
for ref in "$@"; do
    want=$(awk -F'\t' -v r="$ref" '$1 == r { print $2 }' "$refs")
    [ "$(ostree --repo="$repo" rev-parse "$ref")" = "$want" ] || fail "$ref is not at the exported commit after the pull"
done

# ---- finish it, unsigned: summary, appstream data (new commits, unsigned), the files users add the repository with ----
"$tools/build/flatpak/publish-repo.sh" "$site" "$SITE_URL"

# ---- what the signing job signs: the refs that are not what gh-pages has, and nothing unexpected ----
: > "$tmp/sign-list.txt"
for ref in $(ostree --repo="$repo" refs | LC_ALL=C sort); do
    new=$(ostree --repo="$repo" rev-parse "$ref")
    old=$(awk -v r="$ref" '$1 == r { print $2 }' "$tmp/refs.before")
    [ "$new" != "$old" ] || continue
    case $ref in
        appstream/x86_64 | appstream2/x86_64) ;;
        *)
            want=$(awk -F'\t' -v r="$ref" '$1 == r { print $2 }' "$refs")
            [ "$want" = "$new" ] || fail "$ref changed, and it is neither a ref of the build nor an appstream ref"
            ;;
    esac
    printf '%s\t%s\n' "$ref" "$new" >> "$tmp/sign-list.txt"
done
for ref in "$@"; do
    awk -F'\t' -v r="$ref" '$1 == r { f = 1 } END { exit !f }' "$tmp/sign-list.txt" || fail "$ref is not on the list to sign"
done

# ---- the prepared site and the list, as artifacts ----
tar --sort=name --owner=0 --group=0 --numeric-owner --exclude=site/.git --exclude=site/repo/tmp -C "$tmp" -cf "$out/prepared.tar" site
cp "$tmp/sign-list.txt" "$out/sign-list.txt"
prepared_sha=$(sha256sum "$out/prepared.tar" | cut -d' ' -f1)
signlist_sha=$(sha256sum "$out/sign-list.txt" | cut -d' ' -f1)
echo "$me: prepared.tar $prepared_sha, built on gh-pages $base; to sign:"
cat "$out/sign-list.txt"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        echo "prepared_sha256=$prepared_sha"
        echo "signlist_sha256=$signlist_sha"
        echo "base_sha=$base"
    } >> "$GITHUB_OUTPUT"
fi
