#!/bin/sh
# Smoke test of the deb in bare Ubuntu containers: install it with apt with recommends off (the recipe's
# Depends list must be enough; that is exactly what apt resolves), run offscreen under a session bus
# (version, the core on D-Bus, QML error greps), uninstall and find nothing left of it.
#     [KUICKSHARE_EXPECT_VERSION=1.2.3] build/packaging/test-deb.sh dist/KuickShare-x86_64.deb
set -eu

if [ ! -e /run/.containerenv ]; then
    root=$(cd "$(dirname "$0")/../.." && pwd)
    pkg=$(readlink -f "$1")
    shift
    [ $# -gt 0 ] || set -- ubuntu
    failed=
    for target in "$@"; do
        case $target in
            ubuntu) image=docker.io/library/ubuntu:26.04
                # dbus-daemon carries dbus-run-session and dbus-send; dejavu is a font Qt can measure.
                setup='export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq dbus dbus-daemon fonts-dejavu-core' ;;
            *) echo "unknown target $target" >&2; exit 2 ;;
        esac
        echo "=== $target ($image)"
        podman run --rm --security-opt label=disable -e KUICKSHARE_EXPECT_VERSION \
            -v "$pkg:/pkg.deb:ro" -v "$root/build/packaging/test-deb.sh:/test.sh:ro" \
            "$image" sh -c "$setup >/dev/null 2>&1 && exec sh /test.sh /pkg.deb" || failed="$failed $target"
    done
    [ -z "$failed" ] || { echo "FAILED:$failed" >&2; exit 1; }
    echo "all passed: $*"
    exit 0
fi

# ---- inside the container ----
id=io.github.the_beper.KuickShare
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
export QT_QPA_PLATFORM=offscreen QT_FORCE_STDERR_LOGGING=1
[ -e /run/dbus ] || mkdir -p /run/dbus
export XDG_RUNTIME_DIR=/tmp/xdg-run
# shellcheck disable=SC2174 # only the last component needs the mode; the script runs twice (dbus-run-session)
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
HOME=$(mktemp -d) # a home the app only writes its own data (and the log) into
export HOME

if [ "${KS_INNER:-}" != 1 ]; then
    exec env KS_INNER=1 dbus-run-session -- sh "$0" "$1"
fi

owner() {
    dbus-send --session --print-reply --dest=$id /io/github/the_beper/KuickShare \
        org.freedesktop.DBus.Properties.Get string:${id}1 string:DeviceName 2>/dev/null
}
files() { (cd "$HOME" && find . -mindepth 1 -type f | sort); }

export DEBIAN_FRONTEND=noninteractive
apt-get install -y -qq --no-install-recommends /pkg.deb || { apt-get install /pkg.deb >/dev/null 2>&1 || fail "apt-get install failed"; }
ok "installed"

if [ -n "${KUICKSHARE_EXPECT_VERSION:-}" ]; then
    out=$(/usr/bin/kuickshare --version 2>&1) || fail "--version failed: $out"
    printf '%s\n' "$out" | grep -qxF -e "kuickshare $KUICKSHARE_EXPECT_VERSION" -e "kuickshare: $KUICKSHARE_EXPECT_VERSION" \
        || fail "--version is not $KUICKSHARE_EXPECT_VERSION: $out"
    ok "version $KUICKSHARE_EXPECT_VERSION"
fi
/usr/bin/kuickshare --background >"$HOME/app.log" 2>&1 &
pid=$!
for _ in $(seq 40); do owner >/dev/null && break; sleep 1; done
owner >/dev/null || { cat "$HOME/app.log"; fail "the core did not answer on D-Bus"; }
ok "core answers: $(owner | tail -1)"
! grep -E 'Type [A-Za-z.0-9]+ unavailable|is not installed|[Cc]annot load|[Cc]ould not load|failed to load' "$HOME/app.log" \
    || { cat "$HOME/app.log"; fail "QML or plugin errors"; }
ok "no QML or plugin errors"
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
for _ in $(seq 10); do owner >/dev/null 2>&1 || break; sleep 1; done

dpkg -P kuickshare || fail "dpkg -P failed"
# Also take the packages apt had installed for us — the ones from the recipes Depends only.
apt-get autoremove -y -qq
[ ! -e /usr/bin/kuickshare ] && [ ! -e /usr/bin/kuickshared ] || fail "the binaries are still installed"
for path in \
    "/usr/share/applications/io.github.the_beper.KuickShare.desktop" \
    "/usr/share/metainfo/io.github.the_beper.KuickShare.metainfo.xml" \
    "/usr/share/knotifications6/kuickshare.notifyrc" \
    "/usr/share/kio/servicemenus/kuickshare-send.desktop" \
    "/usr/share/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg" \
    "/usr/share/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg"; do
    [ ! -e "$path" ] || fail "$path is still installed after dpkg -P"
done
[ -z "$(find /usr/share/locale -name kuickshare.mo 2>/dev/null)" ] \
    || fail "a .mo catalog is still installed after dpkg -P"
bad=$(files | grep -Ev '^\./\.(local/share|config)/kuickshare/|^\./app\.log$' || true)
[ -z "$bad" ] || fail "files outside the app's own data: $bad"
ok "clean uninstall, nothing of ours in HOME (or just the app's own data)"
