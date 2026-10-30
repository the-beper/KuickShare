#!/bin/sh
# Local simulation of the release: runs the real check-version.sh, verify-assets.sh, export-flatpak-commits.sh,
# prepare-flatpak-repo.sh, sign-flatpak-repo.sh, finish-flatpak-repo.sh and publish.sh (and build/flatpak/publish-repo.sh)
# against fixtures, with gh, flatpak, ostree and
# curl replaced by the fakes in build/release/testdata (put first in PATH). The signatures are real GnuPG signatures made
# with ephemeral keys generated in temporary keyrings; no real key, no network, no container.
# Needs only sh, git, gpg 2.2+, python3, tar, sha256sum, sha1sum. Everything happens in a mktemp directory that is removed
# at the end. Output is TAP: "ok N - ..." lines and "# 34/34 passed"; the exit status is not 0 if a case fails.
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
total=34
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
no_leftovers() { [ -z "$(find "$RUNNER_TEMP" -mindepth 1 -maxdepth 1)" ] && [ -z "$(find /dev/shm -maxdepth 1 -name 'kuickshare-gnupg.*' 2> /dev/null)" ]; }

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
    <release version="1.2.2" date="2025-12-01">
      <description>
        <p>Older.</p>
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
    job_prepare
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
job_prepare() { # the keyless job that finishes the repository unsigned
    rm -rf "$w/prepared" "$w/prepare.gho"
    EXPECT_COMMITS_SHA256=$(sha256_of "$w/commits/flatpak-commits.tar")
    EXPECT_REFS_SHA256=$(sha256_of "$w/commits/refs.txt")
    export EXPECT_COMMITS_SHA256 EXPECT_REFS_SHA256
    run env GITHUB_OUTPUT="$w/prepare.gho" "$rel/prepare-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/prepared"
    cp "$work/last.out" "$w/prepare.out"
}
# The jobs sign-flatpak (--check without the secret, then --sign with it) and finish-flatpak, from the files of prepare
# and the hashes it reported. job_sign runs them in a row and stops at the first that fails, with its status in $rc.
out_of() { sed -n "s/^$2=//p" "$1"; } # a value a job wrote to its GITHUB_OUTPUT
job_sign_check() {
    rm -rf "$w/signwork" "$w/signed-repo" "$w/signed" "$w/sign.gho"
    EXPECT_PREPARED_SHA256=$(out_of "$w/prepare.gho" prepared_sha256)
    EXPECT_SIGNLIST_SHA256=$(out_of "$w/prepare.gho" signlist_sha256)
    EXPECT_REFS_SHA256=$(sha256_of "$w/commits/refs.txt")
    EXPECT_BASE_SHA=$(out_of "$w/prepare.gho" base_sha)
    export EXPECT_PREPARED_SHA256 EXPECT_SIGNLIST_SHA256 EXPECT_REFS_SHA256 EXPECT_BASE_SHA
    run "$rel/sign-flatpak-repo.sh" --check "$w/prepared/prepared.tar" "$w/prepared/sign-list.txt" "$w/commits/refs.txt" "$w/signwork"
}
job_sign_key() { # <key name>
    use_secret "${1:-a}"
    run env GITHUB_OUTPUT="$w/sign.gho" "$rel/sign-flatpak-repo.sh" --sign "$w/signwork" "$w/signed-repo"
    drop_secret
}
job_finish() {
    EXPECT_PREPARED_SHA256=$(out_of "$w/prepare.gho" prepared_sha256)
    EXPECT_SIGNLIST_SHA256=$(out_of "$w/prepare.gho" signlist_sha256)
    EXPECT_SIGNED_SHA256=$(out_of "$w/sign.gho" signed_sha256)
    EXPECT_REFS_SHA256=$(sha256_of "$w/commits/refs.txt")
    EXPECT_BASE_SHA=$(out_of "$w/prepare.gho" base_sha)
    export EXPECT_PREPARED_SHA256 EXPECT_SIGNLIST_SHA256 EXPECT_SIGNED_SHA256 EXPECT_REFS_SHA256 EXPECT_BASE_SHA
    run env GITHUB_OUTPUT="$w/finish.gho" "$rel/finish-flatpak-repo.sh" "$w/prepared/prepared.tar" "$w/prepared/sign-list.txt" "$w/signed-repo/signed-repo.tar" "$w/commits/refs.txt" "$w/signed"
}
job_sign() { # <key name>
    job_sign_check
    if [ "$rc" -eq 0 ]; then job_sign_key "${1:-a}"; fi
    if [ "$rc" -eq 0 ]; then job_finish; fi
    cp "$work/last.out" "$w/sign.out"
}
prepared_has() { tar -tf "$w/prepared/prepared.tar" | grep -qx -e "$1"; }
prepared_file() { tar -xOf "$w/prepared/prepared.tar" "$1"; }
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
    export ASSETS_DIR EXPECT_SUMS_SHA256 SITE_TAR EXPECT_SITE_SHA256 SITE_BASE_SHA
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
check "the release notes list the metainfo items" file_has "$FAKE_GH_STATE" "What's Changed\\\\n\\\\n- First change with \`code\`."
check "the release notes link the changelog to the previous tag" file_has "$FAKE_GH_STATE" "Full Changelog\\*\\*: https://github.com/example/KuickShare/compare/v1.2.2\\.\\.\\.v1.2.3"
check "the AppImage was uploaded once" same "$(gh_count 'upload KuickShare-x86_64.AppImage')" 1
check "gh-pages is the new site" same "$(pages_file repo/summary)" "$(tar -xOf "$w/signed/site.tar" ./repo/summary)"
check "the old gh-pages is kept as gh-pages-previous" same "$(git -C "$origin" rev-parse refs/heads/gh-pages-previous)" "$base_sha"
check "prepare kept the GPGKey of the published repository" file_has "$w/prepare.out" "the GPGKey is the published one"
check "the new app commit sits on the published one" same "$(awk -F'\t' '$1 ~ /^app\// { print $3 }' "$w/commits/refs.txt")" "$(ostree --repo="$site/repo" rev-parse "app/$app_id/x86_64/stable")"
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
check "(a client with only the public key accepted it in finish)" out_has "a client with only the public key accepts"
check "(the signed site has the signature of the summary)" tar -tf "$w/signed/site.tar" ./repo/summary.sig > /dev/null
job_sign b
check "sign with a key that is not the committed one is refused" rc_is 2
check "(the message names the problem)" out_has "refusing to sign"
check "(no keyring is left)" no_leftovers
check "(nothing was signed)" not test -e "$w/signed-repo/signed-repo.tar"
sign_with_pubkey b a
check "sign with a public key that is not the secret's is refused" rc_is 2
check "(no keyring is left)" no_leftovers
sign_with_pubkey b b
check "a new key, not the published one, signs, and finish refuses the site" rc_is 1
check "(the message says the GPGKey is not the key)" out_has "byte for byte"
check "(no keyring is left)" no_leftovers
check "(no site was written)" not test -e "$w/signed/site.tar"
FAKE_FLATPAK_LEAK=1
export FAKE_FLATPAK_LEAK
job_sign
unset FAKE_FLATPAK_LEAK
check "sign refuses to archive a repository that holds key material" rc_is 1
check "(the message says so)" out_has "key material found"
check "(nothing was signed)" not test -e "$w/signed-repo/signed-repo.tar"
check "(no keyring is left)" no_leftovers
job_sign_check
run "$rel/sign-flatpak-repo.sh" --sign "$w/signwork" "$w/signed-repo"
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
check "sign refuses when gh-pages moved since the build (stale base)" rc_is 3
check "(no keyring is left)" no_leftovers
end_case "failed build and failed sign: nothing exported, wrong parent, wrong key, wrong public key, new key, key material, no secret, stale base; keyring always gone"

