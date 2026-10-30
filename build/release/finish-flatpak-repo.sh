#!/bin/sh
# Release gate 3c, keyless, on a machine of its own. Trusts nothing the signing job returned: it checks, with the public key
# only, that the signed repository is the prepared one plus signatures, and then writes the site as a tar archive for
# publish.sh. Nothing is pushed.
#   finish-flatpak-repo.sh <prepared.tar> <sign-list.txt> <signed-repo.tar> <refs.txt> <out-dir>      writes <out-dir>/site.tar
# What it checks:
#   - the four files against the hashes the earlier jobs reported; plain files and directories only; the sign list against
#     the refs.txt of the build;
#   - the signed repository has the same refs as the prepared one, with the same commits (so exactly the commits of the list
#     were signed, and no ref or commit was added, moved or dropped), and the sign list is on it;
#   - against gh-pages itself (a read-only clone at the base the build started from, which the prepare job is not trusted
#     for): every ref that is not on the sign list is as published, none was dropped, none is new, and every listed ref
#     continues its published commit; every listed commit says it belongs to its own ref (ostree.ref-binding);
#   - the files of the site outside repo/ are exactly what build/flatpak/write-site-files.sh writes for SITE_URL and the key;
#   - no file of the signed repository differs from the prepared one except the summary files and the signatures of the
#     listed commits (no other object, no stray file, no config change);
#   - a client that has only signing-key.asc accepts the summary and every listed commit (flatpak remote-add, remote-ls,
#     install --no-deploy for the app and runtime refs; for every listed ref an ostree pull with commit and summary signatures checked);
#   - the GPGKey of the .flatpakrepo and the .flatpakref is exactly that key.
# Environment:
#   EXPECT_PREPARED_SHA256, EXPECT_SIGNLIST_SHA256, EXPECT_SIGNED_SHA256, EXPECT_REFS_SHA256   as prepare-flatpak, sign-flatpak
#                            and build-flatpak reported them
#   SITE_URL                 where Pages serves the site (the files outside repo/ are compared with what is written for it)
#   GITHUB_REPOSITORY        owner/name, to find gh-pages (or PAGES_REMOTE: the git URL)
#   EXPECT_BASE_SHA          the gh-pages commit the build started from ("-" when there was none); passed on to the output
#   GITHUB_SHA               the commit of the run; HEAD of the checkout must be exactly it
#   RUNNER_TEMP              where scratch files go (default $TMPDIR, then /tmp)
#   SIGNING_PUBKEY           (optional) the public key, default build/flatpak/signing-key.asc
#   KUICKSHARE_ROOT          (optional) the checkout whose HEAD is compared
# Refuses to run when FLATPAK_GPG_PRIVATE_KEY is set. Writes site_sha256= and base_sha= to $GITHUB_OUTPUT when it is set.
# Needs flatpak, ostree, gpg 2.2+, git, tar, base64, diff. Exit codes: 0 ok, 1 a check failed, 2 bad input or the secret is
# set, 3 gh-pages is not at the base of the build.
# shellcheck disable=SC2154,SC2034 # $tab and $remote (used by check_pages_base) come from flatpak-lib.sh
set -eu
tools=$(cd "$(dirname "$0")/../.." && pwd)
root=${KUICKSHARE_ROOT:-$tools}
pubkey=${SIGNING_PUBKEY:-$tools/build/flatpak/signing-key.asc}
me=finish-flatpak-repo
scratch=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
tmp=$(mktemp -d "$scratch/finish.XXXXXX")
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
[ $# -eq 5 ] || bad_input "usage: finish-flatpak-repo.sh <prepared.tar> <sign-list.txt> <signed-repo.tar> <refs.txt> <out-dir>"
: "${GITHUB_SHA:?}" "${EXPECT_PREPARED_SHA256:?}" "${EXPECT_SIGNLIST_SHA256:?}" "${EXPECT_SIGNED_SHA256:?}" "${EXPECT_REFS_SHA256:?}" "${EXPECT_BASE_SHA:?}"
: "${SITE_URL:?}"
remote=${PAGES_REMOTE:-https://github.com/${GITHUB_REPOSITORY:?}.git}
printf '%s\n' "$EXPECT_BASE_SHA" | grep -Eq '^(-|[0-9a-f]{40})$' || bad_input "EXPECT_BASE_SHA is not a commit or -"
[ "$(git -C "$root" rev-parse HEAD)" = "$GITHUB_SHA" ] || fail "HEAD is not $GITHUB_SHA"
for f in "$1" "$2" "$3" "$4"; do [ -f "$f" ] || bad_input "$f does not exist"; done
prepared=$(readlink -f "$1")
signlist=$(readlink -f "$2")
signed=$(readlink -f "$3")
refs=$(readlink -f "$4")
mkdir -p "$5"
out=$(cd "$5" && pwd)
mkdir -m 700 "$tmp/gnupg"
GNUPGHOME=$tmp/gnupg
export GNUPGHOME

# ---- the files ----
check_sha256 "$prepared" "$EXPECT_PREPARED_SHA256" "the prepared site"
check_sha256 "$signlist" "$EXPECT_SIGNLIST_SHA256" "the sign list"
check_sha256 "$signed" "$EXPECT_SIGNED_SHA256" "the signed repository"
check_sha256 "$refs" "$EXPECT_REFS_SHA256" "refs.txt"
read_refs "$refs"
[ "$base" = "$EXPECT_BASE_SHA" ] || bad_input "refs.txt says the build started from $base, not from $EXPECT_BASE_SHA"
tar_safe "$prepared" site
tar_safe "$signed" repo
mkdir "$tmp/prepared" "$tmp/signed"
tar -xf "$prepared" -C "$tmp/prepared" --no-same-owner
tar -xf "$signed" -C "$tmp/signed" --no-same-owner
site=$tmp/prepared/site
new=$tmp/signed/repo
old=$site/repo
[ -d "$old/objects" ] && [ -d "$new/objects" ] || bad_input "the prepared site or the signed archive has no ostree repository"
for r in "$old" "$new"; do mkdir -p "$r/refs/heads" "$r/refs/mirrors" "$r/refs/remotes" "$r/tmp"; done
read_signlist "$signlist" "$refs"

# ---- the signed repository is the prepared one plus signatures ----
ref_map "$old" > "$tmp/refs.old"
ref_map "$new" > "$tmp/refs.new"
[ -s "$tmp/refs.old" ] || fail "the prepared repository has no ref"
cmp -s "$tmp/refs.old" "$tmp/refs.new" || { diff "$tmp/refs.old" "$tmp/refs.new" >&2 || true; fail "the signed repository has other refs or commits than the prepared one"; }
while IFS=$tab read -r ref commit; do
    grep -qxF "$ref $commit" "$tmp/refs.new" || fail "$ref is not at the listed commit $commit in the signed repository"
done < "$signlist"
ostree --repo="$new" fsck --quiet > "$tmp/fsck.log" 2>&1 || { sed 's/^/    /' "$tmp/fsck.log" >&2; fail "ostree fsck of the signed repository failed"; }
while IFS=$tab read -r ref commit; do check_binding "$new" "$ref" "$commit"; done < "$signlist"

no_deltas "$old"
no_deltas "$new"
# ---- against gh-pages: the prepared repository is not trusted for what the build did not make ----
check_pages_base "$new"
# The only files that may be new, changed or gone: the summary files and the signatures of commits.
allowed='^\./(summary|summary\.sig|summary\.idx|summary\.idx\.sig)$|^\./summaries/[^/]+$'
while IFS=$tab read -r ref commit; do printf './objects/%s/%s.commitmeta\n' "$(printf '%s' "$commit" | cut -c1-2)" "${commit#??}"; done < "$signlist" > "$tmp/files.signatures"
for r in old new; do
    case $r in old) d=$old ;; *) d=$new ;; esac
    (cd "$d" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum > "$tmp/files.$r")
