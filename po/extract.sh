#!/bin/sh
# Regenerates po/kuickshare.pot from the files in po/POTFILES.in and merges it into the
# translations listed in po/LINGUAS (one po/<lang>.po each):
#   build/build-env.sh po/extract.sh
set -eu
cd "$(dirname "$0")/.."

# A Rust source with a call of the macros that is not listed would lose its strings silently.
for file in $(grep -l 'i18n[a-z]*!(' src/core/*.rs || true)
do
    grep -qx "$file" po/POTFILES.in || { echo "$file calls i18n!() but is not in po/POTFILES.in" >&2; exit 1; }
done

# The Rust sources say `i18n!("…")` (macros, not calls), which xgettext does not look at. They
# go through copies that xgettext's C++ lexer reads as it would the real thing, with the same
# line numbers:
#   - `i18n!(` becomes `i18n(`, `i18nc!(` becomes `i18nc(`, and so on;
#   - a lifetime (`&'static`, `<'_>`) would open a character constant, which can swallow a
#     string on the same line: the quote goes;
#   - a `\` at the end of a line (Rust drops the indentation that follows) closes the string and
#     opens the next one on the next line, which C++ concatenates.
# A `\u{…}` escape is not understood by C++: write the character itself in a msgid.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
while IFS= read -r file
do
    [ -z "$file" ] && continue
    [ -f "$file" ] || { echo "po/POTFILES.in names a missing file: $file" >&2; exit 1; }
    mkdir -p "$tmp/$(dirname "$file")"
    case "$file" in
    *.rs)
        sed -E \
            -e 's/\bi18n([a-z]*)!\(/i18n\1(/g' \
            -e "s/([<&]|: |\\+ )'([a-z_]+)([^a-z_'])/\\1\\2\\3/g" \
            -e ':join' -e '/\\$/{N;s/\\\n[ \t]*/"\n"/;b join' -e '}' \
            "$file" > "$tmp/$file"
        ;;
    *)
        cp "$file" "$tmp/$file"
        ;;
    esac
done < po/POTFILES.in

xgettext --from-code=UTF-8 --language=C++ --add-comments=TRANSLATORS \
    --keyword=i18n:1 --keyword=i18nc:1c,2 --keyword=i18np:1,2 --keyword=i18ncp:1c,2,3 \
    --directory="$tmp" --files-from=po/POTFILES.in \
    --package-name=kuickshare --package-version=1.0.0 --copyright-holder="the-beper" \
    --msgid-bugs-address= \
    -o po/kuickshare.pot
# xgettext leaves placeholders in the first lines of the template: fill in the ones that are ours.
sed -i \
    -e 's/^# SOME DESCRIPTIVE TITLE\./# Translation template for KuickShare./' \
    -e 's/^# Copyright (C) YEAR /# Copyright (C) 2026 /' \
    -e 's/^# FIRST AUTHOR <EMAIL@ADDRESS>, YEAR\./# the-beper <193650145+the-beper@users.noreply.github.com>, 2026./' \
    po/kuickshare.pot
for lang in $(cat po/LINGUAS); do
    msgmerge --update --backup=none --quiet "po/$lang.po" po/kuickshare.pot
    # Drop the entries of strings that are gone, instead of keeping them as obsolete (#~) ones.
    msgattrib --no-obsolete --output-file="po/$lang.po" "po/$lang.po"
done