# ---- 6e. review findings: config, bindings, unlisted refs, tampered base ----
add_published_ref() { # an extra ref on gh-pages with a parent, signed with key A, summary made again (as an earlier release would)
    extra=app/other.app/x86_64/stable
    git clone -q "file://$origin" "$w/pubx"
    git -C "$w/pubx" checkout -q gh-pages
    mkdir -p "$w/pubx/repo/refs/heads" "$w/pubx/repo/refs/mirrors" "$w/pubx/repo/refs/remotes" "$w/pubx/repo/tmp"
    extra_parent=$(ostree --repo="$w/pubx/repo" fake-commit "$extra" "other 1")
    extra_head=$(ostree --repo="$w/pubx/repo" fake-commit "$extra" "other 2")
    ostree --repo="$w/pubx/repo" gpg-sign --gpg-homedir="$keys/a" "$extra_parent" "$(cat "$keys/a/fpr")"
    ostree --repo="$w/pubx/repo" gpg-sign --gpg-homedir="$keys/a" "$extra_head" "$(cat "$keys/a/fpr")"
    GNUPGHOME=$keys/a GPG_KEY_ID=$(cat "$keys/a/fpr") "$repo_root/build/flatpak/publish-repo.sh" "$w/pubx" "$SITE_URL"
    git -C "$w/pubx" add -A
    git -C "$w/pubx" commit -q -m "Another app"
    git -C "$w/pubx" push -q origin gh-pages
    base_sha=$(git -C "$w/pubx" rev-parse HEAD)
}
rehash_prepared() { sed -i '/^prepared_sha256=/d; /^signlist_sha256=/d' "$w/prepare.gho"; { echo "prepared_sha256=$(sha256_of "$w/prepared/prepared.tar")"; echo "signlist_sha256=$(sha256_of "$w/prepared/sign-list.txt")"; } >> "$w/prepare.gho"; }
tamper_prepared() { # <shell command run in the unpacked site>: a compromised prepare job
    rm -rf "$w/x"
    mkdir "$w/x"
    tar -xf "$w/prepared.orig/prepared.tar" -C "$w/x"
    (cd "$w/x/site/repo" && eval "$1")
    tar -C "$w/x" -cf "$w/prepared/prepared.tar" site
    rehash_prepared
}
make_world 24
add_published_ref
build_jobs
check "prepare accepts a published repository with another app in it" rc_is 0
check "(and verified the published repository with the pinned key)" file_has "$w/prepare.out" "verify with the pinned key"
check "the other app is not on the sign list" not file_has "$w/prepared/sign-list.txt" "other.app"
cp -a "$w/prepared" "$w/prepared.orig"
cp "$w/prepare.gho" "$w/prepare.gho.orig"
app_ref=app/$app_id/x86_64/stable
app_commit=$(awk -F'\t' -v r="$app_ref" '$1 == r { print $2 }' "$w/commits/refs.txt")
restore_prepared() { rm -rf "$w/prepared"; cp -a "$w/prepared.orig" "$w/prepared"; cp "$w/prepare.gho.orig" "$w/prepare.gho"; }
objfile() { printf 'objects/%s/%s.commit' "$(printf '%s' "$1" | cut -c1-2)" "${1#??}"; }

job_sign
check "an honest run goes through" rc_is 0
# config
tamper_prepared 'printf "[remote \"evil\"]\nurl=file:///etc\ngpg-verify=false\n" >> config'
job_sign_check
check "sign --check refuses a config with a remote" rc_is 2
check "(the message says so)" out_has "config of the repository"
restore_prepared
tamper_prepared 'echo "gpg-verify=false" >> config'
job_sign_check
check "sign --check refuses a config with another key" rc_is 2
restore_prepared
# the old summary is gone before the key is used
job_sign_check
check "the old summary files are deleted in --check" not test -e "$w/signwork/site/repo/summary"
check "(and the summaries folder)" not test -e "$w/signwork/site/repo/summaries"
# binding of a listed commit
tamper_prepared "sed -i 's|^binding=.*|binding=app/other.app/x86_64/stable|' $(objfile "$app_commit")"
job_sign_check
check "sign --check refuses a listed commit bound to another ref" rc_is 1
check "(the message says so)" out_has "not bound to its own ref"
restore_prepared
tamper_prepared "sed -i '/^binding=/d' $(objfile "$app_commit")"
job_sign_check
check "sign --check refuses a listed commit with no binding" rc_is 1
restore_prepared
# a signature already on a listed commit
tamper_prepared "touch $(objfile "$app_commit" | sed 's/\.commit$/.commitmeta/')"
job_sign_check
check "sign --check refuses a listed commit that has a signature" rc_is 1
restore_prepared
# an unlisted ref rolled back or dropped, which a compromised prepare job would do and the new summary would vouch for
tamper_prepared "printf '%s\n' $extra_parent > refs/heads/$extra"
job_sign
check "sign --check refuses a ref that was rolled back, though it is not on the list" rc_is 1
check "(the message says so)" out_has "not on the sign list"
check "(nothing was signed)" not test -e "$w/signed-repo/signed-repo.tar"
restore_prepared
tamper_prepared "mkdir -p refs/heads/app/new.app/x86_64; printf '%s\n' $extra_head > refs/heads/app/new.app/x86_64/stable"
job_sign
check "sign --check refuses a ref that is new and not on the list" rc_is 1
restore_prepared
# the appstream commit continues the published one, or the key does not sign
tamper_prepared "FAKE_OSTREE_ORPHAN=1 ostree --repo=. fake-commit appstream/x86_64 evil > ../../newappstream"
sed -i "s/^\(appstream\/x86_64\t\).*/\1$(cat "$w/x/newappstream")/" "$w/prepared/sign-list.txt"
rehash_prepared
job_sign
check "sign --check refuses an appstream commit that does not continue the published one" rc_is 1
check "(the message says so)" out_has "does not continue the published commit"
restore_prepared
tamper_prepared "mkdir deltas; echo x > deltas/x"
job_sign
check "sign --check refuses static deltas" rc_is 1
check "(the message says so)" out_has "static deltas"
restore_prepared
tamper_prepared "touch 'we\\ird'"
job_sign
check "sign --check refuses a member name with a backslash" rc_is 2
check "(the message says so)" out_has "characters outside"
restore_prepared
tamper_prepared "echo '# changed' >> ../kuickshare.flatpakref"
job_sign
check "finish refuses a changed file outside repo/" rc_is 1
check "(the message says so)" out_has "outside repo/"
restore_prepared
tamper_prepared "echo x > ../extra.html"
job_sign
check "finish refuses an extra file outside repo/" rc_is 1
restore_prepared
tamper_prepared "rm refs/heads/$extra"
job_sign
check "finish refuses a ref that was dropped" rc_is 1
check "(the message says so)" out_has "is missing in the repository"
restore_prepared
# refs that `ostree refs` does not list, and a new ref whose name matches a published one only as a pattern
tamper_prepared "mkdir -p refs/mirrors/org.example; printf '%s\n' $extra_head > refs/mirrors/org.example/x"
job_sign
check "sign --check refuses a ref under refs/mirrors" rc_is 1
check "(the message says so)" out_has "refs/mirrors"
restore_prepared
# app.other.app/x86_64/stable: as a regex it matches the published app/other.app/x86_64/stable
dotted=$(printf '%s' "$extra" | sed 's|^app/|app.|')
tamper_prepared "mkdir -p refs/heads/$(dirname "$dotted"); printf '%s\n' $extra_head > refs/heads/$dotted"
job_sign
check "sign --check refuses a new ref that matches a published one only as a pattern" rc_is 1
check "(the message says so)" out_has "not on the sign list"
restore_prepared
# a listed commit with another binding, after the signature step
job_sign_check
job_sign_key
job_finish
check "finish accepts the honest signed repository" rc_is 0
rm -rf "$w/y"
mkdir "$w/y"
tar -xf "$w/signed-repo/signed-repo.tar" -C "$w/y"
sed -i 's|^binding=.*|binding=app/other.app/x86_64/stable|' "$w/y/repo/$(objfile "$app_commit")"
tar -C "$w/y" -cf "$w/signed-repo/signed-repo.tar" repo
sed -i '/^signed_sha256=/d' "$w/sign.gho"
echo "signed_sha256=$(sha256_of "$w/signed-repo/signed-repo.tar")" >> "$w/sign.gho"
rm -rf "$w/signed"
job_finish
check "finish refuses a listed commit bound to another ref" rc_is 1
check "(the message says so)" out_has "not bound to its own ref"
check "(no site was written)" not test -e "$w/signed/site.tar"
end_case "review findings: config, bindings, a signature already there, rolled-back and dropped unlisted refs"

