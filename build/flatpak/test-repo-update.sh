#!/bin/sh
# Proves, with throw-away keys and a local web server, that the split signing of the release (build/release/prepare-,
# sign- and finish-flatpak-repo.sh) does not stop installed Flatpak clients from updating. Nothing real is used: no published
# repository, no real key, no network.
#
#   1. Site S1 is made the way releases were once made: the commit is signed when it is created, then
#      build/flatpak/publish-repo.sh finishes the repository. A client adds it from its .flatpakrepo and installs.
#   2. Site S2 is made the new way: an unsigned commit is exported on top of a clone of S1 (the build job), taken out with
#      build/release/export-flatpak-commits.sh, finished unsigned by prepare-flatpak-repo.sh, signed with the same key by
#      sign-flatpak-repo.sh (--check, --unpack, --sign), and checked and archived by finish-flatpak-repo.sh.
#   3. The same URL then serves S3: S2 whose new commit (and summary) is signed by another key; the client's update must fail
#      and leave the installed commit alone. Then S2 itself: the update must succeed and reach the new commit, whose parent is
#      the installed one. There are no static deltas: the client fetches objects.
#      (A summary signed by another key alone is not refused by Flatpak 1.14 when the commits carry a trusted signature, so that
#      case is not asserted: the commit signature is what protects an installed client.)
# The application is the one the build made: pass its repository (default dist/flatpak-repo, as build-flatpak.sh leaves it).
# Runs where flatpak, ostree, gpg, git, python3 and curl are installed; CI runs it on plain ubuntu-24.04 with the apt packages the release's
# signing job installs, plus librsvg2-common (the re-export below validates an SVG icon). Needs no D-Bus and no bwrap (verified in a
# container without user namespaces, where bwrap cannot start).
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
# ostree on Ubuntu does not create missing parent directories of a new repository.
mkdir "$s1"
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
# As in the release: a clone lacks the empty directories of the repository.
mkdir -p "$work/pages/repo/refs/heads" "$work/pages/repo/refs/mirrors" "$work/pages/repo/refs/remotes" "$work/pages/repo/tmp"

# ---- S2: the build job exports an unsigned commit with another tree, the sign job signs it ----
ostree --repo="$built" checkout --user-mode "$ref" "$work/tree"
chmod -R u+w "$work/tree"
mkdir -p "$work/tree/files/share/kuickshare"
echo "repository update test" > "$work/tree/files/share/kuickshare/update-test"
# Every release changes the appstream data (a new <release>), so build-update-repo makes new, unsigned appstream commits that
# the sign list must carry. Done when the built tree has appstream data.
for xml in "$work"/tree/files/share/app-info/xmls/*.xml.gz; do
    [ -f "$xml" ] || continue
    gzip -dc "$xml" | sed 's|<summary>[^<]*</summary>|<summary>update test 2</summary>|' | gzip -n > "$xml.new"
    mv "$xml.new" "$xml"
done
# No sandbox for the icon check: it needs user namespaces, which CI runners may restrict.
flatpak build-export --disable-sandbox "$work/changed" "$work/tree" stable
flatpak build-commit-from --src-repo="$work/changed" --src-ref="$ref" --no-update-summary --subject="update test 2" "$work/pages/repo" "$ref"
"$root/build/release/export-flatpak-commits.sh" "$work/pages" "$work/commits"
# The checks of the scripts look at HEAD of a checkout: a one-commit repository stands in for it.
git init -q --initial-branch=main "$work/checkout"
git -C "$work/checkout" commit -q --allow-empty -m "Release"
sha=$(git -C "$work/checkout" rev-parse HEAD)
export GITHUB_SHA=$sha KUICKSHARE_ROOT=$work/checkout PAGES_REMOTE=file://$work/origin.git SITE_URL=$url RUNNER_TEMP=$work SIGNING_PUBKEY=$work/key-a/public.asc
# prepare lists the published releases with gh (no network, no real repository here): one, at the version of the app that S1
# publishes, with the Flatpak bundle. prepare reads that version from the signed app head with the real ostree and compares it.
version=$(ostree --repo="$built" cat "$ref" "/files/share/metainfo/$id.metainfo.xml" | awk '/<release / && !d { v = $0; sub(/.*version="/, "", v); sub(/".*/, "", v); print v; d = 1 }') || true
[ -n "$version" ] || fail "the built app has no metainfo with a release (/files/share/metainfo/$id.metainfo.xml)"
mkdir "$work/bin"
printf '#!/bin/sh\necho '"'"'[{"tag_name":"v%s","draft":false,"prerelease":false,"assets":[{"name":"KuickShare-x86_64.flatpak","state":"uploaded"}]}]'"'"'\n' "$version" > "$work/bin/gh"
chmod +x "$work/bin/gh"
export GITHUB_REPOSITORY=example/KuickShare PATH=$work/bin:$PATH
base=$(git -C "$work/origin.git" rev-parse gh-pages)
mkdir "$work/ci"
# prepare-flatpak: no key
EXPECT_COMMITS_SHA256=$(sha256sum "$work/commits/flatpak-commits.tar" | cut -d' ' -f1) EXPECT_REFS_SHA256=$(sha256sum "$work/commits/refs.txt" | cut -d' ' -f1) \
    GITHUB_OUTPUT=$work/ci/prepare.out "$root/build/release/prepare-flatpak-repo.sh" "$work/commits/flatpak-commits.tar" "$work/prepared"
