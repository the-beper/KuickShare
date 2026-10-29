#!/bin/sh
# Proves, with throw-away keys and a local web server, that moving the signing out of the build (build/release/sign-flatpak-repo.sh)
# does not stop installed Flatpak clients from updating. Nothing real is used: no published repository, no real key, no network.
#
#   1. Site S1 is made the way releases were made until now: the commit is signed when it is created, then
#      build/flatpak/publish-repo.sh finishes the repository. A client adds it from its .flatpakrepo and installs.
#   2. Site S2 is made the new way: an unsigned commit is exported on top of a clone of S1 (the build job), taken out with
#      build/release/export-flatpak-commits.sh, and signed and finished by build/release/sign-flatpak-repo.sh with the same key.
#   3. The same URL then serves, in turn: S3 (S2 with the summary signed by another key) and S2 without summary signatures; the
#      client's update must fail and leave the installed commit alone. Then S2 itself: the update must succeed, reach the new
#      commit, whose parent is the installed one, and fetch a static delta.
# The application is the one the build made: pass its repository (default dist/flatpak-repo, as build-flatpak.sh leaves it).
# Runs in the Flatpak image (flatpak, ostree, gpg, git, python3, curl); needs no D-Bus and no bwrap.
# Usage: build/flatpak/test-repo-update.sh [<built repository>]
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
built=$(readlink -f "${1:-$root/dist/flatpak-repo}")
id=io.github.the_beper.KuickShare
ref=app/$id/x86_64/stable
fail() { echo "FAIL: $*" >&2; exit 1; }
[ -d "$built/objects" ] || fail "$built is not an OSTree repository (build with build/flatpak/build-flatpak.sh first)"
ostree --repo="$built" rev-parse "$ref" > /dev/null || fail "$built has no $ref"