make_world 25
git clone -q "file://$origin" "$w/pubx"
git -C "$w/pubx" checkout -q gh-pages
echo tampered >> "$w/pubx/repo/summary"
git -C "$w/pubx" commit -q -a -m "Tampered"
git -C "$w/pubx" push -q origin gh-pages
base_sha=$(git -C "$w/pubx" rev-parse HEAD)
build_jobs
check "prepare refuses a published summary that the pinned key did not sign" rc_is 2
check "(the message says so)" out_has "does not verify with the pinned key"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
end_case "tampered gh-pages: the base is verified with the pinned key before anything is built on it"

# ---- 6g. gh-pages with refs/heads moved and the old signed summary kept ----
make_world 26
git clone -q "file://$origin" "$w/pubx"
git -C "$w/pubx" checkout -q gh-pages
mkdir -p "$w/pubx/repo/refs/heads" "$w/pubx/repo/refs/mirrors" "$w/pubx/repo/refs/remotes" "$w/pubx/repo/tmp"
evil=$(ostree --repo="$w/pubx/repo" fake-commit "app/$app_id/x86_64/stable" "evil")
ostree --repo="$w/pubx/repo" gpg-sign --gpg-homedir="$keys/a" "$evil" "$(cat "$keys/a/fpr")"
git -C "$w/pubx" add -A
git -C "$w/pubx" commit -q -m "Moved a ref, summary unchanged"
git -C "$w/pubx" push -q origin gh-pages
base_sha=$(git -C "$w/pubx" rev-parse HEAD)
build_jobs
check "prepare refuses a gh-pages whose refs are not those of its signed summary" rc_is 2
check "(the message says so)" out_has "not the refs and commits of its signed summary"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
end_case "gh-pages with a ref moved under a valid signed summary: refused"

# ---- 6h. gh-pages with a link under refs/mirrors ----
# git keeps symbolic links; `ostree refs` and a search for plain files do not see one. (In a tar the link is refused by tar_safe,
# so only prepare, which works on the clone itself, depends on the check of ref_map.)
make_world 27
git clone -q "file://$origin" "$w/pubx"
git -C "$w/pubx" checkout -q gh-pages
mkdir -p "$w/pubx/repo/refs/mirrors/org.example"
ln -s "../../heads/app/$app_id/x86_64/stable" "$w/pubx/repo/refs/mirrors/org.example/x"
git -C "$w/pubx" add -A
git -C "$w/pubx" commit -q -m "A link under refs/mirrors"
git -C "$w/pubx" push -q origin gh-pages
base_sha=$(git -C "$w/pubx" rev-parse HEAD)
build_jobs
check "prepare refuses a gh-pages with a link under refs/mirrors" rc_is 1
check "(the message says so)" out_has "refs/mirrors"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
end_case "gh-pages with a symbolic link under refs/mirrors: refused by the check itself, not by a later tar check"

# ---- 6i. the same key (fingerprint) with other bytes ----
# A renewed expiry or an added user id: the fingerprint is the same, the bytes clients imported are not.
make_world 28
mkdir -m 700 "$keys/a2"
cp "$keys/a/pubring.kbx" "$keys/a2/"
cp -r "$keys/a/private-keys-v1.d" "$keys/a2/"
GNUPGHOME=$keys/a2 gpg --batch --pinentry-mode loopback --passphrase '' --quick-add-uid "$(cat "$keys/a/fpr")" "KuickShare second user id" > "$keys/a2/uid.log" 2>&1
GNUPGHOME=$keys/a2 gpg --batch --armor --export "$(cat "$keys/a/fpr")" > "$keys/a2/pub.asc" 2>> "$keys/a2/uid.log"
check "the changed key has the fingerprint of key A" same "$(GNUPGHOME=$keys/a2 gpg --batch --show-keys --with-colons "$keys/a2/pub.asc" 2> /dev/null | awk -F: '$1 == "fpr" { print $10; exit }')" "$(cat "$keys/a/fpr")"
check "(and other bytes)" not cmp -s "$keys/a/pub.asc" "$keys/a2/pub.asc"
build_jobs
SIGNING_PUBKEY=$keys/a2/pub.asc
job_prepare
SIGNING_PUBKEY=$keys/a/pub.asc
check "prepare refuses a public key file with the published fingerprint and other bytes" rc_is 2
check "(the message says byte for byte)" out_has "byte for byte"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
# the same, one job later: finish compares the site files with the bytes of its own public key file
job_prepare
job_sign_check
job_sign_key a
SIGNING_PUBKEY=$keys/a2/pub.asc
job_finish
SIGNING_PUBKEY=$keys/a/pub.asc
check "finish refuses a public key file with the same fingerprint and other bytes than the site" rc_is 1
check "(the message says byte for byte)" out_has "byte for byte"
check "(no site was written)" not test -e "$w/signed/site.tar"
# what CONTRIBUTING.md (Releases) tells to do: put the new bytes on gh-pages by hand, then the release goes through
git clone -q "file://$origin" "$w/pubx"
git -C "$w/pubx" checkout -q gh-pages
sed -i "s|^GPGKey=.*|GPGKey=$(gpg --batch --dearmor < "$keys/a2/pub.asc" | base64 -w0)|" "$w/pubx/kuickshare.flatpakrepo" "$w/pubx/kuickshare.flatpakref"
git -C "$w/pubx" commit -q -a -m "The renewed key"
git -C "$w/pubx" push -q origin gh-pages
base_sha=$(git -C "$w/pubx" rev-parse HEAD)
SIGNING_PUBKEY=$keys/a2/pub.asc
build_jobs
job_sign a
SIGNING_PUBKEY=$keys/a/pub.asc
check "with the new bytes on gh-pages, prepare, sign and finish go through" rc_is 0
check "(and the site has the new bytes)" same "$(tar -xOf "$w/signed/site.tar" ./kuickshare.flatpakrepo | sed -n 's/^GPGKey=//p')" "$(gpg --batch --dearmor < "$keys/a2/pub.asc" | base64 -w0)"
end_case "same fingerprint, other key bytes: prepare and finish refuse until gh-pages carries the new bytes"

# ---- 6j. an empty variable is said to be empty ----
make_world 29
mkdir "$w/s"
run env -u GPG_KEY_ID GPG_KEY_B64= "$repo_root/build/flatpak/publish-repo.sh" "$w/s" "$SITE_URL"
check "publish-repo.sh refuses an empty GPG_KEY_B64" rc_fails
check "(and says it is empty or not set)" out_has "GPG_KEY_B64 is empty or not set"
run env -u GPG_KEY_B64 "$repo_root/build/flatpak/write-site-files.sh" "$w/s" "$SITE_URL"
check "write-site-files.sh refuses a GPG_KEY_B64 that is not set" rc_fails
check "(and says it is empty or not set)" out_has "GPG_KEY_B64 is empty or not set"
check "(nothing was written)" test -z "$(find "$w/s" -mindepth 1)"
build_jobs
job_sign_check
run env FLATPAK_GPG_PRIVATE_KEY= "$rel/sign-flatpak-repo.sh" --sign "$w/signwork" "$w/signed-repo"
check "sign --sign refuses an empty secret" rc_is 2
check "(and says it is empty or not available)" out_has "is empty or not available"
check "(no scratch directory is left)" no_leftovers
end_case "empty GPG_KEY_B64 and empty secret: refused, and the message says empty"