EXPECT_PREPARED_SHA256=$(sed -n 's/^prepared_sha256=//p' "$work/ci/prepare.out")
EXPECT_SIGNLIST_SHA256=$(sed -n 's/^signlist_sha256=//p' "$work/ci/prepare.out")
# check-flatpak: the checks, without the key, on a machine of their own
EXPECT_PREPARED_SHA256=$EXPECT_PREPARED_SHA256 EXPECT_SIGNLIST_SHA256=$EXPECT_SIGNLIST_SHA256 EXPECT_BASE_SHA=$base \
    EXPECT_REFS_SHA256=$(sha256sum "$work/commits/refs.txt" | cut -d' ' -f1) GITHUB_OUTPUT=$work/ci/check.out \
    "$root/build/release/sign-flatpak-repo.sh" --check "$work/prepared/prepared.tar" "$work/prepared/sign-list.txt" "$work/commits/refs.txt" "$work/checked"
# sign-flatpak: the checked files against the hashes of the check, prepare and build, without the key, then the key in the step that signs
EXPECT_CHECKED_SHA256=$(sed -n 's/^checked_sha256=//p' "$work/ci/check.out") EXPECT_SIGNLIST_SHA256=$EXPECT_SIGNLIST_SHA256 \
    EXPECT_REFS_SHA256=$(sha256sum "$work/commits/refs.txt" | cut -d' ' -f1) \
    "$root/build/release/sign-flatpak-repo.sh" --unpack "$work/checked/checked-repo.tar" "$work/checked/sign-list.txt" "$work/commits/refs.txt" "$work/signwork"
FLATPAK_GPG_PRIVATE_KEY=$(GNUPGHOME=$work/key-a gpg --batch --pinentry-mode loopback --passphrase '' --armor --export-secret-keys "$fpr_a") \
    GITHUB_OUTPUT=$work/ci/sign.out "$root/build/release/sign-flatpak-repo.sh" --sign "$work/signwork" "$work/signed-repo"
# finish-flatpak: no key
EXPECT_PREPARED_SHA256=$EXPECT_PREPARED_SHA256 EXPECT_SIGNLIST_SHA256=$EXPECT_SIGNLIST_SHA256 EXPECT_BASE_SHA=$base \
    EXPECT_SIGNED_SHA256=$(sed -n 's/^signed_sha256=//p' "$work/ci/sign.out") EXPECT_REFS_SHA256=$(sha256sum "$work/commits/refs.txt" | cut -d' ' -f1) \
    "$root/build/release/finish-flatpak-repo.sh" "$work/prepared/prepared.tar" "$work/prepared/sign-list.txt" "$work/signed-repo/signed-repo.tar" "$work/commits/refs.txt" "$work/signed"
mkdir "$work/s2"
tar -xf "$work/signed/site.tar" -C "$work/s2"
c2=$(ostree --repo="$work/s2/repo" rev-parse "$ref")
[ "$c2" != "$c1" ] || fail "the new commit is the old one"
[ "$(ostree --repo="$work/s2/repo" rev-parse "$c2^")" = "$c1" ] || fail "the new commit does not have the old one as its parent"
echo "ok: S2 built the new way, commit $c2 on top of $c1"