done
# Lines are "hash  ./path": a path that is in one list only, or with another hash, shows up as a line of the diff.
diff "$tmp/files.old" "$tmp/files.new" | sed -n 's/^[<>] [0-9a-f]\{64\}  //p' | LC_ALL=C sort -u > "$tmp/files.changed"
if bad=$(grep -Ev "$allowed" "$tmp/files.changed" | grep -vxF -f "$tmp/files.signatures"); then
    printf '%s\n' "$bad" | sed 's/^/    /' >&2
    fail "the signed repository differs from the prepared one in files that are not summary files or signatures of the listed commits"
fi
echo "$me: the signed repository is the prepared one plus $(wc -l < "$tmp/files.changed") summary and signature files"

# ---- the key ----
want_fpr=$(key_fpr "$pubkey")
printf '%s\n' "$want_fpr" | grep -Eq '^[0-9A-F]{40}$' || bad_input "cannot read the fingerprint of $pubkey"
gpg --batch --dearmor < "$pubkey" > "$tmp/public.gpg"
for f in kuickshare.flatpakrepo kuickshare.flatpakref; do
    key=$(sed -n 's/^GPGKey=//p' "$site/$f")
    [ -n "$key" ] || fail "$f has no GPGKey"
    printf '%s' "$key" | base64 -d > "$tmp/$f.gpg" 2> /dev/null || fail "the GPGKey of $f is not valid base64"
    [ "$(key_fpr "$tmp/$f.gpg")" = "$want_fpr" ] || fail "the GPGKey of $f is not exactly the key $want_fpr of $(basename "$pubkey")"