# ---- 6k. review findings of PaletoModkit: dates, unlisted heads, appstream commits, top of the repository, rolled-back gh-pages ----
snapshot_prepared() { rm -rf "$w/prepared.orig"; cp -a "$w/prepared" "$w/prepared.orig"; cp "$w/prepare.gho" "$w/prepare.gho.orig"; }
tamper_signed() { # <shell command run in the unpacked signed repository>: a compromised sign job
    rm -rf "$w/y"
    mkdir "$w/y"
    tar -xf "$w/signed-repo/signed-repo.tar" -C "$w/y"
    (cd "$w/y/repo" && eval "$1")
    tar -C "$w/y" -cf "$w/signed-repo/signed-repo.tar" repo
    sed -i '/^signed_sha256=/d' "$w/sign.gho"
    echo "signed_sha256=$(sha256_of "$w/signed-repo/signed-repo.tar")" >> "$w/sign.gho"
    rm -rf "$w/signed"
}
list_commit() { awk -F'\t' -v r="$1" '$1 == r { print $2 }' "$w/prepared.orig/sign-list.txt"; }

make_world 30
build_jobs
snapshot_prepared
app_ref=app/$app_id/x86_64/stable
app_commit=$(list_commit "$app_ref")
now=$(date +%s)
tamper_prepared "sed -i 's|^date=.*|date=$((now + 7200))|' $(objfile "$app_commit")"
job_sign_check
check "sign --check refuses a new commit dated two hours ahead" rc_is 1
check "(the message says so)" out_has "is dated more than 3600s after now"
restore_prepared
tamper_prepared "sed -i 's|^date=.*|date=$((now + 1800))|' $(objfile "$app_commit")"
job_sign_check
check "sign --check accepts a new commit dated half an hour ahead (clock skew)" rc_is 0
restore_prepared
tamper_prepared "sed -i 's|^date=.*|date=1000|' $(objfile "$app_commit")"
job_sign_check
check "sign --check refuses a new commit older than the published head" rc_is 1
check "(the message says so)" out_has "is older than the published head"
restore_prepared
tamper_prepared "sed -i 's|^date=.*|date=$((now + 7200))|' $(objfile "$(list_commit appstream/x86_64)")"
job_sign_check
check "sign --check refuses a new appstream commit dated two hours ahead" rc_is 1
restore_prepared
job_sign_check
job_sign_key a
tamper_signed "sed -i 's|^date=.*|date=$((now + 7200))|' $(objfile "$app_commit")"
job_finish
check "finish refuses a signed commit dated two hours ahead" rc_is 1
check "(the message says so)" out_has "is dated more than 3600s after now"
end_case "commit dates: a new commit is not dated far ahead of now or before the head it continues, in sign --check and finish"

make_world 31
build_jobs
snapshot_prepared
as1=$(list_commit appstream/x86_64)
as2=$(list_commit appstream2/x86_64)
job_sign
check "an honest run goes through, with appstream commits of the expected shape" rc_is 0
tamper_prepared "echo 'keys=ostree.collection-binding ostree.ref-binding xa.ref' >> $(objfile "$as1")"
job_sign_check
check "sign --check refuses an appstream commit with one more metadata key" rc_is 1
check "(the message says so)" out_has "other metadata keys"
restore_prepared
tamper_prepared "echo 'keys=ostree.ref-binding ostree.ref-binding ostree.collection-binding' >> $(objfile "$as1")"
job_sign_check
check "sign --check refuses an appstream commit with a key twice" rc_is 1
restore_prepared
tamper_prepared "echo 'keys=ostree.ref-binding' >> $(objfile "$as2")"
job_sign_check
check "sign --check refuses an appstream2 commit without the collection binding" rc_is 1
restore_prepared
tamper_prepared "echo 'subject=Something else' >> $(objfile "$as2")"
job_sign_check
check "sign --check refuses an appstream commit with another subject" rc_is 1
check "(the message says so)" out_has "another subject or a body"
restore_prepared
tamper_prepared "echo 'message=a body' >> $(objfile "$as1")"
job_sign_check
check "sign --check refuses an appstream commit with a body" rc_is 1
restore_prepared
job_sign_check
job_sign_key a
tamper_signed "echo 'subject=Something else' >> $(objfile "$as1")"
job_finish
check "finish refuses a signed appstream commit with another subject" rc_is 1
end_case "appstream commits: exact metadata keys (sorted, duplicates count), subject Update, no body, in sign --check and finish"

make_world 32
add_published_ref
build_jobs
snapshot_prepared
tamper_prepared "echo '# changed' >> $(objfile "$extra_head")"
job_sign_check
check "sign --check refuses an unchanged head whose commit file differs from gh-pages" rc_is 1
check "(the message says so)" out_has "differs from gh-pages"
restore_prepared
tamper_prepared "printf x >> $(objfile "$extra_head" | sed 's/commit$/commitmeta/')"
job_sign_check
check "sign --check refuses an unchanged head whose signature file differs from gh-pages" rc_is 1
check "(the message says so)" out_has "differs from gh-pages"
restore_prepared
job_sign_check
check "the honest repository is accepted" rc_is 0
end_case "unchanged heads: their commit and signature files are those of gh-pages"

make_world 33
build_jobs
snapshot_prepared
tamper_prepared "echo x > stray.txt"
job_sign_check
check "sign --check refuses a file at the top of repo/ that ostree ignores" rc_is 1
check "(the message names it)" out_has "stray.txt"
restore_prepared
tamper_prepared "mkdir old-site"
job_sign_check
check "sign --check refuses a directory at the top of repo/ that ostree does not know" rc_is 1
restore_prepared
job_sign
check "the honest run goes through (config objects refs summary... are allowed)" rc_is 0
end_case "repo/ has only the entries of an ostree repository at its top"

