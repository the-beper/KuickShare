# shellcheck shell=sh disable=SC2154
# Shared by prepare-flatpak-repo.sh, sign-flatpak-repo.sh and finish-flatpak-repo.sh (sourced). The caller sets $me (its
# name, for the messages) and $tmp (its scratch directory) before it calls anything here.
tab=$(printf '\t')
fail() { echo "$me: $*" >&2; exit 1; }
bad_input() { echo "$me: $*" >&2; exit 2; }
stale() { echo "$me: stale base: $*" >&2; exit 3; }

# The steps that read what other jobs made run without the signing key; an environment that has it is a mistake in the
# workflow, so they refuse to start.
no_secret() {
    [ -z "${FLATPAK_GPG_PRIVATE_KEY:-}" ] || bad_input "FLATPAK_GPG_PRIVATE_KEY is set: this step reads data from other jobs and must run without the secret"
}

# check_sha256 FILE EXPECTED WHAT
check_sha256() {
    [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ] || fail "$3 is not the one the earlier job produced"
}

# tar_safe TAR PREFIX: every member is a plain file or a directory inside PREFIX/ (or PREFIX itself, or one of the plain
# files named in the rest of the arguments): no absolute path, no .., no link or device. A link in an archive that a job
# made could point anywhere once extracted.
tar_safe() {
    tar_file=$1
    tar_prefix=$2
    shift 2
    # --quoting-style=escape: a name with a newline or a backslash is listed with a backslash, and refused below.
    tar --quoting-style=escape -tf "$tar_file" > "$tmp/tar.names" 2> /dev/null || bad_input "$tar_file is not a readable archive"
    if grep -Evq '^[A-Za-z0-9._/-]+$' "$tmp/tar.names"; then
        bad_input "$(basename "$tar_file") has a member name with characters outside A-Z a-z 0-9 . _ / -"
    fi
    tar_allowed="^($tar_prefix(/|\$)"
    for extra in "$@"; do tar_allowed="$tar_allowed|$(printf '%s' "$extra" | sed 's/[.]/\\./g')\$"; done
    tar_allowed="$tar_allowed)"
    if grep -Evq "$tar_allowed" "$tmp/tar.names" || grep -Eq '(^/|(^|/)\.\.(/|$))' "$tmp/tar.names"; then
        bad_input "$(basename "$tar_file") has files other than $tar_prefix/ $*"
    fi
    if tar -tvf "$tar_file" | cut -c1 | grep -qv '[-d]'; then
        bad_input "$(basename "$tar_file") has a member that is not a plain file or a directory (link, device)"
    fi
}

# read_refs FILE: checks the refs.txt export-flatpak-commits.sh wrote (a base line, then ref, new commit, parent), sets
# $base and writes the refs, one per line, to $tmp/ref.names.
read_refs() {
    base=$(awk -F'\t' '$1 == "base" { print $2 }' "$1")
    printf '%s\n' "$base" | grep -Eq '^(-|[0-9a-f]{40})$' || bad_input "refs.txt has no valid base"
    : > "$tmp/ref.names"
    while IFS=$tab read -r ref commit parent; do
        [ "$ref" != base ] || continue
        printf '%s\n' "$ref" | grep -Eq '^(app|runtime)/[A-Za-z0-9._-]+/[A-Za-z0-9_-]+/[A-Za-z0-9._-]+$' || bad_input "refs.txt: bad ref '$ref'"
        printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{64}$' || bad_input "refs.txt: bad commit for $ref"
        printf '%s\n' "$parent" | grep -Eq '^(-|[0-9a-f]{64})$' || bad_input "refs.txt: bad parent for $ref"
        printf '%s\n' "$ref" >> "$tmp/ref.names"
    done < "$1"
    [ -s "$tmp/ref.names" ] || bad_input "refs.txt lists no ref"
    [ -z "$(sort "$tmp/ref.names" | uniq -d)" ] || bad_input "refs.txt lists a ref twice"
}

