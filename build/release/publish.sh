#!/bin/sh
# Release gate 4, the only step that writes: publishes verified files, never rebuilds anything. It can be run again
# after any failure; each step looks at what is already there and does only what is missing.
#
#   1. Checks again what it was given: the five assets against SHA256SUMS, the site archive against the hash the sign
#      job reported, and that the tag still points to the commit of the run.
#   2. Finds the release for the tag (drafts included). Creates a DRAFT when there is none, then uploads the assets that
#      are missing. An asset that exists with the same SHA-256 is skipped; one that differs fails the run. Never --clobber.
#   3. Pushes the Flatpak repository to gh-pages as one orphan commit, keeping the old head as gh-pages-previous. Skipped
#      when gh-pages already has that tree; refused (exit 3) when gh-pages is neither the base the site was built on nor
#      the new site; the push itself is one `git push --atomic` with a lease on gh-pages and on gh-pages-previous. When it
#      reports a failure both branches are read again: gh-pages already at the new commit means the push had landed (the
#      acknowledgement was lost) and the run goes on; exit 3 if either branch moved to something else, exit 1 if not.
#   4. Waits until Pages serves the new repo/summary and repo/summary.sig.
#   5. Publishes the draft (--latest) and checks the release again: exactly the five assets, each with its SHA-256.
# The release stays a draft until the Flatpak repository is live, so a release that people can see is never incomplete.
#
# Environment (nothing from the command line):
#   RELEASE_TAG, GITHUB_SHA, GITHUB_REPOSITORY, GH_TOKEN
#   ASSETS_DIR           the five release files
#   EXPECT_SUMS_SHA256   SHA-256 of SHA256SUMS, as reported by the verify job
#   SITE_TAR             the site archive from the finish job, EXPECT_SITE_SHA256 its SHA-256
#   SITE_BASE_SHA        the gh-pages commit the site was built on ("-" when there was none)
#   SITE_URL             where Pages serves the site
#   REPO_REMOTE          (optional) git remote of the repository, default https://github.com/$GITHUB_REPOSITORY.git
#   PAGES_WAIT_SECS, PAGES_POLL_INTERVAL   (optional) how long and how often to look at Pages, default 600 and 15
#   KUICKSHARE_ROOT      (optional) the checkout the files are checked against
# Exit codes: 0 done, 1 a check or the push failed, 2 bad input, 3 gh-pages is not where the build left it.
set -eu
tools=$(cd "$(dirname "$0")/../.." && pwd)
root=${KUICKSHARE_ROOT:-$tools}
: "${RELEASE_TAG:?}" "${GITHUB_SHA:?}" "${GITHUB_REPOSITORY:?}" "${ASSETS_DIR:?}" "${EXPECT_SUMS_SHA256:?}"
: "${SITE_TAR:?}" "${EXPECT_SITE_SHA256:?}" "${SITE_BASE_SHA:?}" "${SITE_URL:?}"
tag=$RELEASE_TAG
repo=$GITHUB_REPOSITORY
remote=${REPO_REMOTE:-https://github.com/$repo.git}
site_url=${SITE_URL%/}
wait_secs=${PAGES_WAIT_SECS:-600}
interval=${PAGES_POLL_INTERVAL:-15}
fail() { echo "publish: $*" >&2; exit 1; }
# After a push that reported a failure: if gh-pages is the commit this run pushed and gh-pages-previous the old head, the
# push had landed and only the acknowledgement was lost (the atomic push moved both): go on. Exit 3 only if gh-pages or
# gh-pages-previous is no longer what this run read (a writer got in, and the atomic push moved nothing); otherwise the
# push failed for another reason (network, server, ruleset): exit 1.
push_failed() {
    now=$(git ls-remote "$remote" refs/heads/gh-pages refs/heads/gh-pages-previous) || fail "the push to gh-pages failed and the branches cannot be read again"
    now_head=$(printf '%s\n' "$now" | awk '$2 == "refs/heads/gh-pages" { print $1 }')
    now_previous=$(printf '%s\n' "$now" | awk '$2 == "refs/heads/gh-pages-previous" { print $1 }')
    if [ "$now_head" = "$commit" ] && [ "$now_previous" = "$remote_head" ]; then
        echo "publish: the push had landed (gh-pages is $commit), only its acknowledgement was lost"
        return 0
    fi
    if [ "$now_head" != "$remote_head" ] || [ "$now_previous" != "$remote_previous" ]; then
        echo "publish: the push to gh-pages failed because another writer changed gh-pages or gh-pages-previous. Nothing was pushed; build again." >&2
        exit 3
    fi
    fail "the push to gh-pages failed and neither branch has moved (network or server error, or the server refused it): nothing was pushed; run this job again"
}
printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || { echo "publish: the tag is not vMAJOR.MINOR.PATCH" >&2; exit 2; }
printf '%s\n' "$repo" | grep -Eq '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$' || { echo "publish: bad repository name" >&2; exit 2; }
printf '%s\n' "$SITE_BASE_SHA" | grep -Eq '^(-|[0-9a-f]{40})$' || { echo "publish: SITE_BASE_SHA is neither - nor a commit id" >&2; exit 2; }
version=${tag#v}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
[ "$(git -C "$root" rev-parse HEAD)" = "$GITHUB_SHA" ] || fail "HEAD is not $GITHUB_SHA"

# Credentials for git over https go through the environment of the git processes only: nothing is written to a git config
# or a URL, and the header is masked in the log.
if [ -n "${GH_TOKEN:-}" ]; then
    case $remote in
        https://*)
            auth=$(printf 'x-access-token:%s' "$GH_TOKEN" | base64 | tr -d '\n')
            echo "::add-mask::$auth"
            export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.https://github.com/.extraheader "GIT_CONFIG_VALUE_0=AUTHORIZATION: basic $auth"
            ;;
    esac
fi

# The five assets, SHA256SUMS last: a release without it is visibly unfinished.
set -- KuickShare-x86_64.AppImage KuickShare-x86_64.AppImage.zsync KuickShare-x86_64.flatpak KuickShare-x86_64.tar.gz SHA256SUMS

# ---- 1. what we were given ----
"$tools/build/release/verify-assets.sh" --recheck "$ASSETS_DIR"
[ "$(sha256_of "$ASSETS_DIR/SHA256SUMS")" = "$EXPECT_SUMS_SHA256" ] || fail "SHA256SUMS is not the file the verify job produced"
[ "$(sha256_of "$SITE_TAR")" = "$EXPECT_SITE_SHA256" ] || fail "the site archive is not the one the finish job produced"
tar -tf "$SITE_TAR" > "$tmp/site.list"
if grep -Eq '(^/|(^|/)\.\.(/|$)|(^|/)\.git(/|$))' "$tmp/site.list"; then fail "the site archive has an unsafe path"; fi
tags=$(git ls-remote "$remote" "refs/tags/$tag" "refs/tags/$tag^{}")
peeled=$(printf '%s\n' "$tags" | awk -v r="refs/tags/$tag^{}" '$2 == r { print $1 }')
direct=$(printf '%s\n' "$tags" | awk -v r="refs/tags/$tag" '$2 == r { print $1 }')
[ "${peeled:-$direct}" = "$GITHUB_SHA" ] || fail "the tag $tag on the remote points to '${peeled:-$direct}', not to $GITHUB_SHA"
echo "publish: $tag, commit $GITHUB_SHA, files and site verified"

# ---- release lookup ----
# Prints "none", or "<id> <draft:true|false>" and writes the assets (id, name, state, digest, size; tab separated) to
# $tmp/assets.tsv. The list endpoint is used because the by-tag endpoint does not show drafts.
lookup() {
    gh api --paginate "repos/$repo/releases?per_page=100" > "$tmp/releases.json"
    python3 -I - "$tmp/releases.json" "$tag" "$tmp/assets.tsv" <<'PY'
import json
import sys

path, tag, out = sys.argv[1:4]
with open(path) as f:
    text = f.read()
decoder = json.JSONDecoder()
releases = []
pos = 0
while True:
    while pos < len(text) and text[pos].isspace():
        pos += 1
    if pos >= len(text):
        break
    page, pos = decoder.raw_decode(text, pos)
    releases.extend(page if isinstance(page, list) else [page])
found = [r for r in releases if r.get("tag_name") == tag]
if not found:
    print("none")
    sys.exit(0)
if len(found) > 1:
    sys.exit("%d releases have the tag %s" % (len(found), tag))
release = found[0]
with open(out, "w") as f:
    for a in release.get("assets", []):
        f.write("%s\t%s\t%s\t%s\t%s\n" % (a["id"], a["name"], a.get("state", "uploaded"), a.get("digest") or "-", a.get("size", 0)))
print(release["id"], "true" if release.get("draft") else "false")
PY
}

# Does the asset on the release have this SHA-256? Uses the digest GitHub reports and, when there is none, the file.
asset_matches() { # name state digest want
    [ "$2" = uploaded ] || return 1
    if [ "$3" != - ]; then
        [ "$3" = "sha256:$4" ]
        return
    fi
    rm -rf "$tmp/dl"
    mkdir "$tmp/dl"
    gh release download "$tag" --repo "$repo" --pattern "$1" --dir "$tmp/dl"
    [ "$(sha256_of "$tmp/dl/$1")" = "$4" ]
}

# Exactly the five assets, all complete, all equal to the local files.
check_release() {
    info=$(lookup)
    [ "$info" != none ] || fail "release $tag does not exist"
    names=$(cut -f2 "$tmp/assets.tsv" | sort)
    expected=$(printf '%s\n' "$@" | sort)
    [ "$names" = "$expected" ] || fail "the release has the wrong files. Expected: $(echo "$expected" | tr '\n' ' ')Found: $(echo "$names" | tr '\n' ' ')"
    for name in "$@"; do
        row=$(awk -F'\t' -v n="$name" '$2 == n' "$tmp/assets.tsv")
        want=$(sha256_of "$ASSETS_DIR/$name")
        asset_matches "$name" "$(echo "$row" | cut -f3)" "$(echo "$row" | cut -f4)" "$want" || fail "$name on the release is not the verified file (or not completely uploaded)"
    done
    echo "$info"
}

# ---- 2. the release and its assets ----
info=$(lookup)
if [ "$info" = none ]; then
    python3 -I "$tools/build/release/release-notes.py" "$root/data/io.github.the_beper.KuickShare.metainfo.xml" "$version" \
        --repo "$repo" > "$tmp/notes.md"
    gh release create "$tag" --repo "$repo" --draft --verify-tag --title "$tag" --notes-file "$tmp/notes.md"
    info=$(lookup)
    [ "$info" != none ] || fail "the release was created but cannot be found"
fi
release_id=${info% *}
if [ "${info#* }" = false ]; then
    echo "::warning::publish: release $tag is already published; only the missing files are added"
fi

for name in "$@"; do
    want=$(sha256_of "$ASSETS_DIR/$name")
    row=$(awk -F'\t' -v n="$name" '$2 == n' "$tmp/assets.tsv")
    if [ -n "$row" ]; then
        if asset_matches "$name" "$(echo "$row" | cut -f3)" "$(echo "$row" | cut -f4)" "$want"; then
            echo "publish: $name is already on the release, same SHA-256"
            continue
        fi
        if [ "$(echo "$row" | cut -f3)" = uploaded ]; then
            fail "$name is on the release $tag with another content: refusing to overwrite it (delete it by hand if that is intended)"
        fi
        # An upload that died halfway leaves an unfinished asset behind; it is no content to protect.
        echo "publish: removing the unfinished upload of $name"
        gh api -X DELETE "repos/$repo/releases/assets/$(echo "$row" | cut -f1)"
    fi
    echo "publish: uploading $name"
    gh release upload "$tag" "$ASSETS_DIR/$name" --repo "$repo"
done
check_release "$@" > /dev/null

# ---- 3. gh-pages ----
site=$tmp/site
mkdir "$site"
tar -xf "$SITE_TAR" -C "$site" --no-same-owner
g=$tmp/pages.git
git init --quiet --bare "$g"
git -C "$g" --work-tree="$site" -c core.excludesFile=/dev/null add --force --all
site_tree=$(git -C "$g" write-tree)
remote_head=$(git ls-remote "$remote" refs/heads/gh-pages | cut -f1)
remote_previous=$(git ls-remote "$remote" refs/heads/gh-pages-previous | cut -f1)
remote_tree=
if [ -n "$remote_head" ]; then
    git -C "$g" fetch --quiet "$remote" refs/heads/gh-pages
    remote_tree=$(git -C "$g" rev-parse "FETCH_HEAD^{tree}")
fi
base=$SITE_BASE_SHA
[ "$base" != - ] || base=
if [ "$remote_tree" = "$site_tree" ]; then
    echo "publish: gh-pages already has this site (tree $site_tree), no push"
elif [ "$remote_head" = "$base" ]; then
    commit=$(GIT_AUTHOR_NAME=github-actions GIT_AUTHOR_EMAIL=github-actions@users.noreply.github.com \
        GIT_COMMITTER_NAME=github-actions GIT_COMMITTER_EMAIL=github-actions@users.noreply.github.com \
        git -C "$g" commit-tree "$site_tree" -m "Flatpak repository for $tag")
    # One atomic push, both branches or neither, each with a lease on the value read above: a writer that got in
    # since then makes the push fail and nothing moves.
    if [ -n "$remote_head" ]; then
        echo "publish: keeping the old gh-pages ($remote_head) as gh-pages-previous"
        git -C "$g" push --quiet --atomic --force-with-lease="refs/heads/gh-pages:$remote_head" \
            --force-with-lease="refs/heads/gh-pages-previous:$remote_previous" \
            "$remote" "$commit:refs/heads/gh-pages" "$remote_head:refs/heads/gh-pages-previous" || push_failed
    else
        git -C "$g" push --quiet --atomic --force-with-lease=refs/heads/gh-pages: "$remote" "$commit:refs/heads/gh-pages" || push_failed
    fi
    echo "publish: gh-pages is now $commit"
else
    echo "publish: gh-pages is at '${remote_head:-nothing}', not at the base '${base:-nothing}' the site was built on, and it is not the new site: another release got there first. Nothing was pushed; build again." >&2
    exit 3
fi

# ---- 4. wait for Pages ----
deadline=$(( $(date +%s) + wait_secs ))
while :; do
    served=1
    for f in repo/summary repo/summary.sig; do
        [ -f "$site/$f" ] || fail "the site has no $f"
        if curl -fsSL --max-time 30 -H 'Cache-Control: no-cache' -o "$tmp/served" "$site_url/$f?nocache=$(date +%s)" 2> "$tmp/curl.err" \
            && [ "$(sha256_of "$tmp/served")" = "$(sha256_of "$site/$f")" ]; then
            :
        else
            served=0
            last=$f
        fi
    done
    [ "$served" = 0 ] || break
    [ "$(date +%s)" -lt "$deadline" ] || fail "Pages does not serve the new $last at $site_url after ${wait_secs}s: the release stays a draft; run this job again when Pages is up"
    sleep "$interval"
done
echo "publish: Pages serves the new repository"

# ---- 5. publish ----
gh release edit "$tag" --repo "$repo" --draft=false --latest
info=$(check_release "$@")
[ "${info#* }" = false ] || fail "the release is still a draft"
gh api "repos/$repo/releases/latest" > "$tmp/latest.json"
latest=$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["tag_name"])' "$tmp/latest.json")
[ "$latest" = "$tag" ] || fail "the latest release is $latest, not $tag"
echo "publish: $tag is published and is the latest release (release id $release_id)"