fake_release() { # <tag> <draft true|false> <name of the one asset, or - for none>: a release in the fake gh
    python3 -I - "$FAKE_GH_STATE" "$1" "$2" "$3" << 'PY'
import json
import sys

path, tag, draft, asset = sys.argv[1:5]
with open(path) as f:
    state = json.load(f)
state["releases"] = [r for r in state["releases"] if r["tag_name"] != tag]
state["releases"].append({"id": 900 + len(state["releases"]), "tag_name": tag, "name": tag, "draft": draft == "true", "body": "",
                          "assets": [] if asset == "-" else [{"id": 800, "name": asset, "size": 1, "state": "uploaded"}]})
with open(path, "w") as f:
    json.dump(state, f)
PY
}
make_world 34
build_jobs
check "prepare accepts a gh-pages when no release is published" rc_is 0
fake_release v1.2.2 false KuickShare-x86_64.flatpak
job_prepare
check "prepare accepts a gh-pages at the newest published release" rc_is 0
check "(and says which)" out_has "the newest published release is v1.2.2"
fake_release v1.2.4 false KuickShare-x86_64.flatpak
job_prepare
check "prepare refuses a gh-pages older than the newest published release" rc_is 1
check "(the message says it was rolled back)" out_has "rolled back or never updated"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
fake_release v1.2.4 false -
job_prepare
check "prepare ignores a published release without files (made by hand, never on Pages)" rc_is 0
fake_release v1.2.4 true KuickShare-x86_64.flatpak
job_prepare
check "prepare ignores a draft" rc_is 0
fake_release v1.2.4 false KuickShare-x86_64.flatpak
# the old record of gh-pages is what a pusher writes: the subject of the head commit says it is current
git clone -q "file://$origin" "$w/pubx"
git -C "$w/pubx" checkout -q gh-pages
git -C "$w/pubx" commit -q --allow-empty -m "Flatpak repository for v1.2.4"
git -C "$w/pubx" push -q origin gh-pages
base_sha=$(git -C "$w/pubx" rev-parse HEAD)
build_jobs
check "prepare refuses a rolled-back gh-pages whose head commit says it is current" rc_is 1
check "(the version comes from the signed app: the message names it)" out_has "the app of gh-pages is at 1.2.2, but v1.2.4 is published"
fake_release v1.2.4 false -
job_prepare
check "(without the bundle on the release it is accepted again)" rc_is 0
# GH_TOKEN is for gh alone
: > "$FAKE_OSTREE_LOG"
: > "$FAKE_FLATPAK_LOG"
fake_release v1.2.2 false KuickShare-x86_64.flatpak
job_prepare
check "prepare lists the releases with the token" rc_is 0
check "(ostree never saw the token)" not file_has "$FAKE_OSTREE_LOG" "SAW GH_TOKEN"
check "(flatpak never saw it)" not file_has "$FAKE_FLATPAK_LOG" "SAW GH_TOKEN"
# the first publication: no gh-pages, so no release may be on Pages already
make_world 36
git -C "$origin" update-ref -d refs/heads/gh-pages
rm -rf "$w/pages" "$w/commits" "$w/dist"
mkdir "$w/pages"
ostree --repo="$w/pages/repo" fake-commit "app/$app_id/x86_64/stable" "first 1.2.3" > /dev/null
ostree --repo="$w/pages/repo" fake-commit "runtime/$app_id.Locale/x86_64/stable" "first locale 1.2.3" > /dev/null
run "$rel/export-flatpak-commits.sh" "$w/pages" "$w/commits"
fake_release v1.2.2 false KuickShare-x86_64.flatpak
job_prepare
check "prepare refuses a first publication when a release with the bundle exists" rc_is 1
check "(the message says gh-pages is gone)" out_has "there is no gh-pages, but v1.2.2 is published"
fake_release v1.2.2 false -
job_prepare
check "(a release without files does not stop the first publication)" rc_is 0
end_case "a gh-pages older than the newest published release (with files) is refused, a draft or a release without files is no anchor"

# ---- 6p. a published repository from the time of static deltas ----
# The real gh-pages has deltas/ and delta-indexes/ (names with a plus sign, as base64 names have). The release has none.
make_world 35
git clone -q "file://$origin" "$w/pubx"
git -C "$w/pubx" checkout -q gh-pages
mkdir -p "$w/pubx/repo/deltas/ab/cd+ef-gh_ij/0" "$w/pubx/repo/delta-indexes/ab"
echo x > "$w/pubx/repo/deltas/ab/cd+ef-gh_ij/superblock"
echo x > "$w/pubx/repo/deltas/ab/cd+ef-gh_ij/0/part"
echo x > "$w/pubx/repo/delta-indexes/ab/cd+ef.index"
git -C "$w/pubx" add -A
git -C "$w/pubx" commit -q -m "Static deltas of an earlier release"
git -C "$w/pubx" push -q origin gh-pages
base_sha=$(git -C "$w/pubx" rev-parse HEAD)
build_jobs
job_sign
check "a release goes through on a gh-pages that has static deltas and delta indexes" rc_is 0
check "(the site has neither)" test -z "$(tar -tf "$w/signed/site.tar" | grep -i delta)"
end_case "a base with static deltas and delta indexes (names with a plus sign): both are removed before anything checks the repository"

# ---- 6b. prepare: the keyless job that finishes the repository unsigned ----
make_world 20
build_jobs
job_prepare
check "prepare exits 0" rc_is 0
check "(and leaves no scratch directory behind)" no_leftovers
check "it writes the prepared site and the list" test -s "$w/prepared/prepared.tar" -a -s "$w/prepared/sign-list.txt"
check "it reports the hashes and the base" file_has "$w/prepare.gho" "^prepared_sha256=$(sha256_of "$w/prepared/prepared.tar")$"
check "(the hash of the list too)" file_has "$w/prepare.gho" "^signlist_sha256=$(sha256_of "$w/prepared/sign-list.txt")$"
check "(and the base)" file_has "$w/prepare.gho" "^base_sha=$base_sha$"
check "the list has the two refs of the build and the two appstream refs" same "$(cut -f1 "$w/prepared/sign-list.txt" | tr '\n' ' ')" "app/$app_id/x86_64/stable appstream/x86_64 appstream2/x86_64 runtime/$app_id.Locale/x86_64/stable "
check "the commits of the build are the listed ones" same "$(grep -c "^app/.*	$(awk -F'\t' '$1 ~ /^app\// { print $2 }' "$w/commits/refs.txt")$" "$w/prepared/sign-list.txt")" 1
check "the prepared site has the repository and the files users add it with" prepared_has "site/repo/summary"
check "(the .flatpakref too)" prepared_has "site/kuickshare.flatpakref"
check "the git metadata of the clone is not in it" not prepared_has "site/.git/HEAD"
check "the summary is not signed yet" not prepared_has "site/repo/summary.sig"
check "the new commits are not signed" not prepared_has "site/repo/objects/$(awk -F'\t' '$1 ~ /^app\// { print $2 }' "$w/commits/refs.txt").commitmeta"
check "the GPGKey is byte-identical to the published one" same "$(prepared_file site/kuickshare.flatpakrepo | sed -n 's/^GPGKey=//p')" "$(sed -n 's/^GPGKey=//p' "$site/kuickshare.flatpakrepo")"
check "(the log says so)" out_has "the GPGKey is the published one"
# a secret in the environment: refused before anything is read
use_secret a
rm -rf "$w/prepared"
run "$rel/prepare-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/prepared"
drop_secret
check "prepare refuses to run with the secret in its environment" rc_is 2
check "(the message says so)" out_has "must run without the secret"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
check "(no scratch directory is left)" no_leftovers
# changed inputs
job_prepare
saved=$EXPECT_COMMITS_SHA256
EXPECT_COMMITS_SHA256=0
run env GITHUB_OUTPUT="$w/prepare.gho" "$rel/prepare-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/prepared2"
check "prepare refuses commits that are not the ones the build job reported" rc_is 1
EXPECT_COMMITS_SHA256=$saved
EXPECT_REFS_SHA256=0
run env GITHUB_OUTPUT="$w/prepare.gho" "$rel/prepare-flatpak-repo.sh" "$w/commits/flatpak-commits.tar" "$w/prepared2"
check "prepare refuses a refs.txt that is not the one the build job reported" rc_is 1
check "(the message names it)" out_has "refs.txt of the commits archive"
EXPECT_REFS_SHA256=$(sha256_of "$w/commits/refs.txt")
mkdir "$w/evil"
tar -xf "$w/commits/flatpak-commits.tar" -C "$w/evil"
ln -s /etc/passwd "$w/evil/repo/objects/link"
tar -C "$w/evil" -cf "$w/commits-evil.tar" repo refs.txt
EXPECT_COMMITS_SHA256=$(sha256_of "$w/commits-evil.tar")
run env GITHUB_OUTPUT="$w/prepare.gho" "$rel/prepare-flatpak-repo.sh" "$w/commits-evil.tar" "$w/prepared2"
check "prepare refuses a commits archive with a symlink" rc_is 2
check "(the message says plain files)" out_has "not a plain file"
rm -f "$w/evil/repo/objects/link"
echo surprise > "$w/evil/notes.txt"
tar -C "$w/evil" -cf "$w/commits-evil.tar" repo refs.txt notes.txt
EXPECT_COMMITS_SHA256=$(sha256_of "$w/commits-evil.tar")
run env GITHUB_OUTPUT="$w/prepare.gho" "$rel/prepare-flatpak-repo.sh" "$w/commits-evil.tar" "$w/prepared2"
check "prepare refuses a commits archive with other files" rc_is 2
check "(no scratch directory is left)" no_leftovers
EXPECT_COMMITS_SHA256=$saved
export EXPECT_COMMITS_SHA256 EXPECT_REFS_SHA256
SIGNING_PUBKEY=$keys/b/pub.asc
job_prepare
SIGNING_PUBKEY=$keys/a/pub.asc
check "prepare refuses a published GPGKey that is not the key of the public key file" rc_is 2
check "(the message says clients would stop updating)" out_has "installed clients would stop updating"
check "(nothing was written)" not test -e "$w/prepared/prepared.tar"
with_sha 0000000000000000000000000000000000000001 job_prepare
check "prepare refuses a checkout that is not the commit of the run" rc_is 1
git clone -q "file://$origin" "$w/other"
git -C "$w/other" checkout -q gh-pages
echo other > "$w/other/other.txt"
git -C "$w/other" add -A
git -C "$w/other" commit -q -m "Another release"
git -C "$w/other" push -q origin gh-pages
job_prepare
check "prepare refuses when gh-pages moved after the build (stale base)" rc_is 3
check "(no scratch directory is left)" no_leftovers
# the first release: no gh-pages yet, so the GPGKey is made from the public key file
make_world 21
git -C "$origin" update-ref -d refs/heads/gh-pages
rm -rf "$w/pages" "$w/commits" "$w/dist"
mkdir "$w/pages"
ostree --repo="$w/pages/repo" fake-commit "app/$app_id/x86_64/stable" "first 1.2.3" > /dev/null
ostree --repo="$w/pages/repo" fake-commit "runtime/$app_id.Locale/x86_64/stable" "first locale 1.2.3" > /dev/null
run "$rel/export-flatpak-commits.sh" "$w/pages" "$w/commits"
job_prepare
check "prepare accepts the first release" rc_is 0
check "(and reports no base)" file_has "$w/prepare.gho" "^base_sha=-$"
check "(it says the GPGKey is made from the public key file)" out_has "there was no published .flatpakrepo"
check "the GPGKey is the key of the public key file" same "$(prepared_file site/kuickshare.flatpakrepo | sed -n 's/^GPGKey=//p' | base64 -d | gpg --batch --show-keys --with-colons 2> /dev/null | awk -F: '$1 == "fpr" { print $10; exit }')" "$(cat "$keys/a/fpr")"
check "(and the .flatpakref has it too)" same "$(prepared_file site/kuickshare.flatpakref | sed -n 's/^GPGKey=//p')" "$(prepared_file site/kuickshare.flatpakrepo | sed -n 's/^GPGKey=//p')"
end_case "prepare: unsigned site and list, hashes, GPGKey kept; refuses the secret, wrong hashes, links, other files, another key, stale base"