done

# Everything outside the repository is what publish-repo.sh writes for this URL and this key, and nothing else (no extra file,
# no changed .flatpakrepo, .flatpakref, icon or .nojekyll).
mkdir "$tmp/expected"
GPG_KEY_B64=$(sed -n 's/^GPGKey=//p' "$site/kuickshare.flatpakrepo") "$tools/build/flatpak/write-site-files.sh" "$tmp/expected" "$SITE_URL"
diff -r -x repo "$site" "$tmp/expected" > "$tmp/site.diff" 2>&1 || { sed 's/^/    /' "$tmp/site.diff" >&2; fail "the files of the site outside repo/ are not the ones publish-repo.sh writes"; }

# ---- check it as a client would: only the public key, a keyring of its own ----
FLATPAK_USER_DIR=$tmp/client
export FLATPAK_USER_DIR
mkdir "$FLATPAK_USER_DIR"
flatpak remote-add --user --gpg-import="$tmp/public.gpg" verify "file://$new"
flatpak remote-ls --user verify > /dev/null
while IFS=$tab read -r ref commit; do
    case $ref in
        app/* | runtime/*) flatpak install --user --noninteractive -y --no-deps --no-related --no-deploy verify "$ref" ;;
    esac
done < "$signlist"
# Flatpak does not refuse an unsigned appstream commit when it updates the appstream data (tried), so every listed head, the
# appstream ones too, is also pulled the way ostree does with signature checks on (commits and summary).
ostree --repo="$tmp/chk" init --mode=archive
ostree --repo="$tmp/chk" remote add --set=gpg-verify=true --set=gpg-verify-summary=true --gpg-import="$tmp/public.gpg" chk "file://$new"
# shellcheck disable=SC2046 # refs have no spaces (read_signlist)
ostree --repo="$tmp/chk" pull --untrusted chk $(cat "$tmp/sign.refs") > "$tmp/pull.log" 2>&1 || { sed 's/^/    /' "$tmp/pull.log" >&2; fail "a pull with signature checks of the listed refs failed"; }
echo "$me: a client with only the public key accepts the summary and the new commits"

# ---- the site, without git metadata and scratch files ----
rm -rf "$old"
mv "$new" "$old"
rm -rf "$old/tmp"
tar --sort=name --owner=0 --group=0 --numeric-owner --exclude=./.git -C "$site" -cf "$out/site.tar" .
site_sha=$(sha256sum "$out/site.tar" | cut -d' ' -f1)
echo "$me: site.tar $site_sha, built on gh-pages $EXPECT_BASE_SHA"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        echo "site_sha256=$site_sha"
        echo "base_sha=$EXPECT_BASE_SHA"
    } >> "$GITHUB_OUTPUT"
fi