# key_fpr FILE: the fingerprint of the key in a public key file (armored or not); nothing unless the file holds exactly one
# primary key and no secret key. Needs GNUPGHOME to be a scratch keyring.
key_fpr() {
    gpg --batch --quiet --show-keys --with-colons "$1" 2> /dev/null |
        awk -F: '$1 == "pub" { n++; w = 1; next } $1 == "sec" { bad = 1 } $1 == "fpr" && w { f = $10; w = 0 } END { if (n == 1 && !bad && f != "") print f }'
}

# read_signlist FILE [REFS]: checks sign-list.txt (one `ref TAB commit` per ref to sign: the refs of the build and the
# appstream refs) and writes the refs, one per line, to $tmp/sign.refs. With the refs.txt of the build: every app and
# runtime ref of the list is a ref of the build with the commit the build made, and every ref of the build is listed.
read_signlist() {
    : > "$tmp/sign.refs"
    while IFS=$tab read -r ref commit extra; do
        printf '%s\n' "$ref" | grep -Eq '^((app|runtime)/[A-Za-z0-9._-]+/[A-Za-z0-9_-]+/[A-Za-z0-9._-]+|appstream2?/x86_64)$' || bad_input "sign list: bad ref '$ref'"
        printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{64}$' || bad_input "sign list: bad commit for $ref"
        [ -z "$extra" ] || bad_input "sign list: more than two fields for $ref"
        if [ -n "${2:-}" ]; then
            case $ref in
                app/* | runtime/*)
                    [ "$(awk -F'\t' -v r="$ref" '$1 == r { print $2 }' "$2")" = "$commit" ] \
                        || bad_input "sign list: $ref is not a ref of the build, or has another commit than the build made"
                    ;;
            esac
        fi
        printf '%s\n' "$ref" >> "$tmp/sign.refs"
    done < "$1"
    [ -s "$tmp/sign.refs" ] || bad_input "the sign list is empty"
    [ -z "$(sort "$tmp/sign.refs" | uniq -d)" ] || bad_input "the sign list has a ref twice"
    if [ -n "${2:-}" ]; then
        awk -F'\t' '$1 != "base" { print $1 }' "$2" > "$tmp/build.refs"
        while read -r ref; do
            grep -qxF "$ref" "$tmp/sign.refs" || bad_input "sign list: $ref of the build is not on it"
        done < "$tmp/build.refs"
    fi
}

# check_binding REPO REF COMMIT: the commit says it belongs to REF (ostree.ref-binding, which flatpak build-export and
# build-update-repo set on app, runtime and appstream commits alike: tried with flatpak 1.14.6), so a signed commit cannot be
# served under another ref.
check_binding() {
    [ "$(ostree --repo="$1" show --print-metadata-key=ostree.ref-binding "$3" 2> /dev/null)" = "['$2']" ] \
        || fail "$2 $3 is not bound to its own ref (ostree.ref-binding)"
}

# check_config FILE: the config of an ostree repository is read by every ostree and flatpak call, so it may hold only what
# `ostree init --mode=archive-z2` and flatpak build-update-repo write: no remote, no gpg setting, nothing else.
check_config() {
    [ -f "$1" ] || bad_input "the repository has no config file"
    if bad=$(grep -Evx '\[core\]|repo_version=1|mode=archive-z2|indexed-deltas=true|' "$1"); then
        printf '%s\n' "$bad" | sed 's/^/    /' >&2
        bad_input "the config of the repository has lines that are not the expected ones (see above)"
    fi
}

# ref_map REPO: "ref commit" for every ref of an ostree repository, sorted; stops if a ref cannot be read (an empty commit
# would pass the comparisons of check_pages_base). Refs under refs/mirrors are not listed by `ostree refs` but can end up in
# the summary, so a repository with any is refused.
ref_map() {
    [ -z "$(find "$1/refs/mirrors" -type f 2> /dev/null)" ] || fail "$1 has refs under refs/mirrors"
    ostree --repo="$1" refs > "$tmp/ref_map.list" || fail "cannot list the refs of $1"
    LC_ALL=C sort "$tmp/ref_map.list" > "$tmp/ref_map.sorted"
    while read -r ref; do
        ref_commit=$(ostree --repo="$1" rev-parse "$ref") && [ -n "$ref_commit" ] || fail "cannot read the commit of $ref in $1"
        printf '%s %s\n' "$ref" "$ref_commit"
    done < "$tmp/ref_map.sorted"
}

# no_deltas REPO: static deltas carry the signature of their commit and are not part of a release (publish-repo.sh deletes
# them), so none may be in a repository that is about to be signed or has been.
no_deltas() {
    [ ! -e "$1/deltas" ] && [ ! -e "$1/delta-indexes" ] || fail "the repository has static deltas (deltas/ or delta-indexes/)"
}

# check_pages_base REPO: the repository (prepared or signed) against gh-pages as the build saw it, which the job that made
# the repository is not trusted for. Needs $remote, $base, $tmp and $tmp/sign.refs (read_signlist). Clones gh-pages at $base
# (anonymous, read-only) and requires: every ref that is not on the sign list is unchanged and none was dropped; every listed
# ref continues its published commit (its parent is the published head), or has no parent when it is new; nothing else is new.
# Leaves "ref commit" of the published repository in $tmp/refs.pub.
check_pages_base() {
    ref_map "$1" > "$tmp/refs.checked"
    mkdir "$tmp/pages"
    if [ "$base" = - ]; then
        heads=$(git ls-remote --heads "$remote" gh-pages | cut -f1)
        [ -z "$heads" ] || stale "the build saw no gh-pages, but it exists now ($heads)"
        : > "$tmp/refs.pub"
    else
        git clone --quiet --depth 1 --branch gh-pages "$remote" "$tmp/pages"
        [ "$(git -C "$tmp/pages" rev-parse HEAD)" = "$base" ] || stale "gh-pages is not at $base any more"
        mkdir -p "$tmp/pages/repo/refs/heads" "$tmp/pages/repo/refs/mirrors" "$tmp/pages/repo/refs/remotes" "$tmp/pages/repo/tmp"
        ref_map "$tmp/pages/repo" > "$tmp/refs.pub"
    fi
    while read -r ref commit; do
        now=$(awk -v r="$ref" '$1 == r { print $2 }' "$tmp/refs.checked")
        [ -n "$now" ] || fail "$ref of gh-pages is missing in the repository"
        if grep -qxF "$ref" "$tmp/sign.refs"; then
            [ "$(ostree --repo="$1" rev-parse "$now^" 2> /dev/null)" = "$commit" ] || fail "$ref: the new commit $now does not continue the published commit $commit"
        else
            [ "$now" = "$commit" ] || fail "$ref is at $now in the repository and at $commit on gh-pages, and it is not on the sign list"
        fi
    done < "$tmp/refs.pub"
    while read -r ref commit; do
        if awk -v r="$ref" '$1 == r { f = 1 } END { exit !f }' "$tmp/refs.pub"; then continue; fi
        grep -qxF "$ref" "$tmp/sign.refs" || fail "$ref is new in the repository and not on the sign list"
        if ostree --repo="$1" rev-parse "$commit^" > /dev/null 2>&1; then fail "$ref is new but its commit $commit has a parent"; fi
    done < "$tmp/refs.checked"
}

# no_commitmeta REPO COMMIT: a commit about to be signed has no signature file (and none inside the repository's metadata).
no_commitmeta() {
    [ ! -e "$1/objects/$(printf '%s' "$2" | cut -c1-2)/${2#??}.commitmeta" ] || fail "commit $2 has a signature file already"
}