# ---- 6c. the secret reaches nothing but the sign step; tampered inputs are refused ----
make_world 22
build_jobs
cp -a "$w/prepared" "$w/prepared.orig"
cp "$w/prepare.gho" "$w/prepare.gho.orig"
restore_prepared() { rm -rf "$w/prepared"; cp -a "$w/prepared.orig" "$w/prepared"; cp "$w/prepare.gho.orig" "$w/prepare.gho"; }
rehash_prepared() { # a compromised prepare job would report the hashes of what it made
    sed -i '/^prepared_sha256=/d; /^signlist_sha256=/d' "$w/prepare.gho"
    { echo "prepared_sha256=$(sha256_of "$w/prepared/prepared.tar")"; echo "signlist_sha256=$(sha256_of "$w/prepared/sign-list.txt")"; } >> "$w/prepare.gho"
}
unpack_prepared() { rm -rf "$w/x"; mkdir "$w/x"; tar -xf "$w/prepared.orig/prepared.tar" -C "$w/x"; }
pack_prepared() { tar -C "$w/x" -cf "$w/prepared/prepared.tar" site; }
app_ref=app/$app_id/x86_64/stable
app_commit=$(awk -F'\t' -v r="$app_ref" '$1 == r { print $2 }' "$w/commits/refs.txt")
locale_commit=$(awk -F'\t' '$1 ~ /^runtime\// { print $2 }' "$w/commits/refs.txt")

# the secret in the environment of the steps that read data of other jobs
use_secret a
job_sign_check
drop_secret
check "sign --check refuses to run with the secret in its environment" rc_is 2
check "(the message says so)" out_has "must run without the secret"
check "(nothing was unpacked)" not test -e "$w/signwork/sign-list.txt"

# no child process of the sign step sees the secret, and the step does what it should and no more
rt=$work/shim
mkdir -p "$rt/bin"
for t in flatpak gpg ostree git; do
    real=$(command -v "$t")
    # shellcheck disable=SC2016 # the shim expands its own variables
    printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\nprintenv FLATPAK_GPG_PRIVATE_KEY >> "%s/seen" || true\nexec %s "$@"\n' "$t" "$rt" "$rt" "$real" > "$rt/bin/$t"
    chmod +x "$rt/bin/$t"
done
with_shims() { saved_path=$PATH; PATH=$rt/bin:$PATH; export PATH; "$@"; PATH=$saved_path; export PATH; }
: > "$rt/seen"
: > "$rt/calls"
with_shims job_sign_check
check "sign --check passes" rc_is 0
: > "$rt/seen"
: > "$rt/calls"
with_shims job_sign_key
check "sign --sign works" rc_is 0
check "the secret was seen by no child process" same "$(wc -c < "$rt/seen")" 0
for t in flatpak gpg ostree; do
    check "(the shim of $t was called: the check proves something)" test "$(grep -c "^$t " "$rt/calls")" -gt 0
done
check "the key was imported" file_has "$rt/calls" "^gpg .*--import"
check "the commits were signed with ostree gpg-sign" file_has "$rt/calls" "^ostree .*gpg-sign"
check "the summary was signed without appstream data" file_has "$rt/calls" "^flatpak build-update-repo --no-update-appstream --gpg-sign="
check "no static delta" not file_has "$rt/calls" "generate-static-deltas"
check "no prune" not file_has "$rt/calls" "prune"
check "no commit was made or pulled from the build" not file_has "$rt/calls" "build-commit-from\\|pull-local\\|fake-commit"
check "no git, no network in the step with the key" not file_has "$rt/calls" "^git "
check "(no keyring is left)" no_leftovers
use_secret a
job_finish
drop_secret
check "finish refuses to run with the secret in its environment" rc_is 2
check "(the message says so)" out_has "must run without the secret"
job_finish
check "finish accepts what sign made" rc_is 0
cp -a "$w/signed-repo" "$w/signed-repo.orig"
cp "$w/sign.gho" "$w/sign.gho.orig"

