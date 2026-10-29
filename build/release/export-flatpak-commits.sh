#!/bin/sh
# Takes the commits a Flatpak build added to the published repository out of it, without any signature, so that a job
# that holds no key can hand them to the one that does.
#   export-flatpak-commits.sh <pages-dir> <out-dir>
# <pages-dir> is a clone of the gh-pages branch (or an empty directory for the first release) that
# build-flatpak.sh exported its build into (KUICKSHARE_FLATPAK_REPO=<pages-dir>/repo). For every app and runtime ref whose
# commit is not the published one, the commit is copied (ostree pull-local, so its checksum does not change) into a fresh
# repository <out-dir>/repo, and <out-dir>/refs.txt records what was done:
#   base <TAB> <commit of gh-pages the build started from, or - when there was none>
#   <ref> <TAB> <new commit> <TAB> <its parent, or - when it has none>
# <out-dir>/flatpak-commits.tar holds both. Each new commit's parent must be the published commit of its ref, otherwise
# the update chain of the installed clients would break: exit 3.
# Needs ostree, git, tar. Exit codes: 0 ok, 1 nothing to export or a failed tool, 2 bad input, 3 broken parent chain.
set -eu
[ $# -eq 2 ] || { echo "usage: export-flatpak-commits.sh <pages-dir> <out-dir>" >&2; exit 2; }
[ -d "$1/repo" ] || { echo "export-flatpak-commits: $1/repo does not exist" >&2; exit 2; }
pages=$(cd "$1" && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
repo=$pages/repo
# A clone of gh-pages lacks the empty directories of the repository (git keeps none).
mkdir -p "$repo/refs/heads" "$repo/refs/mirrors" "$repo/refs/remotes" "$repo/tmp"
fail() { echo "export-flatpak-commits: $*" >&2; exit 1; }

base=-
if [ -d "$pages/.git" ]; then base=$(git -C "$pages" rev-parse HEAD); fi
rm -rf "$out/repo" "$out/refs.txt" "$out/flatpak-commits.tar"
ostree --repo="$out/repo" init --mode=archive-z2
printf 'base\t%s\n' "$base" > "$out/refs.txt"

exported=
for ref in $(ostree --repo="$repo" refs | sort); do
    case $ref in app/* | runtime/*) ;; *) continue ;; esac
    new=$(ostree --repo="$repo" rev-parse "$ref")
    old=-
    if [ "$base" != - ] && git -C "$pages" cat-file -e "HEAD:repo/refs/heads/$ref" 2> /dev/null; then
        old=$(git -C "$pages" show "HEAD:repo/refs/heads/$ref" | tr -d '\n')
    fi
    [ "$new" != "$old" ] || continue
    if parent=$(ostree --repo="$repo" rev-parse "$new^" 2> /dev/null); then :; else parent=-; fi
    if [ "$parent" != "$old" ]; then
        echo "export-flatpak-commits: $ref: the new commit $new has parent $parent, the published one is $old" >&2
        exit 3
    fi
    printf '%s\t%s\t%s\n' "$ref" "$new" "$parent" >> "$out/refs.txt"
    exported="$exported $ref"
done
[ -n "$exported" ] || fail "the build added no commit to $repo"

# shellcheck disable=SC2086 # refs have no spaces
set -- $exported
for ref in "$@"; do
    want=$(awk -F'\t' -v r="$ref" '$1 == r { print $2 }' "$out/refs.txt")
    # By checksum, not by name: pull-local resolves a name through the summary file when the
    # repository has one, and the published summary still names the old commit.
    ostree --repo="$out/repo" pull-local "$repo" "$want"
    ostree --repo="$out/repo" refs --create="$ref" "$want"
    [ "$(ostree --repo="$out/repo" rev-parse "$ref")" = "$want" ] || fail "$ref was not copied unchanged"
done
tar --sort=name --owner=0 --group=0 --numeric-owner -C "$out" -cf "$out/flatpak-commits.tar" repo refs.txt
echo "export-flatpak-commits: $# refs exported on top of $base:"
cat "$out/refs.txt"
