#!/bin/sh
# Smoke test of the Arch package in bare Arch containers: first pacman installs what the PKGBUILD lists
# as depends (the same names), then the package file itself, then a run offscreen under a session bus
# (version, the core on D-Bus, QML error greps). The binary uninstall leaves nothing behind. Run offscreen: QT_QPA_PLATFORM=offscreen, no window server needed; the core starts up alone.
# For the *installed* files the smoke test proves the packaged set only.
#     [KUICKSHARE_EXPECT_VERSION=1.2.3] build/packaging/test-arch.sh dist/KuickShare-x86_64.pkg.tar.zst
set -eu

if [ ! -e /run/.containerenv ]; then
    root=$(cd "$(dirname "$0")/../.." && pwd)
    pkg=$(readlink -f "$1")
    shift
    [ $# -gt 0 ] || set -- arch
    failed=
    for target in "$@"; do
        case $target in
            arch) image=docker.io/library/archlinux:latest
                # dbus for the run (session bus), dejavu for a font Qt can measure with; the depends
                # list below must be exactly the PKGBUILD's: pacman -U only checks it, the run proves it.
                setup='pacman-key --init; pacman -Syu --noconfirm --needed dbus ttf-dejavu
                       pacman -S --noconfirm --needed \
                           dbus qt6-base qt6-declarative qt6-svg kirigami kirigami-addons ki18n kcoreaddons '
                setup="$setup kcrash kdbusaddons kjobwidgets knotifications kstatusnotifieritem qqc2-desktop-style" ;;
            *) echo "unknown target $target" >&2; exit 2 ;;
        esac
        echo "=== $target ($image)"
        podman run --rm --security-opt label=disable -e KUICKSHARE_EXPECT_VERSION \
            -v "$pkg:/pkg.pkg.tar.zst:ro" -v "$root/build/packaging/test-arch.sh:/test.sh:ro" \
            "$image" sh -c "$setup >/dev/null 2>&1 && exec sh /test.sh /pkg.pkg.tar.zst" || failed="$failed $target"
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

pacman -U --noconfirm /pkg.pkg.tar.zst || fail "pacman -U failed"
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

# -Rns also removes exactly what the package needed; the leftovers check is below.
pacman -Rns --noconfirm kuickshare || fail "pacman -Rns failed"
[ ! -e /usr/bin/kuickshare ] && [ ! -e /usr/bin/kuickshared ] || fail "the binaries are still installed"
for path in \
    "/usr/share/applications/io.github.the_beper.KuickShare.desktop" \
    "/usr/share/metainfo/io.github.the_beper.KuickShare.metainfo.xml" \
    "/usr/share/knotifications6/kuickshare.notifyrc" \
    "/usr/share/kio/servicemenus/kuickshare-send.desktop" \
    "/usr/share/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg" \
    "/usr/share/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg"; do
    [ ! -e "$path" ] || fail "$path is still installed after pacman -Rns"
done
[ -z "$(find /usr/share/locale -name kuickshare.mo 2>/dev/null)" ] \
    || fail "a .mo catalog is still installed after pacman -Rns"
bad=$(files | grep -Ev '^\./\.(local/share|config)/kuickshare/|^\./app\.log$' || true)
[ -z "$bad" ] || fail "files outside the app's own data: $bad"
ok "clean uninstall, nothing of ours in HOME (or just the app's own data)"