# a tampered prepared site or sign list
job_sign_check
check "the untouched files pass the checks" rc_is 0
unpack_prepared
echo surprise > "$w/x/site/evil.txt"
pack_prepared
job_sign_check
check "sign refuses a prepared site that is not the one prepare reported" rc_is 1
check "(the message says so)" out_has "not the one the earlier job produced"
check "(nothing was unpacked)" not test -e "$w/signwork/sign-list.txt"
unpack_prepared
ln -s /etc/passwd "$w/x/site/repo/objects/link"
pack_prepared
rehash_prepared
job_sign_check
check "sign refuses a prepared site with a symlink, whatever hash prepare reported" rc_is 2
check "(the message says plain files)" out_has "not a plain file"
restore_prepared
unpack_prepared
printf '%s\n' "$locale_commit" > "$w/x/site/repo/refs/heads/$app_ref"
pack_prepared
rehash_prepared
job_sign_check
check "sign refuses a repository whose listed ref is not at the listed commit" rc_is 1
check "(the message names the ref)" out_has "is not at the listed commit"
restore_prepared
echo "app/other.app/x86_64/stable	$app_commit" >> "$w/prepared/sign-list.txt"
job_sign_check
check "sign refuses a sign list that is not the one prepare reported" rc_is 1
rehash_prepared
job_sign_check
check "sign refuses a sign list with a ref that is not in refs.txt" rc_is 2
check "(the message says so)" out_has "is not a ref of the build"
restore_prepared
awk -F'\t' -v r="$app_ref" -v c="$locale_commit" 'BEGIN { OFS = "\t" } $1 == r { $2 = c } { print }' "$w/prepared.orig/sign-list.txt" > "$w/prepared/sign-list.txt"
rehash_prepared
job_sign_check
check "sign refuses a sign list with another commit than the build made" rc_is 2
check "(the message says so)" out_has "another commit than the build made"
restore_prepared
grep -v "^$app_ref" "$w/prepared.orig/sign-list.txt" > "$w/prepared/sign-list.txt"
rehash_prepared
job_sign_check
check "sign refuses a sign list that lacks a ref of the build" rc_is 2
check "(the message says so)" out_has "of the build is not on it"
restore_prepared
sed -i 's/^base_sha=.*/base_sha=0000000000000000000000000000000000000001/' "$w/prepare.gho"
job_sign_check
check "sign refuses a base that is not the one refs.txt records" rc_is 2
restore_prepared
job_sign_check
EXPECT_REFS_SHA256=0
run "$rel/sign-flatpak-repo.sh" --check "$w/prepared/prepared.tar" "$w/prepared/sign-list.txt" "$w/commits/refs.txt" "$w/signwork"
check "sign refuses a refs.txt that is not the one the build job reported" rc_is 1

# a signed repository that is not the prepared one plus signatures
job_sign_check
job_sign_key
restore_signed() { rm -rf "$w/signed-repo"; cp -a "$w/signed-repo.orig" "$w/signed-repo"; cp "$w/sign.gho.orig" "$w/sign.gho"; }
rehash_signed() { sed -i '/^signed_sha256=/d' "$w/sign.gho"; echo "signed_sha256=$(sha256_of "$w/signed-repo/signed-repo.tar")" >> "$w/sign.gho"; }
unpack_signed() { rm -rf "$w/y"; mkdir "$w/y"; tar -xf "$w/signed-repo.orig/signed-repo.tar" -C "$w/y"; }
pack_signed() { tar -C "$w/y" -cf "$w/signed-repo/signed-repo.tar" repo; rehash_signed; }
restore_signed
job_finish
check "finish accepts the signed repository" rc_is 0
unpack_signed
echo surprise >> "$w/y/repo/refs/heads/$app_ref"
tar -C "$w/y" -cf "$w/signed-repo/signed-repo.tar" repo
job_finish
check "finish refuses a signed repository that is not the one sign reported" rc_is 1
check "(the message says so)" out_has "not the one the earlier job produced"
restore_signed
unpack_signed
mkdir -p "$w/y/repo/refs/heads/app/evil.app/x86_64"
printf '%s\n' "$app_commit" > "$w/y/repo/refs/heads/app/evil.app/x86_64/stable"
pack_signed
job_finish
check "finish refuses an extra ref" rc_is 1
check "(the message says so)" out_has "other refs or commits"
restore_signed
unpack_signed
printf '%s\n' "$locale_commit" > "$w/y/repo/refs/heads/$app_ref"
pack_signed
job_finish
check "finish refuses a ref moved to another commit" rc_is 1
check "(the message says so)" out_has "other refs or commits"
restore_signed
unpack_signed
echo "<script>alert(1)</script>" > "$w/y/repo/evil.html"
pack_signed
job_finish
check "finish refuses a stray file in the repository" rc_is 1
check "(the message names the file)" out_has "evil.html"
restore_signed
unpack_signed
obj=$(find "$w/y/repo/objects" -name '*.commit' | head -n 1)
echo changed >> "$obj"
pack_signed
job_finish
check "finish refuses a changed object" rc_is 1
check "(the message says so)" out_has "differs from the prepared one"
restore_signed
unpack_signed
rm "$w/y/repo/objects/$(printf '%s' "$app_commit" | cut -c1-2)/${app_commit#??}.commitmeta"
pack_signed
job_finish
check "finish refuses a listed commit that is not signed" rc_fails
check "(the client check says so)" out_has "not signed by a key"
restore_signed
unpack_signed
rm "$w/y/repo/summary.sig"
pack_signed
job_finish
check "finish refuses an unsigned summary" rc_fails
restore_signed
unpack_signed
astream=$(cat "$w/y/repo/refs/heads/appstream/x86_64")
rm "$w/y/repo/objects/$(printf '%s' "$astream" | cut -c1-2)/${astream#??}.commitmeta"
pack_signed
job_finish
check "finish refuses an appstream commit that is not signed (Flatpak itself does not notice)" rc_fails
check "(the pull with signature checks says so)" out_has "not signed by a key"
restore_signed
unpack_signed
ln -s /etc/passwd "$w/y/repo/link"
pack_signed
job_finish
check "finish refuses a symlink in the signed repository" rc_is 2
check "(the message says plain files)" out_has "not a plain file"
restore_signed
unpack_prepared
printf '[Flatpak Repo]\nGPGKey=%s\n' "$(gpg --batch --dearmor < "$keys/b/pub.asc" | base64 -w0)" > "$w/x/site/kuickshare.flatpakrepo"
pack_prepared
rehash_prepared
job_finish
check "finish refuses a GPGKey that is not the key of the public key file" rc_is 1
check "(the message says so)" out_has "byte for byte"
restore_prepared
unpack_prepared
k=$({ gpg --batch --dearmor < "$keys/a/pub.asc"; printf '\250\003PGP'; } | base64 -w0)
sed -i "s|^GPGKey=.*|GPGKey=$k|" "$w/x/site/kuickshare.flatpakrepo" "$w/x/site/kuickshare.flatpakref"
pack_prepared
rehash_prepared
job_finish
check "finish refuses the same key with other bytes (an extra marker packet)" rc_is 1
check "(the message says so)" out_has "byte for byte"
restore_prepared
job_finish
check "and accepts everything again once the files are the real ones" rc_is 0
end_case "split signing: the secret reaches only the sign step; tampered prepared site, sign list, refs, signed repository, stray file, object, signature and key are refused"

# ---- 6d. gh-pages moved since the build: sign refuses before the key is imported ----
make_world 23
build_jobs
git clone -q "file://$origin" "$w/other"
git -C "$w/other" checkout -q gh-pages
echo other > "$w/other/other.txt"
git -C "$w/other" add -A
git -C "$w/other" commit -q -m "Another release"
git -C "$w/other" push -q origin gh-pages
rt=$work/shim2
mkdir -p "$rt/bin"
for t in flatpak gpg ostree git; do
    real=$(command -v "$t")
    # shellcheck disable=SC2016 # the shim expands its own variables
    printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\nexec %s "$@"\n' "$t" "$rt" "$real" > "$rt/bin/$t"
    chmod +x "$rt/bin/$t"
