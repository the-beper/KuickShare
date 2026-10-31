#!/bin/sh
# Checks version-bump.sh on a small checkout of its own (a git repository with Cargo.toml, Cargo.lock, CMakeLists.txt
# and the metainfo): the four places move together, the notes become the new <release>, and every refusal leaves the
# files as they were. No network, no container.
#   build/release/version-bump-test.sh
set -eu
unset KUICKSHARE_ROOT
here=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d "${TMPDIR:-/tmp}/version-bump-test.XXXXXX")
trap 'rm -rf "$t"' EXIT
failed=0
ok() { echo "ok   $1"; }
nok() {
    echo "FAIL $1"
    failed=1
}
same() { if [ "$2" = "$3" ]; then ok "$1"; else nok "$1 (expected '$2', got '$3')"; fi; }
meta=data/io.github.the_beper.KuickShare.metainfo.xml
today=$(date -u +%F)

# fixture VERSION: a fresh checkout at VERSION, committed; a dependency shares the version string.
fixture() {
    r=$t/repo
    rm -rf "$r"
    mkdir -p "$r/build/release" "$r/src/shell" "$r/data"
    cp "$here/version-bump.sh" "$r/build/release/"
    printf '[package]\nname = "kuickshare-core"\nversion = "%s"\nedition = "2024"\n\n[dependencies]\nzed = { version = "%s" }\n' "$1" "$1" > "$r/Cargo.toml"
    for n in kuickshare-core zed; do printf '[[package]]\nname = "%s"\nversion = "%s"\n\n' "$n" "$1"; done > "$r/Cargo.lock"
    printf 'cmake_minimum_required(VERSION 3.24)\nproject(kuickshare VERSION %s LANGUAGES CXX)\n' "$1" > "$r/src/shell/CMakeLists.txt"
    cat > "$r/$meta" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <releases>
    <release version="$1" date="2026-01-02">
      <description>
        <p>Older notes.</p>
        <ul>
          <li>Old item.</li>
        </ul>
      </description>
    </release>
  </releases>
</component>
EOF
    git -C "$r" init -q
    git -C "$r" add -A
    git -C "$r" -c user.name=t -c user.email=t@t commit -q -m fixture
}
bump() { (cd "$r" && sh build/release/version-bump.sh "$@") > "$t/out" 2>&1; }
state() { cat "$r/Cargo.toml" "$r/Cargo.lock" "$r/src/shell/CMakeLists.txt" "$r/$meta" | sha256sum; }
lockv() { awk -v n="$1" '$0 == "name = \"" n "\"" { getline; sub(/^version = "/, ""); sub(/"$/, ""); print }' "$r/Cargo.lock"; }
refused() { # refused NAME CODE ARGS...: the exit code, and nothing changed
    name=$1
    code=$2
    shift 2
    before=$(state)
    rc=0
    bump "$@" || rc=$?
    same "$name: exit $code" "$code" "$rc"
    same "$name: files unchanged" "$before" "$(state)"
}

printf 'Faster & <safer> sends.\n\n- First item.\n- Second "item" with < and &.\n' > "$t/notes"

fixture 1.5.0
bump 1.6.0 "$t/notes" || nok "1.5.0 -> 1.6.0 failed: $(cat "$t/out")"
same "Cargo.toml" 'version = "1.6.0"' "$(grep '^version' "$r/Cargo.toml")"
same "the dependency in Cargo.toml" 'zed = { version = "1.5.0" }' "$(grep '^zed' "$r/Cargo.toml")"
same "Cargo.lock kuickshare-core" 1.6.0 "$(lockv kuickshare-core)"
same "Cargo.lock dependency" 1.5.0 "$(lockv zed)"
same "CMakeLists.txt" 'project(kuickshare VERSION 1.6.0 LANGUAGES CXX)' "$(grep '^project' "$r/src/shell/CMakeLists.txt")"
same "new first <release>" "<release version=\"1.6.0\" date=\"$today\">" "$(awk '/<release / { print; exit }' "$r/$meta" | sed 's/^ *//')"
same "summary, escaped" '<p>Faster &amp; &lt;safer&gt; sends.</p>' "$(grep -m1 '<p>' "$r/$meta" | sed 's/^ *//')"
same "items, escaped" '<li>First item.</li>|<li>Second "item" with &lt; and &amp;.</li>|<li>Old item.</li>' "$(grep '<li>' "$r/$meta" | sed 's/^ *//' | paste -sd'|')"
same "the old entry stays" 2 "$(grep -c '<release ' "$r/$meta")"
if command -v python3 > /dev/null; then
    same "release-notes.py reads the items" "$(printf '%s\n\n%s\n%s\n\n%s' "## What's Changed" "- First item." "- Second \"item\" with < and &." "**Full Changelog**: https://github.com/o/n/compare/v1.5.0...v1.6.0")" "$(python3 -I "$here/release-notes.py" "$r/$meta" 1.6.0 --repo o/n 2>&1)"
