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
    tar -tf "$tar_file" > "$tmp/tar.names" 2> /dev/null || bad_input "$tar_file is not a readable archive"
    tar_allowed="^($tar_prefix(/|\$)"
    for extra in "$@"; do tar_allowed="$tar_allowed|$extra\$"; done
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