done
: > "$rt/calls"
with_shims job_sign
check "sign exits 3 when gh-pages is not where the build left it" rc_is 3
check "(the message says stale base)" out_has "stale base"
check "the key was never imported" not file_has "$rt/calls" "--import"
check "the repository was not touched" not file_has "$rt/calls" "gpg-sign\\|build-update-repo"
check "gh-pages did ask: git ls-remote ran" file_has "$rt/calls" "^git ls-remote"
check "(no keyring is left)" no_leftovers
end_case "stale base: sign refuses before the key is imported, nothing was signed"

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

# ---- 13. no release below an existing one ----
make_world 14
tag_seed() { git -C "$seed" tag -a "$1" -m "$1" "v1.2.3^{commit}"; git -C "$seed" push -q "$origin" "$1"; git -C "$w/checkout" fetch -q --tags origin; }
tag_seed v1.2.2
tag_seed v1.2.3-rc.1
tag_seed v9.9.9-rc.1
tag_seed not-a-release
job_validate
check "an older tag, prerelease-looking tags and other tags do not block the newest release" rc_is 0
tag_seed v1.2.10
job_validate
check "a release below an existing tag (v1.2.10 is above v1.2.3 in version order) is refused" rc_is 1
check "(the message names the newest tag)" out_has "the newest release tag is v1.2.10, not v1.2.3"
tag_seed v1.3.0
job_validate
check "still refused with a newer tag" rc_is 1
check "(the message names the newest tag)" out_has "the newest release tag is v1.3.0"
end_case "anti-downgrade: only the newest vMAJOR.MINOR.PATCH tag is released; other tags do not count"

# ---- 14. a refused push moves nothing, and is not "another writer" ----
make_world 15
build_jobs
job_verify
job_sign
# A server that refuses gh-pages (a ruleset, an outage) but would take gh-pages-previous.
# shellcheck disable=SC2016 # the hook expands $1 itself
printf '#!/bin/sh\n[ "$1" != refs/heads/gh-pages ] || { echo "refused by the test" >&2; exit 1; }\n' > "$origin/hooks/update"
chmod +x "$origin/hooks/update"
job_publish
check "publish exits 1 when the push fails and neither branch moved" rc_is 1
check "(the message says nothing moved)" out_has "neither branch has moved"
check "(and does not blame another writer)" not out_has "another writer"
check "gh-pages is untouched" same "$(pages_head)" "$base_sha"
check "gh-pages-previous was not written either" not git -C "$origin" rev-parse --verify -q refs/heads/gh-pages-previous
check "the release is still a draft" state_is "draft 5 -"
rm "$origin/hooks/update"
job_publish
check "the rerun completes" rc_is 0
check "gh-pages-previous holds the old repository" same "$(git -C "$origin" rev-parse refs/heads/gh-pages-previous)" "$base_sha"
check "the release is published and the latest" state_is "published 5 latest"
end_case "push refused by the server: exit 1, neither branch moved, the rerun completes"

# ---- 15. a writer gets in during the push ----
make_world 16
build_jobs
job_verify
job_sign
# A server that moves gh-pages by itself, as another release would (the variables are unset because a hook may not update
# refs inside the quarantine of a push), and then refuses the push.
# shellcheck disable=SC2016 # the hook expands its own variables
printf '#!/bin/sh\nunset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\nt=$(git rev-parse "refs/heads/gh-pages^{tree}")\nc=$(git commit-tree "$t" -p refs/heads/gh-pages -m "Another release")\ngit update-ref refs/heads/gh-pages "$c"\necho "refused by the test" >&2\nexit 1\n' > "$origin/hooks/pre-receive"
chmod +x "$origin/hooks/pre-receive"
job_publish
rm "$origin/hooks/pre-receive"
check "publish exits 3 when gh-pages moved during the push" rc_is 3
check "(the message says another writer changed it)" out_has "another writer"
check "gh-pages is the other writer's commit, not ours" not same "$(pages_head)" "$base_sha"
check "gh-pages-previous was not written" not git -C "$origin" rev-parse --verify -q refs/heads/gh-pages-previous
check "the release is still a draft" state_is "draft 5 -"
end_case "a writer moves gh-pages during the push: exit 3, nothing of ours pushed"

# ---- 15b. the push lands, the acknowledgement is lost ----
make_world 19
build_jobs
job_verify
job_sign
# A server that takes the push and then dies before it can say so: once the refs are committed (reference-transaction hook,
# state "committed"), the hook kills its parent, git receive-pack. The client reports "the remote end hung up unexpectedly"
# while gh-pages and gh-pages-previous have both moved.
# shellcheck disable=SC2016 # the hook expands its own variables
printf '#!/bin/sh\n[ "$1" = committed ] || exit 0\ngrep -q " refs/heads/gh-pages$" || exit 0\nkill -9 $PPID\n' > "$origin/hooks/reference-transaction"
chmod +x "$origin/hooks/reference-transaction"
job_publish
rm "$origin/hooks/reference-transaction"
check "publish exits 0 although git reported a failure" rc_is 0
check "(the client did see the failure)" out_has "hung up unexpectedly"
check "(the message says the push had landed)" out_has "the push had landed"
check "gh-pages is the new site" same "$(pages_file repo/summary)" "$(tar -xOf "$w/signed/site.tar" ./repo/summary)"
check "gh-pages-previous holds the old repository" same "$(git -C "$origin" rev-parse refs/heads/gh-pages-previous)" "$base_sha"
check "the release is published and the latest" state_is "published 5 latest"
end_case "the push lands but its acknowledgement is lost: the run goes on and publishes"

# ---- 16. gh-pages-previous refused, gh-pages alone would pass ----
make_world 17
build_jobs
job_verify
job_sign
# shellcheck disable=SC2016 # the hook expands $1 itself
printf '#!/bin/sh\n[ "$1" != refs/heads/gh-pages-previous ] || { echo "refused by the test" >&2; exit 1; }\n' > "$origin/hooks/update"
chmod +x "$origin/hooks/update"
job_publish
check "publish fails when gh-pages-previous is refused" rc_is 1
check "gh-pages is untouched (the push is atomic)" same "$(pages_head)" "$base_sha"
check "gh-pages-previous does not exist" not git -C "$origin" rev-parse --verify -q refs/heads/gh-pages-previous
check "the release is still a draft" state_is "draft 5 -"
end_case "gh-pages-previous refused: gh-pages does not move either"

# ---- 17. the first release: there is no gh-pages yet ----
make_world 18
git -C "$origin" update-ref -d refs/heads/gh-pages
base_sha=-
# What the build job does without a gh-pages: an empty directory instead of the clone.
rm -rf "$w/pages" "$w/commits" "$w/dist"
mkdir "$w/pages"
ostree --repo="$w/pages/repo" fake-commit "app/$app_id/x86_64/stable" "first 1.2.3" > /dev/null
ostree --repo="$w/pages/repo" fake-commit "runtime/$app_id.Locale/x86_64/stable" "first locale 1.2.3" > /dev/null
run "$rel/export-flatpak-commits.sh" "$w/pages" "$w/commits"
check "the export records no base" file_has "$w/commits/refs.txt" "^base	-$"
make_assets "$w/dist" 1.2.3
job_prepare
job_verify
job_sign
check "sign accepts the first release" rc_is 0
check "(and reports no base)" file_has "$w/sign.out" "built on gh-pages -"
job_publish
check "publish exits 0 with SITE_BASE_SHA=-" rc_is 0
check "gh-pages exists now" test -n "$(pages_head)"
check "there is no gh-pages-previous" not git -C "$origin" rev-parse --verify -q refs/heads/gh-pages-previous
check "gh-pages is the new site" same "$(pages_file repo/summary)" "$(tar -xOf "$w/signed/site.tar" ./repo/summary)"
check "the release is published and the latest" state_is "published 5 latest"
end_case "first release: no gh-pages, SITE_BASE_SHA=-: pushed without gh-pages-previous"

# ---- 18. no secret anywhere ----
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