# ---- S4: gh-pages as an attacker could write it: refs/heads/X moved to another commit, the signed summary left as it was ----
# ostree takes the commit of a ref from the verified summary (tried), so a pull alone would not notice; prepare compares the refs.
fpr_b=$(cat "$work/key-b/fpr")
mkdir -p "$work/evil-tree/files"
echo evil > "$work/evil-tree/files/evil"
for variant in unsigned other-key; do
    git clone -q --bare "file://$work/origin.git" "$work/origin-$variant.git"
    git clone -q --depth 1 --branch gh-pages "file://$work/origin-$variant.git" "$work/bad-$variant"
    mkdir -p "$work/bad-$variant/repo/refs/heads" "$work/bad-$variant/repo/refs/mirrors" "$work/bad-$variant/repo/refs/remotes" "$work/bad-$variant/repo/tmp"
    if [ "$variant" = unsigned ]; then signing=; else signing="--gpg-sign=$fpr_b --gpg-homedir=$work/key-b"; fi
    # shellcheck disable=SC2086 # the options have no spaces
    ostree --repo="$work/bad-$variant/repo" commit --branch="$ref" --tree=dir="$work/evil-tree" --no-xattrs --subject="moved ref ($variant)" $signing > /dev/null
    git -C "$work/bad-$variant" add --all
    git -C "$work/bad-$variant" commit -q -m "ref moved, summary unchanged"
    git -C "$work/bad-$variant" push -q "file://$work/origin-$variant.git" HEAD:gh-pages
    # the build job exports on top of that gh-pages, as it would
    git clone -q --depth 1 --branch gh-pages "file://$work/origin-$variant.git" "$work/pages-$variant"
    mkdir -p "$work/pages-$variant/repo/refs/heads" "$work/pages-$variant/repo/refs/mirrors" "$work/pages-$variant/repo/refs/remotes" "$work/pages-$variant/repo/tmp"
    flatpak build-commit-from --src-repo="$work/changed" --src-ref="$ref" --no-update-summary --subject="update test 3" "$work/pages-$variant/repo" "$ref"
    "$root/build/release/export-flatpak-commits.sh" "$work/pages-$variant" "$work/commits-$variant" > /dev/null
    if EXPECT_COMMITS_SHA256=$(sha256sum "$work/commits-$variant/flatpak-commits.tar" | cut -d' ' -f1) EXPECT_REFS_SHA256=$(sha256sum "$work/commits-$variant/refs.txt" | cut -d' ' -f1) \
        PAGES_REMOTE=file://$work/origin-$variant.git "$root/build/release/prepare-flatpak-repo.sh" "$work/commits-$variant/flatpak-commits.tar" "$work/prepared-$variant" > "$work/prepare-$variant.log" 2>&1; then
        cat "$work/prepare-$variant.log"
        fail "prepare accepted a gh-pages whose ref was moved under the signed summary ($variant)"
    fi
    grep -q "not the refs and commits of its signed summary" "$work/prepare-$variant.log" || { cat "$work/prepare-$variant.log"; fail "prepare refused ($variant) for another reason"; }
    [ ! -e "$work/prepared-$variant/prepared.tar" ] || fail "prepare wrote its output after refusing ($variant)"
done
echo "ok: a gh-pages with a ref moved under its signed summary is refused (commit unsigned, or signed by another key)"

# ---- S3: S2 whose new commit is signed by somebody else (the published key's signature removed) ----
cp -a "$work/s2" "$work/s3"
fpr_b=$(cat "$work/key-b/fpr")
ostree --repo="$work/s3/repo" gpg-sign --delete --gpg-homedir="$work/key-a" "$c2" "$fpr_a" > /dev/null
ostree --repo="$work/s3/repo" gpg-sign --gpg-homedir="$work/key-b" "$c2" "$fpr_b"
flatpak build-update-repo --gpg-sign="$fpr_b" --gpg-homedir="$work/key-b" "$work/s3/repo" > /dev/null

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
update > "$work/update-s3.log" 2>&1 || true
[ "$(installed)" = "$c1" ] || { cat "$work/update-s3.log"; fail "a commit signed by another key was installed"; }
echo "ok: a commit signed by another key is refused"

serve "$work/s2"
before=$(wc -l < "$work/http.log")
update || fail "the update to the repository signed the new way failed"
[ "$(installed)" = "$c2" ] || fail "the installed commit is $(installed), not $c2"
if tail -n +"$((before + 1))" "$work/http.log" | grep -q '/repo/deltas/'; then fail "the client asked for a static delta, and the site has none"; fi
echo "ok: the client updated from $c1 to $c2 (child of $c1), without static deltas"
echo "done"
