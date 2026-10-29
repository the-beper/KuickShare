#!/bin/sh
# Smoke test of the self-contained AppImage in throw-away containers, offscreen Qt, private session bus.
# Bare distros (no Qt, no KF6, only dbus and fonts): debian:stable-slim, alpine (musl), nixos/nix (no FHS):
#   - ubuntu (22.04, Xvfb, no Wayland): the main window appears within 10 s, so the xcb platform plugin and the
#     libraries Qt dlopens for it (libxkbcommon-x11, libxcb-xkb) are in the bundle
#   - direct launch: the core answers on D-Bus (DeviceName), no QML errors, no files outside the app's own
#     settings/data under HOME, a second launch hands its file to the running instance
# Usage: [KUICKSHARE_EXPECT_VERSION=1.2.3] build/appimage/test-appimage.sh dist/KuickShare-x86_64.AppImage [debian alpine nix ubuntu]
# KUICKSHARE_EXPECT_VERSION (optional): the version `--version` must print. main.cpp has no flag of its own: it comes
# from KAboutData::setupCommandLine, which prints "kuickshare <version>" (a "kuickshare: <version>" line is accepted too).
# Needs no FUSE: the AppImage runs with --appimage-extract-and-run. Nothing is installed on the host.
set -eu

if [ ! -e /run/.containerenv ]; then
    root=$(cd "$(dirname "$0")/../.." && pwd)
    appimage=$(readlink -f "$1")
    shift
    [ $# -gt 0 ] || set -- debian alpine nix ubuntu
    failed=
    for target in "$@"; do
        case $target in
            debian) image=docker.io/library/debian:stable-slim
                setup='export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq dbus fonts-dejavu-core >/dev/null 2>&1' ;;
            alpine) image=docker.io/library/alpine:latest
                setup='apk add --no-cache -q dbus ttf-dejavu' ;;
            nix) image=docker.io/nixos/nix:latest
                setup='nix-env -iA nixpkgs.dbus nixpkgs.dejavu_fonts nixpkgs.gnused nixpkgs.gnugrep >/dev/null 2>&1' ;;
            ubuntu) image=docker.io/library/ubuntu:22.04
                setup='export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq dbus fonts-dejavu-core xvfb libgl1-mesa-dri libegl1 x11-utils >/dev/null' ;;
            *) echo "unknown target $target" >&2; exit 2 ;;
        esac
        echo "=== $target ($image)"
        podman run --rm --security-opt label=disable -e KUICKSHARE_EXPECT_VERSION \
            -v "$appimage:/app.AppImage:ro" -v "$root/build/appimage/test-appimage.sh:/test.sh:ro" \
            "$image" sh -c "$setup && exec sh /test.sh /app.AppImage" || failed="$failed $target"
    done
    [ -z "$failed" ] || { echo "FAILED:$failed" >&2; exit 1; }
    echo "all passed: $*"
    exit 0
fi

# ---- inside the container ----
id=io.github.the_beper.KuickShare
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
app=$(mktemp -d)/app.AppImage
cp "$1" "$app" # a copy the runtime can extract next to (APPIMAGE must name an existing file)
export APPIMAGE_EXTRACT_AND_RUN=1 QT_QPA_PLATFORM=offscreen QT_FORCE_STDERR_LOGGING=1
[ -e /run/dbus ] || mkdir -p /run/dbus
export XDG_RUNTIME_DIR=/tmp/xdg-run
# shellcheck disable=SC2174 # only the last component needs the mode; the script runs twice (dbus-run-session), so -p stays
mkdir -p -m 700 "$XDG_RUNTIME_DIR"

if [ "${KS_INNER:-}" != 1 ]; then
    # nix has no /etc/dbus-1: use the session.conf of the dbus package
    conf=
    [ -e /etc/dbus-1/session.conf ] || [ -e /usr/share/dbus-1/session.conf ] || conf=--config-file=/root/.nix-profile/share/dbus-1/session.conf
    exec env KS_INNER=1 dbus-run-session $conf -- sh "$0" "$1"
fi

newhome() { HOME=$(mktemp -d); export HOME; unset XDG_DATA_HOME XDG_CONFIG_HOME; }
owner() {
    dbus-send --session --print-reply --dest=$id /io/github/the_beper/KuickShare \
        org.freedesktop.DBus.Properties.Get string:${id}1 string:DeviceName 2>/dev/null
}
# The files under HOME, without the AppImage's own cache.
files() { (cd "$HOME" && find . -type f ! -path './.cache/*' | sort); }

newhome
if [ -n "${KUICKSHARE_EXPECT_VERSION:-}" ]; then
    out=$("$app" --version 2>&1) || fail "--version failed: $out"
    printf '%s\n' "$out" | grep -qxF -e "kuickshare $KUICKSHARE_EXPECT_VERSION" -e "kuickshare: $KUICKSHARE_EXPECT_VERSION" \
        || fail "--version is not $KUICKSHARE_EXPECT_VERSION: $out"
    ok "version $KUICKSHARE_EXPECT_VERSION"
fi
"$app" --background >"$HOME/app.log" 2>&1 &
pid=$!
for _ in $(seq 40); do owner >/dev/null && break; sleep 1; done
owner >/dev/null || { cat "$HOME/app.log"; fail "the core did not answer on D-Bus"; }
ok "direct launch, core answers: $(owner | tail -1)"
! grep -E 'Type [A-Za-z.0-9]+ unavailable|is not installed|[Cc]annot load|[Cc]ould not load|failed to load' "$HOME/app.log" || { cat "$HOME/app.log"; fail "QML or plugin errors"; }
ok "no QML or plugin errors"

echo hello >"$HOME/hello.txt"
timeout 60 "$app" "$HOME/hello.txt" >/dev/null 2>&1 || fail "the second launch did not hand over to the running instance"
ok "second launch hands over"

bad=$(files | grep -v -E '^\./\.(local/share|config)/kuickshare/|^\./(app\.log|hello\.txt)$' || true)
[ -z "$bad" ] || fail "files outside the app's own data: $bad"
ok "wrote only its own data: $(files | grep -c kuickshare) files"
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
for _ in $(seq 10); do owner >/dev/null 2>&1 || break; sleep 1; done

if command -v Xvfb >/dev/null; then
    newhome
    unset WAYLAND_DISPLAY QT_QPA_PLATFORM
    Xvfb :99 -screen 0 1280x800x24 >/dev/null 2>&1 &
    xpid=$!
    for _ in $(seq 10); do DISPLAY=:99 xwininfo -root >/dev/null 2>&1 && break; sleep 1; done
    ! owner >/dev/null 2>&1 || fail "the first instance is still running"
    DISPLAY=:99 "$app" >"$HOME/app.log" 2>&1 &
    pid=$!
    for _ in $(seq 10); do DISPLAY=:99 xwininfo -root -tree | grep -q '"KuickShare"' && break; sleep 1; done
    DISPLAY=:99 xwininfo -root -tree | grep -q '"KuickShare"' || { cat "$HOME/app.log"; fail "no window under Xvfb within 10 s"; }
    ok "window shown under Xvfb"
    kill "$pid" "$xpid" 2>/dev/null || true
    wait "$pid" "$xpid" 2>/dev/null || true
fi
