#!/bin/sh
# Sets the version of the sources for a release, in the three places check-version.sh compares with the tag
# (CONTRIBUTING.md, Making a release): the [package] version of Cargo.toml (and kuickshare-core in Cargo.lock),
# project(kuickshare VERSION) of src/shell/CMakeLists.txt, and a new first <release> of the metainfo dated today (UTC),
# whose notes come from NOTES. Never commits, tags or pushes.
#   build/release/version-bump.sh X.Y.Z NOTES
#       NOTES  a text file: the first line is the summary (the <p> for software centers), every other line is
#              "- text", one item each (the "What's Changed" list of the GitHub release); empty lines are skipped.
#   KUICKSHARE_ROOT  (optional) the checkout, default the one this script is in
# The notes are plain text (no markup). The tag checks see only the tags of this clone: run git fetch --tags first.
# Exit codes: 0 done; 1 a check failed (not above the current version or the newest release tag, the tag exists,
# uncommitted changes in a file to change, sources at different versions); 2 bad input (usage, version, notes).
# Nothing is written unless every check passed.
set -eu
root=${KUICKSHARE_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
prog=version-bump
fail() { echo "$prog: $*" >&2; exit 1; }
bad() { echo "$prog: $*" >&2; exit 2; }

[ $# -eq 2 ] || bad "usage: build/release/version-bump.sh X.Y.Z NOTES"
new=$1
notes=$2
[ -f "$notes" ] || bad "the notes file $notes does not exist"
notes=$(cd "$(dirname "$notes")" && pwd)/$(basename "$notes")
case $new in *[!0-9.]* | '') bad "$new is not X.Y.Z (numbers without leading zeros)" ;; esac
printf '%s\n' "$new" | grep -Eqx '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)' || bad "$new is not X.Y.Z (numbers without leading zeros)"
cd "$root"
git rev-parse --git-dir > /dev/null 2>&1 || bad "$root is not a git checkout"
meta=data/io.github.the_beper.KuickShare.metainfo.xml
cml=src/shell/CMakeLists.txt
for f in Cargo.toml Cargo.lock "$cml" "$meta"; do [ -f "$f" ] || fail "$f is missing"; done

# The readers of check-version.sh, so what is written here is what the release checks.
cargo_v() { sed -n 's/^version = "\(.*\)"/\1/p;/^\[dependencies\]/q' "$1" | head -1; }
cmake_v() { sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' "$1"; }
meta_v() { sed -n 's/^ *<release version="\([^"]*\)".*/\1/p' "$1" | head -1; }
lock_v() { awk '$0 == "name = \"kuickshare-core\"" { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }' "$1"; }

cur=$(cargo_v Cargo.toml)
[ -n "$cur" ] || fail "Cargo.toml has no version"
for pair in "$cml:$(cmake_v "$cml")" "the first metainfo <release>:$(meta_v "$meta")" "kuickshare-core in Cargo.lock:$(lock_v Cargo.lock)"; do
    [ "${pair##*:}" = "$cur" ] || fail "${pair%:*} says '${pair##*:}', Cargo.toml says '$cur'"
done
[ "$new" != "$cur" ] && [ "$(printf '%s\n%s\n' "$cur" "$new" | sort -V | tail -1)" = "$new" ] || fail "$new is not above the current version $cur"
if git rev-parse --verify --quiet "refs/tags/v$new" > /dev/null; then fail "the tag v$new exists already"; fi
newest=$(git tag -l 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true)
[ -z "$newest" ] || [ "$(printf '%s\n%s\n' "${newest#v}" "$new" | sort -V | tail -1)" = "$new" ] || fail "$new is not above the newest release tag $newest (check-version.sh refuses a release below it)"
[ -z "$(git status --porcelain -- Cargo.toml Cargo.lock "$cml" "$meta")" ] || fail "Cargo.toml, Cargo.lock, $cml or the metainfo has uncommitted changes: commit or drop them first"

date=$(date -u +%F)
t=$(mktemp -d "${TMPDIR:-/tmp}/version-bump.XXXXXX")
trap 'rm -rf "$t"' EXIT

awk 'function esc(s) { gsub(/&/, "\\&amp;", s); gsub(/</, "\\&lt;", s); gsub(/>/, "\\&gt;", s); return s }
    { sub(/\r$/, ""); gsub(/\t/, " "); sub(/ +$/, "") }
    /[[:cntrl:]]/ { print "line " NR " has a control character" > "/dev/stderr"; err = 1; exit 1 }
    /^ *$/ { next }
    !p { if ($0 ~ /^- /) { print "the first line must be the summary, not an item" > "/dev/stderr"; err = 1; exit 1 }
         s = $0; sub(/^ +/, "", s); p = 1; print "        <p>" esc(s) "</p>"; print "        <ul>"; next }
    /^- / { s = substr($0, 3); sub(/^ +/, "", s); print "          <li>" esc(s) "</li>"; n++; next }
    { print "line " NR " is not an item (\"- text\"): " $0 > "/dev/stderr"; err = 1; exit 1 }
    END { if (err) exit 1; if (!p || !n) { print "the notes need a summary and at least one item" > "/dev/stderr"; exit 1 } print "        </ul>" }' \
    "$notes" > "$t/desc" || bad "bad notes in $notes"

awk -v cur="$cur" -v new="$new" '!done && $0 == "version = \"" cur "\"" { $0 = "version = \"" new "\""; done = 1 } { print }' Cargo.toml > "$t/toml"
awk -v cur="$cur" -v new="$new" '$0 == "name = \"kuickshare-core\"" { n = 1 } n && $0 == "version = \"" cur "\"" { $0 = "version = \"" new "\""; n = 0 } { print }' Cargo.lock > "$t/lock"
awk -v cur="$cur" -v new="$new" 'BEGIN { p = "project(kuickshare VERSION " }
    !done && index($0, p cur) == 1 { rest = substr($0, length(p cur) + 1); if (rest == "" || rest ~ /^[ )]/) { $0 = p new rest; done = 1 } } { print }' "$cml" > "$t/cml"
awk -v new="$new" -v date="$date" -v desc="$t/desc" '
    /^  <releases>/ && !done { print; print "    <release version=\"" new "\" date=\"" date "\">"; print "      <description>"
        while ((getline l < desc) > 0) print l; print "      </description>"; print "    </release>"; done = 1; next }
    { print }' "$meta" > "$t/meta"
grep -q "<release version=\"$new\"" "$t/meta" || fail "no <releases> block found in $meta"

# Read the new files back as check-version.sh does, before any is written.
for pair in "Cargo.toml:$(cargo_v "$t/toml")" "$cml:$(cmake_v "$t/cml")" "the first metainfo <release>:$(meta_v "$t/meta")" "kuickshare-core in Cargo.lock:$(lock_v "$t/lock")"; do
    [ "${pair##*:}" = "$new" ] || fail "could not set the version in ${pair%:*} (it would say '${pair##*:}')"
done

cp "$t/toml" Cargo.toml && cp "$t/lock" Cargo.lock && cp "$t/cml" "$cml" && cp "$t/meta" "$meta" || fail "could not write the files; undo with: git checkout -- Cargo.toml Cargo.lock $cml $meta"

echo "$prog: $cur -> $new in Cargo.toml, Cargo.lock, $cml and $meta ($date)"
echo "next: review git diff, commit (\"Release $new\"), push it to main, then tag v$new (CONTRIBUTING.md, Making a release)"