work=$(mktemp -d)
server=
cleanup() {
    set +e
    [ -z "$server" ] || kill "$server" 2> /dev/null
    for home in "$work"/key-*; do
        [ -d "$home" ] && gpgconf --homedir "$home" --kill gpg-agent 2> /dev/null
    done
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
HOME=$work/home
export HOME
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
mkdir -p "$HOME"

# ---- two keys: the repository's (A) and somebody else's (B) ----
for k in a b; do
    mkdir -m 700 "$work/key-$k"
    GNUPGHOME=$work/key-$k gpg --batch --quiet --pinentry-mode loopback --passphrase '' --quick-gen-key "KuickShare test $k" ed25519 sign never
    GNUPGHOME=$work/key-$k gpg --batch --list-keys --with-colons | awk -F: '$1 == "fpr" { print $10; exit }' > "$work/key-$k/fpr"
    GNUPGHOME=$work/key-$k gpg --batch --armor --export "$(cat "$work/key-$k/fpr")" > "$work/key-$k/public.asc"
done
fpr_a=$(cat "$work/key-a/fpr")

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
url=http://127.0.0.1:$port/current
mkdir "$work/www"

# ---- S1: signed when the commit is made (the old way) ----
s1=$work/s1
ostree --repo="$s1/repo" init --mode=archive-z2
flatpak build-commit-from --src-repo="$built" --src-ref="$ref" --gpg-sign="$fpr_a" --gpg-homedir="$work/key-a" \
    --no-update-summary --subject="update test 1" "$s1/repo" "$ref"
GNUPGHOME=$work/key-a GPG_KEY_ID=$fpr_a "$root/build/flatpak/publish-repo.sh" "$s1" "$url"
c1=$(ostree --repo="$s1/repo" rev-parse "$ref")
echo "ok: S1 built the old way, commit $c1"

# ---- gh-pages as a git remote, a clone for the build job to export onto ----
git init -q --bare --initial-branch=gh-pages "$work/origin.git"
git -C "$s1" init -q --initial-branch=gh-pages
git -C "$s1" add --all
git -C "$s1" commit -q -m "S1"
git -C "$s1" push -q "$work/origin.git" gh-pages
git clone -q --depth 1 --branch gh-pages "file://$work/origin.git" "$work/pages"

# ---- S2: the build job exports an unsigned commit with another tree, the sign job signs it ----
ostree --repo="$built" checkout --user-mode "$ref" "$work/tree"
chmod -R u+w "$work/tree"
mkdir -p "$work/tree/files/share/kuickshare"
echo "repository update test" > "$work/tree/files/share/kuickshare/update-test"
flatpak build-export "$work/changed" "$work/tree" stable
flatpak build-commit-from --src-repo="$work/changed" --src-ref="$ref" --no-update-summary --subject="update test 2" "$work/pages/repo" "$ref"
"$root/build/release/export-flatpak-commits.sh" "$work/pages" "$work/commits"
# The checks of the sign script look at HEAD of a checkout: a one-commit repository stands in for it.
git init -q --initial-branch=main "$work/checkout"
git -C "$work/checkout" commit -q --allow-empty -m "Release"
sha=$(git -C "$work/checkout" rev-parse HEAD)
GITHUB_SHA=$sha KUICKSHARE_ROOT=$work/checkout PAGES_REMOTE=file://$work/origin.git SITE_URL=$url RUNNER_TEMP=$work \
    SIGNING_PUBKEY=$work/key-a/public.asc EXPECT_COMMITS_SHA256=$(sha256sum "$work/commits/flatpak-commits.tar" | cut -d' ' -f1) \
    FLATPAK_GPG_PRIVATE_KEY=$(GNUPGHOME=$work/key-a gpg --batch --pinentry-mode loopback --passphrase '' --armor --export-secret-keys "$fpr_a") \
    "$root/build/release/sign-flatpak-repo.sh" "$work/commits/flatpak-commits.tar" "$work/signed"
mkdir "$work/s2"
tar -xf "$work/signed/site.tar" -C "$work/s2"
c2=$(ostree --repo="$work/s2/repo" rev-parse "$ref")
[ "$c2" != "$c1" ] || fail "the new commit is the old one"
[ "$(ostree --repo="$work/s2/repo" rev-parse "$c2^")" = "$c1" ] || fail "the new commit does not have the old one as its parent"
echo "ok: S2 built the new way, commit $c2 on top of $c1"

# ---- S3: S2 whose summary is signed by somebody else; S4: S2 without summary signatures ----
cp -a "$work/s2" "$work/s3"
for f in summary summary.idx; do
    [ -f "$work/s3/repo/$f" ] || continue
    rm -f "$work/s3/repo/$f.sig"
    GNUPGHOME=$work/key-b gpg --batch --quiet --pinentry-mode error --local-user "$(cat "$work/key-b/fpr")" --detach-sign -o "$work/s3/repo/$f.sig" "$work/s3/repo/$f"
done
cp -a "$work/s2" "$work/s4"
rm -f "$work/s4/repo/summary.sig" "$work/s4/repo/summary.idx.sig"

# ---- the server and the client ----
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work/www" > "$work/http.log" 2>&1 &
server=$!
serve() { ln -sfn "$1" "$work/www/current"; }
serve "$work/s1"
for _ in $(seq 20); do curl -fsS -o /dev/null "$url/kuickshare.flatpakrepo" 2> /dev/null && break; sleep 0.5; done
curl -fsS -o /dev/null "$url/kuickshare.flatpakrepo" || fail "the test server does not answer"

FLATPAK_USER_DIR=$work/client
export FLATPAK_USER_DIR
flatpak remote-add --user kuickshare "$url/kuickshare.flatpakrepo"
flatpak install --user --noninteractive -y --no-deps --no-related kuickshare "$ref"
installed() { flatpak info --user --show-commit "$id"; }
[ "$(installed)" = "$c1" ] || fail "the installed commit is $(installed), not $c1"
echo "ok: a client installed S1 from its .flatpakrepo"

update() { flatpak update --user --noninteractive -y --no-deps --no-related "$id"; }

serve "$work/s3"
if update > "$work/update-s3.log" 2>&1; then cat "$work/update-s3.log"; fail "the update succeeded with a summary signed by another key"; fi
[ "$(installed)" = "$c1" ] || fail "S3 changed the installed commit"
echo "ok: a summary signed by another key is refused"

serve "$work/s4"
if update > "$work/update-s4.log" 2>&1; then cat "$work/update-s4.log"; fail "the update succeeded without summary signatures"; fi
[ "$(installed)" = "$c1" ] || fail "S4 changed the installed commit"
echo "ok: a summary without signatures is refused"

serve "$work/s2"
before=$(wc -l < "$work/http.log")
update || fail "the update to the repository signed the new way failed"
[ "$(installed)" = "$c2" ] || fail "the installed commit is $(installed), not $c2"
tail -n +"$((before + 1))" "$work/http.log" | grep -q '/repo/deltas/' || { tail -n +"$((before + 1))" "$work/http.log"; fail "the client did not ask for a static delta"; }
echo "ok: the client updated from $c1 to $c2 (child of $c1) over a static delta"
echo "done"
