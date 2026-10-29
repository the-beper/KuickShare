#!/bin/sh
# Release gate 3, the only place that touches the Flatpak signing key. Takes the unsigned commits the build job exported
# (export-flatpak-commits.sh), puts them on top of a fresh clone of the published repository, signs them, finishes the
# repository (build/flatpak/publish-repo.sh: summary, deltas, .flatpakrepo), checks it the way a client would, and writes the
# new site as a tar archive. Nothing is pushed: publish.sh does that.
#   sign-flatpak-repo.sh <flatpak-commits.tar> <out-dir>      writes <out-dir>/site.tar and <out-dir>/refs.txt
# Environment:
#   FLATPAK_GPG_PRIVATE_KEY  the armored secret key, without a passphrase (an environment secret, set on this step only)
#   EXPECT_COMMITS_SHA256    SHA-256 of the tar, as the build job reported it
#   SITE_URL                 where Pages serves the site (goes into the .flatpakrepo)
#   GITHUB_SHA               the commit of the run; HEAD of the checkout must be exactly it
#   GITHUB_REPOSITORY        owner/name, to find gh-pages (or PAGES_REMOTE: the git URL to clone it from)
#   RUNNER_TEMP              where the temporary keyring goes (default $TMPDIR, then /tmp)
#   SIGNING_PUBKEY           (optional) the public key the secret must match, default build/flatpak/signing-key.asc
#   KUICKSHARE_ROOT          (optional) the checkout whose HEAD is compared
# The key lives in a keyring made for this run, which is deleted, with its agent stopped, however the script ends.
# Writes site_sha256= and base_sha= to $GITHUB_OUTPUT when it is set.
# Needs flatpak, ostree, gpg 2.2+, git, tar, base64. Exit codes: 0 ok, 1 a check failed, 2 key or input problem,
# 3 gh-pages is not where the build left it ("stale base").
set -eu
tools=$(cd "$(dirname "$0")/../.." && pwd)
root=${KUICKSHARE_ROOT:-$tools}
pubkey=${SIGNING_PUBKEY:-$tools/build/flatpak/signing-key.asc}
fail() { echo "sign-flatpak-repo: $*" >&2; exit 1; }
bad_input() { echo "sign-flatpak-repo: $*" >&2; exit 2; }
stale() { echo "sign-flatpak-repo: stale base: $*" >&2; exit 3; }
[ $# -eq 2 ] || bad_input "usage: sign-flatpak-repo.sh <flatpak-commits.tar> <out-dir>"
: "${GITHUB_SHA:?}" "${EXPECT_COMMITS_SHA256:?}" "${SITE_URL:?}"
remote=${PAGES_REMOTE:-https://github.com/${GITHUB_REPOSITORY:?}.git}
[ -n "${FLATPAK_GPG_PRIVATE_KEY:-}" ] || bad_input "the secret FLATPAK_GPG_PRIVATE_KEY is empty or not available to this job (see CONTRIBUTING.md, Releases)"
[ "$(git -C "$root" rev-parse HEAD)" = "$GITHUB_SHA" ] || fail "HEAD is not $GITHUB_SHA"
[ -f "$1" ] || bad_input "$1 does not exist"
tarfile=$(readlink -f "$1")
mkdir -p "$2"
out=$(cd "$2" && pwd)
tab=$(printf '\t')

scratch=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
tmp=$(mktemp -d "$scratch/sign.XXXXXX")
gnupg=$(mktemp -d "$scratch/gnupg.XXXXXX")
cleanup() {
    set +e
    gpgconf --homedir "$gnupg" --kill gpg-agent 2> /dev/null
    rm -rf "$gnupg" "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
GNUPGHOME=$gnupg
export GNUPGHOME

# ---- the commits ----
[ "$(sha256sum "$tarfile" | cut -d' ' -f1)" = "$EXPECT_COMMITS_SHA256" ] || fail "the commits archive is not the one the build job produced"
tar -tf "$tarfile" > "$tmp/commits.list"
if grep -Evq '^(repo(/|$)|refs\.txt$)' "$tmp/commits.list" || grep -Eq '(^/|(^|/)\.\.(/|$))' "$tmp/commits.list"; then
    bad_input "the commits archive has files other than repo/ and refs.txt"
fi
# Only plain files and directories: a symlink or hard link in a build job's artifact could point anywhere once extracted.
if tar -tvf "$tarfile" | cut -c1 | grep -qv '[-d]'; then
    bad_input "the commits archive has a member that is not a plain file or a directory (link, device)"
fi
mkdir "$tmp/commits"
tar -xf "$tarfile" -C "$tmp/commits" --no-same-owner
refs=$tmp/commits/refs.txt
[ -f "$refs" ] || bad_input "the commits archive has no refs.txt"
base=$(awk -F'\t' '$1 == "base" { print $2 }' "$refs")
printf '%s\n' "$base" | grep -Eq '^(-|[0-9a-f]{40})$' || bad_input "refs.txt has no valid base"
set --
while IFS=$tab read -r ref commit parent; do
    [ "$ref" != base ] || continue
    printf '%s\n' "$ref" | grep -Eq '^(app|runtime)/[A-Za-z0-9._-]+/[A-Za-z0-9_-]+/[A-Za-z0-9._-]+$' || bad_input "refs.txt: bad ref '$ref'"
    printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{64}$' || bad_input "refs.txt: bad commit for $ref"
    printf '%s\n' "$parent" | grep -Eq '^(-|[0-9a-f]{64})$' || bad_input "refs.txt: bad parent for $ref"
    set -- "$@" "$ref"
done < "$refs"
[ $# -gt 0 ] || bad_input "refs.txt lists no ref"

# ---- the key ----
want_fpr=$(gpg --batch --show-keys --with-colons "$pubkey" | awk -F: '$1 == "pub" { p = 1 } p && $1 == "fpr" { print $10; exit }')
printf '%s\n' "$want_fpr" | grep -Eq '^[0-9A-F]{40}$' || bad_input "cannot read the fingerprint of $pubkey"
if printf '%s\n' "$FLATPAK_GPG_PRIVATE_KEY" | gpg --batch --quiet --import 2> "$tmp/import.log"; then :; else
    sed 's/^/gpg: /' "$tmp/import.log" >&2
    bad_input "the secret FLATPAK_GPG_PRIVATE_KEY cannot be imported"
fi
unset FLATPAK_GPG_PRIVATE_KEY
secret_count=$(gpg --batch --list-secret-keys --with-colons | awk -F: '$1 == "sec" { n++ } END { print n + 0 }')
[ "$secret_count" -eq 1 ] || bad_input "the secret holds $secret_count keys, expected exactly one"
fpr=$(gpg --batch --list-secret-keys --with-colons | awk -F: '$1 == "fpr" { print $10; exit }')
[ "$fpr" = "$want_fpr" ] || bad_input "the secret key is $fpr, but $pubkey is $want_fpr: refusing to sign (that would make every installed client reject the repository)"
echo probe > "$tmp/probe"
gpg --batch --yes --quiet --pinentry-mode error --local-user "$fpr" --detach-sign -o "$tmp/probe.sig" "$tmp/probe" \
    || bad_input "the key cannot sign without a passphrase"
echo "sign-flatpak-repo: signing key $fpr matches $(basename "$pubkey")"

# ---- the published repository ----
heads=$(git ls-remote --heads "$remote" gh-pages | cut -f1)
pages=$tmp/pages
if [ "$base" = - ]; then
    [ -z "$heads" ] || stale "the build saw no gh-pages, but it exists now ($heads)"
    mkdir "$pages"
else
    [ "$heads" = "$base" ] || stale "gh-pages is $heads, the build started from $base"
    git clone --quiet --depth 1 --branch gh-pages "$remote" "$pages"
    [ "$(git -C "$pages" rev-parse HEAD)" = "$base" ] || stale "the clone of gh-pages is not at $base"
fi
repo=$pages/repo
if [ ! -d "$repo" ]; then ostree --repo="$repo" init --mode=archive-z2; fi
old_gpgkey=
if [ -f "$pages/kuickshare.flatpakrepo" ]; then old_gpgkey=$(sed -n 's/^GPGKey=//p' "$pages/kuickshare.flatpakrepo"); fi

# Each ref moves from its published commit to the new one: no commit is skipped or replaced.
while IFS=$tab read -r ref commit parent; do
    [ "$ref" != base ] || continue
    if current=$(ostree --repo="$repo" rev-parse "$ref" 2> /dev/null); then :; else current=-; fi
    [ "$current" = "$parent" ] || stale "$ref is at $current in the published repository, the new commit $commit has parent $parent"
done < "$refs"
# --untrusted: the objects come from a build job, so ostree checks every object against its checksum.
ostree --repo="$repo" pull-local --untrusted "$tmp/commits/repo" "$@"
for ref in "$@"; do
    want=$(awk -F'\t' -v r="$ref" '$1 == r { print $2 }' "$refs")
    [ "$(ostree --repo="$repo" rev-parse "$ref")" = "$want" ] || fail "$ref is not at the exported commit after the pull"
done

# ---- sign, then finish: the deltas and the summary carry the commit signatures, so every ref is signed first ----
for ref in "$@"; do
    kind=${ref%%/*}
    rest=${ref#*/}
    id=${rest%%/*}
    rest=${rest#*/}
    arch=${rest%%/*}
    branch=${rest#*/}
    if [ "$kind" = runtime ]; then
        flatpak build-sign --gpg-sign="$fpr" --gpg-homedir="$gnupg" --runtime --arch="$arch" "$repo" "$id" "$branch"
    else
        flatpak build-sign --gpg-sign="$fpr" --gpg-homedir="$gnupg" --arch="$arch" "$repo" "$id" "$branch"
    fi
done
GPG_KEY_ID=$fpr "$tools/build/flatpak/publish-repo.sh" "$pages" "$SITE_URL"

# ---- check it as a client would: only the public key, a keyring of its own ----
gpg --batch --export "$fpr" > "$tmp/public.gpg"
FLATPAK_USER_DIR=$tmp/client
export FLATPAK_USER_DIR
mkdir "$FLATPAK_USER_DIR"
flatpak remote-add --user --gpg-import="$tmp/public.gpg" verify "file://$repo"
flatpak remote-ls --user verify > /dev/null
for ref in "$@"; do
    flatpak install --user --noninteractive -y --no-deps --no-related --no-deploy verify "$ref"
done
echo "sign-flatpak-repo: a client with only the public key accepts the summary and the new commits"

# The key every installed client trusts must not change by accident.
new_gpgkey=$(sed -n 's/^GPGKey=//p' "$pages/kuickshare.flatpakrepo")
[ -n "$new_gpgkey" ] || fail "the new .flatpakrepo has no GPGKey"
[ "$(printf '%s' "$new_gpgkey" | base64 -d | gpg --batch --show-keys --with-colons | awk -F: '$1 == "fpr" { print $10; exit }')" = "$fpr" ] \
    || fail "the GPGKey of the new .flatpakrepo is not the signing key"
if [ -z "$old_gpgkey" ]; then
    echo "sign-flatpak-repo: there was no published .flatpakrepo, so no GPGKey to compare"
elif [ "$new_gpgkey" = "$old_gpgkey" ]; then
    echo "sign-flatpak-repo: the GPGKey is byte-identical to the published one"
else
    bad_input "the GPGKey of the new .flatpakrepo differs from the published one: installed clients would stop updating"
fi

# ---- the site, without git metadata and scratch files ----
tar --sort=name --owner=0 --group=0 --numeric-owner --exclude=./.git --exclude=./repo/tmp -C "$pages" -cf "$out/site.tar" .
cp "$refs" "$out/refs.txt"
site_sha=$(sha256sum "$out/site.tar" | cut -d' ' -f1)
echo "sign-flatpak-repo: site.tar $site_sha, built on gh-pages $base"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "site_sha256=$site_sha" >> "$GITHUB_OUTPUT"
    echo "base_sha=$base" >> "$GITHUB_OUTPUT"
fi
