#!/bin/sh
# Local simulation of the release: runs the real check-version.sh, verify-assets.sh, export-flatpak-commits.sh,
# sign-flatpak-repo.sh and publish.sh (and build/flatpak/publish-repo.sh) against fixtures, with gh, flatpak, ostree and
# curl replaced by the fakes in build/release/testdata (put first in PATH). The signatures are real GnuPG signatures made
# with ephemeral keys generated in temporary keyrings; no real key, no network, no container.
# Needs only sh, git, gpg 2.2+, python3, tar, sha256sum, sha1sum. Everything happens in a mktemp directory that is removed
# at the end. Output is TAP: "ok N - ..." lines and "# 13/13 passed"; the exit status is not 0 if a case fails.
#
# What it does NOT show (see CONTRIBUTING.md, Releases): that real flatpak, ostree, gh and GitHub Pages behave like the
# fakes. build/flatpak/test-repo-update.sh covers the real Flatpak client; the rest needs a release.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$here/../.." && pwd)
td=$here/testdata

for tool in git gpg python3 tar sha256sum sha1sum base64; do
    command -v "$tool" > /dev/null || { echo "Bail out! $tool is needed"; exit 2; }
done

work=$(mktemp -d)
keys=$work/keys
mkdir -p "$keys" "$work/bin" "$work/home"
cleanup() {
    set +e
    for home in "$keys"/*; do
        [ -d "$home" ] && gpgconf --homedir "$home" --kill gpg-agent 2> /dev/null
    done
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

# The fakes, first in PATH; git and gpg stay the real ones. No configuration of the host leaks in.
for fake in gh flatpak ostree curl; do ln -s "$td/fake-$fake" "$work/bin/$fake"; done
PATH=$work/bin:$PATH
HOME=$work/home
GNUPGHOME=$work/gnupg-unused
GIT_CONFIG_GLOBAL=/dev/null
GIT_CONFIG_SYSTEM=/dev/null
GIT_AUTHOR_NAME=simulate GIT_AUTHOR_EMAIL=simulate@example.invalid
GIT_COMMITTER_NAME=simulate GIT_COMMITTER_EMAIL=simulate@example.invalid
export PATH HOME GNUPGHOME GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL
mkdir -m 700 "$GNUPGHOME"

rel=$repo_root/build/release
app_id=io.github.the_beper.KuickShare
total=13
n=0
passed=0
failed=0
case_failed=0
diag=
rc=0

# ---- test harness ----
run() { # run a command, keep its output in $work/last.out and in the log of everything, its status in $rc
    rc=0
    "$@" > "$work/last.out" 2>&1 || rc=$?
    cat "$work/last.out" >> "$work/all.log"
}
check() { # check "what" command args...: records a failure when the command fails
    what=$1
    shift
    if "$@"; then :; else
        case_failed=1
        diag="$diag# FAILED: $what
"
    fi
}
end_case() {
    n=$((n + 1))
    if [ "$case_failed" = 0 ]; then
        passed=$((passed + 1))
        echo "ok $n - $1"
    else
        failed=$((failed + 1))
        echo "not ok $n - $1"
        printf '%s' "$diag"
        echo "# last output:"
        sed 's/^/#   /' "$work/last.out" | tail -n 12
    fi
    case_failed=0
    diag=
}
not() { ! "$@"; }
rc_is() { [ "$rc" -eq "$1" ]; }
rc_fails() { [ "$rc" -ne 0 ]; }
out_has() { grep -q -e "$1" "$work/last.out"; }
file_has() { grep -q -e "$2" "$1"; }
same() { [ "$1" = "$2" ]; }
sha256_of() { sha256sum "$1" 2> /dev/null | cut -d' ' -f1; }
gh_count() { awk -v p="$1" '$0 == p { c++ } END { print c + 0 }' "$FAKE_GH_LOG"; }
gh_calls() { [ "$(wc -l < "$FAKE_GH_LOG")" -eq 0 ]; }
release_state() { # "none", or "draft|published <number of assets> latest|-"
    python3 -I -c '
import json, sys
state = json.load(open(sys.argv[1]))
found = [r for r in state["releases"] if r["tag_name"] == sys.argv[2]]
if not found:
    print("none")
else:
    r = found[0]
    print("draft" if r["draft"] else "published", len(r["assets"]), "latest" if state["latest"] == r["id"] else "-")
' "$FAKE_GH_STATE" "$RELEASE_TAG"
}
state_is() { [ "$(release_state)" = "$1" ]; }
pages_head() { git -C "$origin" rev-parse --verify -q refs/heads/gh-pages; }
pages_file() { git -C "$origin" show "refs/heads/gh-pages:$1"; }
no_leftovers() { [ -z "$(find "$RUNNER_TEMP" -mindepth 1 -maxdepth 1)" ]; }

# ---- ephemeral keys: a real GnuPG key pair each, made in a keyring that exists only in this run ----
gen_key() {
    d=$keys/$1
    mkdir -m 700 "$d"
    GNUPGHOME=$d gpg --batch --pinentry-mode loopback --passphrase '' --quick-gen-key "KuickShare ephemeral $1" ed25519 sign never > "$d/gen.log" 2>&1 \
        || { cat "$d/gen.log"; echo "Bail out! cannot generate a key"; exit 2; }
    GNUPGHOME=$d gpg --batch --list-keys --with-colons 2>> "$d/gen.log" | awk -F: '$1 == "fpr" { print $10; exit }' > "$d/fpr"
    GNUPGHOME=$d gpg --batch --pinentry-mode loopback --passphrase '' --armor --export-secret-keys "$(cat "$d/fpr")" 2>> "$d/gen.log" > "$d/secret.asc"
    GNUPGHOME=$d gpg --batch --armor --export "$(cat "$d/fpr")" 2>> "$d/gen.log" > "$d/pub.asc"
    GNUPGHOME=$d gpg --batch --list-secret-keys --with-keygrip --with-colons 2>> "$d/gen.log" | awk -F: '$1 == "grp" { print $10 }' > "$d/grips"
}
use_secret() { FLATPAK_GPG_PRIVATE_KEY=$(cat "$keys/$1/secret.asc"); export FLATPAK_GPG_PRIVATE_KEY; }
drop_secret() { unset FLATPAK_GPG_PRIVATE_KEY; }

# ---- fixtures ----
make_assets() { # <dir> <version>
    d=$1
    v=$2
    mkdir -p "$d" "$d.tarstage/KuickShare-$v-x86_64/usr/share/kuickshare"
    { printf '\177ELF'; echo "fake AppImage $v"; } > "$d/KuickShare-x86_64.AppImage"
    echo "${3:-$v}" > "$d.tarstage/KuickShare-$v-x86_64/usr/share/kuickshare/VERSION"
    echo '#!/bin/sh' > "$d.tarstage/KuickShare-$v-x86_64/install.sh"
    tar -C "$d.tarstage" -czf "$d/KuickShare-x86_64.tar.gz" "KuickShare-$v-x86_64"
    echo "fake flatpak bundle $v" > "$d/KuickShare-x86_64.flatpak"
    python3 -I - "$d" <<'PY'
import hashlib
import sys

d = sys.argv[1]
data = open(d + "/KuickShare-x86_64.AppImage", "rb").read()
header = ("zsync: 0.6.2\nFilename: KuickShare-x86_64.AppImage\nMTime: Thu, 01 Jan 2026 00:00:00 +0000\nBlocksize: 2048\n"
          "Length: %d\nHash-Lengths: 2,2,5\nURL: KuickShare-x86_64.AppImage\nSHA-1: %s\n\n" % (len(data), hashlib.sha1(data).hexdigest()))
open(d + "/KuickShare-x86_64.AppImage.zsync", "wb").write(header.encode() + b"\0fake block checksums")
PY
    rm -rf "$d.tarstage"
}

make_world() { # <number>: a fresh project, origin and published repository, and the environment the scripts read
    w=$work/w$1
    origin=$w/origin.git
    mkdir -p "$w/runner-temp"
    git init -q --bare --initial-branch=main "$origin"
    seed=$w/seed
    git init -q --initial-branch=main "$seed"
    mkdir -p "$seed/src/shell" "$seed/data"
    printf '[package]\nname = "kuickshare-core"\nversion = "1.2.3"\n\n[dependencies]\nfoo = "1"\n' > "$seed/Cargo.toml"
    printf 'cmake_minimum_required(VERSION 3.20)\nproject(kuickshare VERSION 1.2.3 LANGUAGES CXX)\n' > "$seed/src/shell/CMakeLists.txt"
    cat > "$seed/data/$app_id.metainfo.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <releases>
    <release version="1.2.3" date="2026-01-01">
      <description>
        <p>Fixes the simulation.</p>
        <ul>
          <li>First change with <code>code</code>.</li>
        </ul>
      </description>
    </release>
  </releases>
</component>
XML
    git -C "$seed" add -A
    git -C "$seed" commit -q -m "Release 1.2.3"
    git -C "$seed" tag -a v1.2.3 -m v1.2.3
    git -C "$seed" push -q "$origin" main v1.2.3
    git clone -q "$origin" "$w/checkout"

    RELEASE_TAG=v1.2.3
    GITHUB_SHA=$(git -C "$w/checkout" rev-parse HEAD)
    GITHUB_REPOSITORY=example/KuickShare
    KUICKSHARE_ROOT=$w/checkout
    SITE_URL=https://pages.example/KuickShare
    PAGES_REMOTE=file://$origin
    REPO_REMOTE=file://$origin
    RUNNER_TEMP=$w/runner-temp
    SIGNING_PUBKEY=$keys/a/pub.asc
    GH_TOKEN=ephemeral-test-token
    PAGES_WAIT_SECS=3
    PAGES_POLL_INTERVAL=1
    FAKE_GH_STATE=$w/gh-state.json
    FAKE_GH_LOG=$w/gh.log
    FAKE_GH_ORIGIN=$origin
    FAKE_GH_FAIL=
    FAKE_GH_PARTIAL=
    FAKE_GH_NO_DIGEST=
    FAKE_PAGES_ORIGIN=$origin
    FAKE_PAGES_URL=$SITE_URL
    FAKE_PAGES_STALE=
    FAKE_OSTREE_LOG=$w/ostree.log
    FAKE_OSTREE_ORPHAN=
    FAKE_FLATPAK_LOG=$w/flatpak.log
    export RELEASE_TAG GITHUB_SHA GITHUB_REPOSITORY KUICKSHARE_ROOT SITE_URL PAGES_REMOTE REPO_REMOTE RUNNER_TEMP SIGNING_PUBKEY GH_TOKEN
    export PAGES_WAIT_SECS PAGES_POLL_INTERVAL FAKE_GH_STATE FAKE_GH_LOG FAKE_GH_ORIGIN FAKE_GH_FAIL FAKE_GH_PARTIAL FAKE_GH_NO_DIGEST
    export FAKE_PAGES_ORIGIN FAKE_PAGES_URL FAKE_PAGES_STALE FAKE_OSTREE_LOG FAKE_OSTREE_ORPHAN FAKE_FLATPAK_LOG
    : > "$FAKE_GH_LOG"
    echo '{"next_id": 1, "releases": [], "latest": null}' > "$FAKE_GH_STATE"
    unset EXPECT_SHA256_APPIMAGE EXPECT_SHA256_ZSYNC EXPECT_SHA256_TARBALL EXPECT_SHA256_FLATPAK
    drop_secret

    # The published repository (gh-pages): version 1.2.2, signed with key A, as the live one is.
    site=$w/base-site
    for ref in "app/$app_id/x86_64/stable" "runtime/$app_id.Locale/x86_64/stable"; do
        ostree --repo="$site/repo" fake-commit "$ref" "published 1.2.2 $ref" > /dev/null
    done
    flatpak build-sign --gpg-sign="$(cat "$keys/a/fpr")" --gpg-homedir="$keys/a" "$site/repo" "$app_id" stable
    flatpak build-sign --gpg-sign="$(cat "$keys/a/fpr")" --gpg-homedir="$keys/a" --runtime "$site/repo" "$app_id.Locale" stable
    GNUPGHOME=$keys/a GPG_KEY_ID=$(cat "$keys/a/fpr") "$repo_root/build/flatpak/publish-repo.sh" "$site" "$SITE_URL"
    git -C "$site" init -q --initial-branch=gh-pages
    git -C "$site" add -A
    git -C "$site" commit -q -m "Flatpak repository for v1.2.2"
    git -C "$site" push -q "$origin" HEAD:refs/heads/gh-pages
    base_sha=$(git -C "$site" rev-parse HEAD)
}

# What the build jobs do: export a build onto a clone of gh-pages, take the commits out, build the files.
build_jobs() {
    rm -rf "$w/pages" "$w/commits" "$w/dist"
    git clone -q --depth 1 --branch gh-pages "file://$origin" "$w/pages"
    ostree --repo="$w/pages/repo" fake-commit "app/$app_id/x86_64/stable" "built 1.2.3" > /dev/null
    ostree --repo="$w/pages/repo" fake-commit "runtime/$app_id.Locale/x86_64/stable" "built locale 1.2.3" > /dev/null
    run "$rel/export-flatpak-commits.sh" "$w/pages" "$w/commits"
    make_assets "$w/dist" 1.2.3
}
hashes_of_dist() {
    EXPECT_SHA256_APPIMAGE=$(sha256_of "$w/dist/KuickShare-x86_64.AppImage")
    EXPECT_SHA256_ZSYNC=$(sha256_of "$w/dist/KuickShare-x86_64.AppImage.zsync")
    EXPECT_SHA256_TARBALL=$(sha256_of "$w/dist/KuickShare-x86_64.tar.gz")
    EXPECT_SHA256_FLATPAK=$(sha256_of "$w/dist/KuickShare-x86_64.flatpak")
    export EXPECT_SHA256_APPIMAGE EXPECT_SHA256_ZSYNC EXPECT_SHA256_TARBALL EXPECT_SHA256_FLATPAK
}

# The jobs after the build, each leaves its status in $rc.
job_validate() { run "$rel/check-version.sh"; }
job_verify() {
    rm -rf "$w/assets"
    mkdir "$w/assets"
    cp "$w/dist"/* "$w/assets/"
    hashes_of_dist
    run "$rel/verify-assets.sh" "$w/assets"
}
job_sign() {
    rm -rf "$w/signed"
    EXPECT_COMMITS_SHA256=$(sha256_of "$w/commits/flatpak-commits.tar")
    export EXPECT_COMMITS_SHA256
    use_secret "${1:-a}"
    run "$rel/sign-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/signed"
    drop_secret
    cp "$work/last.out" "$w/sign.out"
}
# The same job with another tag, commit or public key (the caller's own values are restored).
with_tag() { saved=$RELEASE_TAG; RELEASE_TAG=$1; export RELEASE_TAG; "$2"; RELEASE_TAG=$saved; export RELEASE_TAG; }
with_sha() { saved=$GITHUB_SHA; GITHUB_SHA=$1; export GITHUB_SHA; "$2"; GITHUB_SHA=$saved; export GITHUB_SHA; }
sign_with_pubkey() { saved=$SIGNING_PUBKEY; SIGNING_PUBKEY=$keys/$1/pub.asc; export SIGNING_PUBKEY; job_sign "$2"; SIGNING_PUBKEY=$saved; export SIGNING_PUBKEY; }
job_publish() {
    ASSETS_DIR=$w/assets
    EXPECT_SUMS_SHA256=$(sha256_of "$w/assets/SHA256SUMS")
    SITE_TAR=$w/signed/site.tar
    EXPECT_SITE_SHA256=$(sha256_of "$w/signed/site.tar")
    SITE_BASE_SHA=$base_sha
    REFS_FILE=$w/signed/refs.txt
    RUN_URL=https://example.invalid/runs/1
    export ASSETS_DIR EXPECT_SUMS_SHA256 SITE_TAR EXPECT_SITE_SHA256 SITE_BASE_SHA REFS_FILE RUN_URL
    run "$rel/publish.sh"
}
full_release() { # a release that goes right through, up to and including publish
    make_world "$1"
    build_jobs
    job_validate
    job_verify
    job_sign
    job_publish
}

gen_key a
gen_key b
echo "TAP version 13"
echo "1..$total"

# ---- 1. the happy path ----
full_release 1
check "publish exits 0" rc_is 0
check "the release is published, has five assets and is the latest" state_is "published 5 latest"
check "the release notes come from the metainfo and name the commit" file_has "$FAKE_GH_STATE" "Built from commit"
check "the AppImage was uploaded once" same "$(gh_count 'upload KuickShare-x86_64.AppImage')" 1
check "gh-pages is the new site" same "$(pages_file repo/summary)" "$(tar -xOf "$w/signed/site.tar" ./repo/summary)"
check "the old gh-pages is kept as gh-pages-previous" same "$(git -C "$origin" rev-parse refs/heads/gh-pages-previous)" "$base_sha"
check "sign compared the GPGKey with the published one" file_has "$w/sign.out" "byte-identical"
check "the new app commit sits on the published one" same "$(awk -F'\t' '$1 ~ /^app\// { print $3 }' "$w/signed/refs.txt")" "$(ostree --repo="$site/repo" rev-parse "app/$app_id/x86_64/stable")"
end_case "happy path: validate, verify, sign, publish; release complete and latest, previous gh-pages kept"

# ---- 2. a missing artifact ----
make_world 2
build_jobs
rm "$w/dist/KuickShare-x86_64.AppImage.zsync"
job_verify
check "verify refuses a missing .zsync" rc_is 1
check "it says what is wrong" out_has "wrong set of files"
rm -rf "$w/assets"
mkdir "$w/assets"
cp "$w/dist"/* "$w/assets/"
ASSETS_DIR=$w/assets
EXPECT_SUMS_SHA256=0
SITE_TAR=$w/commits/flatpak-commits.tar
EXPECT_SITE_SHA256=0
SITE_BASE_SHA=$base_sha
export ASSETS_DIR EXPECT_SUMS_SHA256 SITE_TAR EXPECT_SITE_SHA256 SITE_BASE_SHA
run "$rel/publish.sh"
check "publish refuses to start without all assets" rc_fails
check "and made no call to GitHub" gh_calls
check "gh-pages was not touched" same "$(pages_head)" "$base_sha"
end_case "missing artifact: verify and publish refuse, nothing is created or pushed"

# ---- 3. an extra artifact ----
make_world 3
build_jobs
echo surprise > "$w/dist/KuickShare-x86_64.exe"
job_verify
check "verify refuses an extra file" rc_is 1
check "it says what is wrong" out_has "wrong set of files"
rm "$w/dist/KuickShare-x86_64.exe"
job_verify
job_sign
echo surprise > "$w/assets/notes.txt"
job_publish
check "publish refuses an extra file too" rc_fails
check "and made no call to GitHub" gh_calls
end_case "extra artifact: refused by verify and by publish"

# ---- 4. checksum mismatches ----
make_world 4
build_jobs
job_verify
check "verify passes a consistent set" rc_is 0
rm -f "$w/assets/SHA256SUMS"
EXPECT_SHA256_TARBALL=$(echo changed | sha256sum | cut -d' ' -f1)
run "$rel/verify-assets.sh" "$w/assets"
check "verify refuses a hash that is not the one the build job reported" rc_is 1
check "(the message names the file)" out_has "KuickShare-x86_64.tar.gz has SHA-256"
rm -rf "$w/dist"
make_assets "$w/dist" 1.2.3
{ printf 'zsync: 0.6.2\nFilename: KuickShare-x86_64.AppImage\nLength: 5\nSHA-1: %s\n\n' "$(echo wrong | sha1sum | cut -d' ' -f1)"; printf 'body'; } > "$w/dist/KuickShare-x86_64.AppImage.zsync"
job_verify
check "verify refuses a .zsync of another AppImage" rc_is 1
check "(the message names the .zsync)" out_has "zsync"
rm -rf "$w/dist"
make_assets "$w/dist" 1.2.3 1.2.2
job_verify
check "verify refuses a tarball whose VERSION is not the tag's" rc_is 1
check "(the message names the VERSION file)" out_has "VERSION file"
rm -rf "$w/dist"
make_assets "$w/dist" 1.2.3
job_verify
job_sign
echo tampered >> "$w/assets/KuickShare-x86_64.tar.gz"
job_publish
check "publish refuses a file changed after verify" rc_fails
check "(SHA256SUMS no longer matches)" out_has "does not match"
check "and made no call to GitHub" gh_calls
echo tampered > "$w/assets/SHA256SUMS"
job_publish
check "publish refuses another SHA256SUMS" rc_fails
end_case "checksum mismatch: build hash, zsync, tarball VERSION, change after verify"

# ---- 5. tag and version ----
make_world 5
job_validate
check "the consistent tag passes" rc_is 0
git -C "$seed" tag -a v1.2.4 -m v1.2.4 "v1.2.3^{commit}"
git -C "$seed" push -q "$origin" v1.2.4
git -C "$w/checkout" fetch -q --tags origin
with_tag v1.2.4 job_validate
check "a tag that is not the version of the sources fails" rc_is 1
check "(the message names the file)" out_has "Cargo.toml says"
for bad in v1.2 1.2.3 'v1.2.3;id' v1.2.3-rc1 'v1.2.3 '; do
    with_tag "$bad" job_validate
    check "the tag '$bad' is refused as malformed" rc_is 2
done
echo more > "$seed/more.txt"
git -C "$seed" add -A
git -C "$seed" commit -q -m "More"
git -C "$seed" push -q "$origin" main
git -C "$w/checkout" pull -q --ff-only origin main
with_sha "$(git -C "$w/checkout" rev-parse HEAD)" job_validate
check "a commit the tag does not point to fails" rc_is 1
check "(the message names the tag)" out_has "the tag v1.2.3 points to"
with_sha "$(git -C "$w/checkout" rev-parse HEAD~1)" job_validate
check "a SHA that is not HEAD fails" rc_is 1
git -C "$seed" checkout -q -b side v1.2.3
echo side > "$seed/side.txt"
git -C "$seed" add -A
git -C "$seed" commit -q -m "Side"
git -C "$seed" tag -f -a v1.2.3 -m v1.2.3 > /dev/null
git -C "$seed" push -q -f "$origin" side v1.2.3
git -C "$w/checkout" fetch -q --force --tags origin
git -C "$w/checkout" checkout -q --detach "$(git -C "$seed" rev-parse side)"
with_sha "$(git -C "$seed" rev-parse side)" job_validate
check "a commit that is not on main fails" rc_is 1
check "(the message says so)" out_has "not on main"
# A tag that moves after the sign job: publish notices, before it creates anything.
make_world 6
build_jobs
job_verify
job_sign
echo moved > "$seed/moved.txt"
git -C "$seed" add -A
git -C "$seed" commit -q -m "Moved"
git -C "$seed" tag -f -a v1.2.3 -m v1.2.3 > /dev/null
git -C "$seed" push -q -f "$origin" v1.2.3
job_publish
check "publish refuses a tag that moved" rc_fails
check "(the message says where it points now)" out_has "points to"
check "and created no release" state_is none
end_case "tag and version mismatch: wrong version, malformed tags, wrong commit, not on main, tag moved before publish"

# ---- 6. failed build and failed sign ----
make_world 7
git clone -q --depth 1 --branch gh-pages "file://$origin" "$w/pages-empty"
run "$rel/export-flatpak-commits.sh" "$w/pages-empty" "$w/commits-empty"
check "a build that exported nothing fails" rc_is 1
check "(the message says so)" out_has "added no commit"
git clone -q --depth 1 --branch gh-pages "file://$origin" "$w/pages-orphan"
FAKE_OSTREE_ORPHAN=1 ostree --repo="$w/pages-orphan/repo" fake-commit "app/$app_id/x86_64/stable" "orphan build" > /dev/null
run "$rel/export-flatpak-commits.sh" "$w/pages-orphan" "$w/commits-orphan"
check "a commit that does not follow the published one exits 3" rc_is 3
build_jobs
job_sign
check "sign with the right key works" rc_is 0
check "(and leaves no keyring or scratch directory behind)" no_leftovers
check "(a client with only the public key accepted it)" out_has "a client with only the public key accepts"
check "(the GPGKey was compared with the published one)" out_has "byte-identical"
job_sign b
check "sign with a key that is not the committed one is refused" rc_is 2
check "(the message names the problem)" out_has "refusing to sign"
check "(no keyring is left)" no_leftovers
check "(no site was written)" not test -e "$w/signed/site.tar"
sign_with_pubkey b a
check "sign with a public key that is not the secret's is refused" rc_is 2
check "(no keyring is left)" no_leftovers
sign_with_pubkey b b
check "a new key, not the published one, is refused even though it signs" rc_is 2
check "(the message says clients would stop updating)" out_has "differs from the published one"
check "(no keyring is left)" no_leftovers
check "(no site was written)" not test -e "$w/signed/site.tar"
rm -rf "$w/signed"
EXPECT_COMMITS_SHA256=0
export EXPECT_COMMITS_SHA256
use_secret a
run "$rel/sign-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/signed"
drop_secret
check "sign refuses commits that are not the ones the build job reported" rc_is 1
check "(no keyring is left)" no_leftovers
FAKE_FLATPAK_LEAK=1
export FAKE_FLATPAK_LEAK
job_sign
unset FAKE_FLATPAK_LEAK
check "sign refuses to archive a site that holds key material" rc_is 1
check "(the message says so)" out_has "key material found"
check "(no site was written)" not test -e "$w/signed/site.tar"
check "(no keyring is left)" no_leftovers
mkdir "$w/evil"
tar -xf "$w/commits/flatpak-commits.tar" -C "$w/evil"
ln -s /etc/passwd "$w/evil/repo/objects/link"
tar -C "$w/evil" -cf "$w/commits-evil.tar" repo refs.txt
EXPECT_COMMITS_SHA256=$(sha256_of "$w/commits-evil.tar")
export EXPECT_COMMITS_SHA256
use_secret a
run "$rel/sign-flatpak-repo.sh" "$w/commits-evil.tar" "$w/signed"
drop_secret
check "sign refuses a commits archive with a symlink" rc_is 2
check "(the message says plain files)" out_has "not a plain file"
check "(no keyring is left)" no_leftovers
EXPECT_COMMITS_SHA256=$(sha256_of "$w/commits/flatpak-commits.tar")
export EXPECT_COMMITS_SHA256
run "$rel/sign-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/signed"
check "sign without the secret exits 2" rc_is 2
check "(the message names the secret)" out_has "FLATPAK_GPG_PRIVATE_KEY"
check "(no keyring is left)" no_leftovers
git clone -q "file://$origin" "$w/other"
git -C "$w/other" checkout -q gh-pages
echo other > "$w/other/other.txt"
git -C "$w/other" add -A
git -C "$w/other" commit -q -m "Another release"
git -C "$w/other" push -q origin gh-pages
job_sign
check "sign refuses when gh-pages moved after the build (stale base)" rc_is 3
check "(no keyring is left)" no_leftovers
end_case "failed build and failed sign: nothing exported, wrong parent, wrong key, wrong public key, new key, wrong hash, no secret, stale base; keyring always gone"

# ---- 7. an upload fails halfway, then the rerun completes ----
make_world 8
build_jobs
job_validate
FAKE_GH_NO_DIGEST=1
export FAKE_GH_NO_DIGEST
job_verify
job_sign
FAKE_GH_FAIL=upload:KuickShare-x86_64.flatpak
FAKE_GH_PARTIAL=1
export FAKE_GH_FAIL FAKE_GH_PARTIAL
job_publish
check "publish fails when an upload fails" rc_fails
check "the release is still a draft" state_is "draft 3 -"
check "gh-pages was not touched" same "$(pages_head)" "$base_sha"
FAKE_GH_FAIL=
FAKE_GH_PARTIAL=
export FAKE_GH_FAIL FAKE_GH_PARTIAL
job_publish
check "the rerun completes" rc_is 0
check "(it removed the unfinished upload)" out_has "removing the unfinished upload"
check "(it found the finished files by their content)" out_has "already on the release"
check "the release is complete" state_is "published 5 latest"
check "no file was uploaded twice" same "$(gh_count 'upload KuickShare-x86_64.AppImage')$(gh_count 'upload KuickShare-x86_64.AppImage.zsync')$(gh_count 'upload KuickShare-x86_64.flatpak')" 111
end_case "upload fails partway: draft stays, rerun finishes without uploading again"

# ---- 8. an existing asset differs ----
make_world 9
build_jobs
job_verify
job_sign
mkdir -p "$w/bogus"
echo "not the AppImage" > "$w/bogus/KuickShare-x86_64.AppImage"
gh release create "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --draft --verify-tag --title "$RELEASE_TAG" --notes-file "$w/assets/SHA256SUMS"
gh release upload "$RELEASE_TAG" "$w/bogus/KuickShare-x86_64.AppImage" --repo "$GITHUB_REPOSITORY"
job_publish
check "publish refuses to replace a different asset" rc_is 1
check "(the message says so)" out_has "refusing to overwrite"
check "nothing was deleted" not file_has "$FAKE_GH_LOG" "delete-asset"
check "the release is still a draft" state_is "draft 1 -"
check "gh-pages was not touched" same "$(pages_head)" "$base_sha"
end_case "an existing asset with other content: refused, never overwritten"

# ---- 9. released by hand ----
make_world 10
build_jobs
job_verify
job_sign
gh release create "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --verify-tag --title "$RELEASE_TAG" --notes-file "$w/assets/SHA256SUMS"
job_publish
check "publish completes a release that was published by hand" rc_is 0
check "(with a warning)" out_has "warning"
check "the release has all five assets and is the latest" state_is "published 5 latest"
end_case "release published by hand: the missing assets are added, with a warning"

# ---- 10. gh-pages pushed, release publication failed ----
make_world 11
build_jobs
job_verify
job_sign
FAKE_PAGES_STALE=1
export FAKE_PAGES_STALE
job_publish
check "publish fails when Pages does not serve the new repository" rc_fails
check "(the message says the release stays a draft)" out_has "stays a draft"
check "gh-pages has the new site" same "$(pages_file repo/summary)" "$(tar -xOf "$w/signed/site.tar" ./repo/summary)"
pushed=$(pages_head)
FAKE_PAGES_STALE=
FAKE_GH_FAIL=edit
export FAKE_PAGES_STALE FAKE_GH_FAIL
job_publish
check "publish fails when the release cannot be published" rc_fails
check "(the push was skipped on the rerun)" out_has "already has this site"
check "gh-pages is as the first run left it" same "$(pages_head)" "$pushed"
check "the release is still a draft" state_is "draft 5 -"
FAKE_GH_FAIL=
export FAKE_GH_FAIL
job_publish
check "the third run completes" rc_is 0
check "the release is published and the latest" state_is "published 5 latest"
check "gh-pages-previous still holds the old repository" same "$(git -C "$origin" rev-parse refs/heads/gh-pages-previous)" "$base_sha"
end_case "gh-pages pushed but release not published: reruns skip the push and finish"

# ---- 11. the base moved ----
make_world 12
build_jobs
job_verify
job_sign
git clone -q "file://$origin" "$w/other"
git -C "$w/other" checkout -q gh-pages
echo other > "$w/other/other.txt"
git -C "$w/other" add -A
git -C "$w/other" commit -q -m "Another release"
git -C "$w/other" push -q origin gh-pages
moved=$(pages_head)
job_publish
check "publish exits 3 when gh-pages is not the base" rc_is 3
check "(the message says another release got there first)" out_has "another release got there first"
check "gh-pages is untouched" same "$(pages_head)" "$moved"
check "there is no gh-pages-previous" not git -C "$origin" rev-parse --verify -q refs/heads/gh-pages-previous
check "the release is still a draft" state_is "draft 5 -"
end_case "base moved: exit 3, origin untouched, release stays a draft"

# ---- 12. rollback from gh-pages-previous ----
full_release 13
check "the release went through" rc_is 0
git -C "$w/checkout" fetch -q origin gh-pages-previous
git -C "$w/checkout" push -q --force origin FETCH_HEAD:refs/heads/gh-pages
check "gh-pages is the old repository again" same "$(pages_head)" "$base_sha"
check "the old summary is served again" same "$(pages_file repo/summary)" "$(git -C "$site" show HEAD:repo/summary)"
gh release edit "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --draft=true
check "the release can be put back to draft" state_is "draft 5 -"
job_publish
check "running the publish job again repeats the release" rc_is 0
check "(and it is complete)" state_is "published 5 latest"
check "gh-pages has the new site again" same "$(pages_file repo/summary)" "$(tar -xOf "$w/signed/site.tar" ./repo/summary)"
end_case "rollback: gh-pages-previous restores the old repository; the publish job can be run again"

# ---- 13. no secret anywhere ----
leaks=$keys/leaks
: > "$leaks"
for k in a b; do
    cat "$keys/$k/grips" >> "$leaks"
    sed -n '3p' "$keys/$k/secret.asc" >> "$leaks"
done
sed -i '/^$/d' "$leaks"
echo "-----BEGIN PGP PRIVATE KEY BLOCK" >> "$leaks"
check "the key material to look for exists" test "$(wc -l < "$leaks")" -ge 5
check "no private key block, keygrip or key material in any output, log or artifact" not grep -rqaF -f "$leaks" --exclude-dir=keys "$work"
check "no keyring or scratch directory is left in any RUNNER_TEMP" test -z "$(find "$work"/w*/runner-temp -mindepth 1 -maxdepth 1)"
check "the log of everything is not empty" test -s "$work/all.log"
end_case "no private key block, keygrip or key material in outputs, logs, artifacts; no keyring left"

echo "# $passed/$total passed"
[ "$failed" -eq 0 ] && [ "$n" -eq "$total" ]