fi
git -C "$r" -c user.name=t -c user.email=t@t commit -qam bump
git -C "$r" tag v1.6.0
git -C "$r" update-ref refs/remotes/origin/main HEAD
rc=0
(RELEASE_TAG=v1.6.0 GITHUB_SHA=$(git -C "$r" rev-parse HEAD) KUICKSHARE_ROOT=$r sh "$here/check-version.sh" > "$t/out" 2>&1) || rc=$?
same "check-version.sh accepts the result: $(cat "$t/out")" 0 "$rc"

fixture 1.5.0
refused "usage" 2 1.6.0
refused "same version" 1 1.5.0 "$t/notes"
refused "lower version" 1 1.4.9 "$t/notes"
for v in 1.6 v1.6.0 01.6.0 1.6.0-rc.1 "1.6.0 "; do refused "version '$v'" 2 "$v" "$t/notes"; done
refused "missing notes file" 2 1.6.0 "$t/none"
printf -- '- An item first.\n' > "$t/n1"
refused "notes start with an item" 2 1.6.0 "$t/n1"
printf 'Summary.\nNot an item.\n' > "$t/n2"
refused "a line that is not an item" 2 1.6.0 "$t/n2"
printf 'Summary only.\n' > "$t/n3"
refused "notes without items" 2 1.6.0 "$t/n3"
git -C "$r" tag v1.6.0
refused "the tag exists" 1 1.6.0 "$t/notes"
git -C "$r" tag -d v1.6.0 > /dev/null
echo "# edit" >> "$r/src/shell/CMakeLists.txt"
refused "uncommitted CMakeLists.txt" 1 1.6.0 "$t/notes"
fixture 1.5.0
sed -i 's/VERSION 1.5.0/VERSION 1.5.1/' "$r/src/shell/CMakeLists.txt"
git -C "$r" -c user.name=t -c user.email=t@t commit -qam drift
refused "sources at different versions" 1 1.6.0 "$t/notes"
for f in Cargo.lock "$meta"; do
    fixture 1.5.0
    sed -i '0,/1\.5\.0/s//1.5.1/' "$r/$f"
    git -C "$r" -c user.name=t -c user.email=t@t commit -qam drift
    refused "$f at another version" 1 1.6.0 "$t/notes"
done
fixture 1.5.0
sed -i 's/^  <releases>/  <releasez>/' "$r/$meta"
git -C "$r" -c user.name=t -c user.email=t@t commit -qam noblock
refused "no <releases> block" 1 1.6.0 "$t/notes"
fixture 1.5.0
git -C "$r" tag v1.7.0
refused "below the newest release tag" 1 1.6.0 "$t/notes"
printf 'Summary.\n- An item with a \001 control character.\n' > "$t/n5"
refused "a control character in the notes" 2 1.8.0 "$t/n5"
refused "a version with a newline" 2 "$(printf '1.8.0\nx')" "$t/notes"
fixture 1.5.0
printf 'cmake_minimum_required(VERSION 3.24)\nproject(kuickshare VERSION 1.5.0)\n' > "$r/src/shell/CMakeLists.txt"
git -C "$r" -c user.name=t -c user.email=t@t commit -qam short
bump 1.6.0 "$t/notes" || nok "a project() line without LANGUAGES failed: $(cat "$t/out")"
same "a project() line without LANGUAGES" 'project(kuickshare VERSION 1.6.0)' "$(grep '^project' "$r/src/shell/CMakeLists.txt")"

[ "$failed" = 0 ] && echo "version-bump-test: all passed" || { echo "version-bump-test: FAILED"; exit 1; }
