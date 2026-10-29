#!/bin/sh
# Smoke test of the tarball installer in the build container (it has the Qt 6 / KF6 libraries the host build needs),
# with a throw-away HOME: install, update, start the binaries, uninstall with --purge and check nothing is left.
#   [KUICKSHARE_EXPECT_VERSION=1.2.3] build/test-tarball.sh dist/KuickShare-x86_64.tar.gz
# KUICKSHARE_EXPECT_VERSION (optional): the version `--version` must print. main.cpp has no flag of its own: it comes
# from KAboutData::setupCommandLine, which prints "kuickshare <version>" (a "kuickshare: <version>" line is accepted too).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
if [ ! -e /run/.containerenv ]; then
    tarball=$(readlink -f "$1")
    exec "$root/build/build-env.sh" env "TARBALL=$tarball" "KUICKSHARE_EXPECT_VERSION=${KUICKSHARE_EXPECT_VERSION:-}" "$root/build/test-tarball.sh"
fi

fail() { echo "FAIL: $*" >&2; exit 1; }
TARBALL=${TARBALL:-$(readlink -f "$1")} # set by the launcher above; CI already runs inside the container
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# A data folder with characters that need quoting in an Exec line and in a shell word.
export HOME=$work/home XDG_DATA_HOME="$work/my data|&\$x/share" XDG_CONFIG_HOME= QT_QPA_PLATFORM=offscreen
unset XDG_CONFIG_HOME
mkdir -p "$HOME" "$work/src"
tar -xzf "$TARBALL" -C "$work/src"
installer=$(ls -d "$work"/src/*/install.sh)

"$installer" --no-launch >/dev/null
app=$XDG_DATA_HOME/kuickshare
[ -x "$app/bin/kuickshare" ] && [ -x "$app/bin/kuickshared" ] || fail "binaries not installed"
[ -x "$XDG_DATA_HOME/kio/servicemenus/kuickshare-send.desktop" ] || fail "service menu missing or not executable"
# In an Exec line the dollar sign is escaped.
want=$(printf '%s' "$app/bin/kuickshare" | sed 's/[$]/\\&/g')
grep -qF "Exec=\"$want\" %F" "$XDG_DATA_HOME/kio/servicemenus/kuickshare-send.desktop" \
    || fail "Exec line of the service menu: $(grep '^Exec=' "$XDG_DATA_HOME/kio/servicemenus/kuickshare-send.desktop")"
grep -q '^Exec=' "$XDG_DATA_HOME/applications/io.github.the_beper.KuickShare.desktop" || fail "menu entry has no Exec"
"$app/bin/kuickshare" --help >/dev/null 2>&1 || fail "the installed kuickshare does not run"
if [ -n "${KUICKSHARE_EXPECT_VERSION:-}" ]; then
    out=$("$app/bin/kuickshare" --version 2>&1) || fail "--version failed: $out"
    printf '%s\n' "$out" | grep -qxF -e "kuickshare $KUICKSHARE_EXPECT_VERSION" -e "kuickshare: $KUICKSHARE_EXPECT_VERSION" \
        || fail "--version is not $KUICKSHARE_EXPECT_VERSION: $out"
    echo "ok: version $KUICKSHARE_EXPECT_VERSION"
fi
echo "ok: installed"

"$installer" --no-launch >/dev/null
[ -x "$app/bin/kuickshare" ] || fail "update broke the installation"
echo "ok: updated"

"$app/uninstall.sh" --purge >/dev/null
left=$(find "$HOME" -type f -not -path '*/.cache/*' | head -3)
[ -z "$left" ] || fail "files left behind: $left"
echo "ok: uninstalled, nothing left"
