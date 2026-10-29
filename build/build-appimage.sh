#!/bin/sh
# Builds dist/KuickShare-x86_64.AppImage (no version in the name: releases/latest/download/ stays a stable link) and its .zsync file, the "Anylinux" way
# (https://github.com/pkgforge-dev/Anylinux-AppImages): a self-contained AppImage that ships its own ld.so, libc,
# Qt 6, KF6 and QML modules, so it runs on any distribution, including NixOS and musl ones (Alpine), without FHS.
#
# Pipeline:
#   1. In the Arch container (build/appimage/Containerfile.arch) cargo builds kuickshared and cmake builds kuickshare.
#   2. quick-sharun (pinned by SHA-256 in the Containerfile; it pins and checks sharun, appimagetool and the
#      other helpers it downloads) bundles both binaries with their libraries, the Qt and KF6 plugins (the
#      platform theme plasma-integration, so the app follows the host's Breeze theme) and the few QML modules
#      the UI imports. strace mode finds what is dlopened.
#   3. Our .mo catalogs, icons, notifyrc, metainfo and the AppRun hook (build/appimage/anylinux.hook: Breeze
#      theming) are added.
#   4. appimagetool writes the AppImage with update information and the .zsync file.
# The Podman volume "kuickshare-appimage" caches the cargo registry; "podman volume rm kuickshare-appimage" clears it.
# Nothing is installed on the host.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)

if [ ! -e /run/.containerenv ]; then
    image=localhost/kuickshare-appimage:arch
    podman image exists "$image" || podman build -t "$image" -f "$root/build/appimage/Containerfile.arch" "$root/build/appimage"
    podman volume exists kuickshare-appimage || podman volume create kuickshare-appimage >/dev/null
    exec podman run --rm -i --userns=keep-id --network=host --security-opt label=disable \
        -e HOME=/cache/home -e CARGO_HOME=/cache/cargo \
        -v kuickshare-appimage:/cache -v "$root:$root" -w "$root" \
        "$image" "$root/build/build-appimage.sh"
fi

# ---- inside the Arch container ----
arch=x86_64
export CARGO_TARGET_DIR=$root/dist/tmp/arch-target
mkdir -p /cache/home dist/tmp
cargo build --release --locked
cmake -S src/shell -B dist/tmp/arch-cmake -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=OFF >/dev/null
cmake --build dist/tmp/arch-cmake

stage=$root/dist/tmp/arch-stage
export APPDIR=$root/dist/tmp/arch-appdir
rm -rf "$stage" "$APPDIR"
mkdir -p "$stage"
install -m755 "$CARGO_TARGET_DIR/release/kuickshared" dist/tmp/arch-cmake/kuickshare "$stage/"

app=io.github.the_beper.KuickShare

# QML modules: only what src/shell/qml imports, with their dependencies. DEPLOY_QML=1 would copy the whole system
# QML tree (about 450 MB with KWin, Plasma, multimedia, ...). Add a directory here if a QML error shows up
# ("module ... is not installed"; run the AppImage with QT_FORCE_STDERR_LOGGING=1 to see it).
q=/usr/lib/qt6/qml
export DEPLOY_QML=0
ADD_DIR="
$q/QtCore
$q/QtQml
$q/QtQuick
$q/Qt
$q/org/kde/kirigami
$q/org/kde/kirigamiaddons
$q/org/kde/desktop
$q/org/kde/qqc2desktopstyle
$q/org/kde/iconthemes
$q/org/kde/coreaddons
$q/org/kde/i18n
$q/org/kde/ksvg
$q/org/kde/config
$q/org/kde/guiaddons
$q/org/kde/kquickcontrols
$q/org/kde/kquickcontrolsaddons
$q/org/kde/private
$q/org/kde/sonnet
"
export ADD_DIR
# No GPU drivers in the bundle: Qt Quick uses the host's, as for any native app.
export USE_HOST_DRIVERS_EXPERIMENTAL=1
# strace mode starts kuickshare for a few seconds to see which libraries it dlopens.
export STRACE_BINARY=kuickshare STRACE_FLAGS=--background QT_QPA_PLATFORM=offscreen
export DESKTOP=$root/data/$app.desktop ICON=$root/data/icons/hicolor/scalable/apps/$app.svg
export TMPDIR
TMPDIR=$(mktemp -d) # fresh each build: quick-sharun checks the hash of what it downloads, not of cached files
trap 'rm -rf "$TMPDIR"' EXIT

dbus-run-session -- quick-sharun "$stage/kuickshare" "$stage/kuickshared"

# The shell starts the core from its applicationDirPath(), which is $APPDIR/shared/bin, where quick-sharun keeps the
# real ELF files. Executed directly they need the host's ld.so (missing on musl and NixOS), so shared/bin/kuickshared
# becomes a script that runs the core through its sharun link instead.
[ -e "$APPDIR/bin/kuickshare" ] && [ -e "$APPDIR/shared/bin/kuickshared" ] || { echo "binaries missing in $APPDIR" >&2; exit 1; }
mv "$APPDIR/shared/bin/kuickshared" "$APPDIR/shared/bin/kuickshared-core"
rm "$APPDIR/bin/kuickshared"
ln "$APPDIR/bin/kuickshare" "$APPDIR/bin/kuickshared-core" # a hardlink of sharun, like every file in bin/
printf '#!/bin/sh\nexec "${0%%/*}/../../bin/kuickshared-core" "$@"\n' >"$APPDIR/shared/bin/kuickshared"
chmod 755 "$APPDIR/shared/bin/kuickshared"
# quick-sharun flags a debug-path string inside the Rust binary as a hardcoded path and adds a hook that makes a
# symlink in /tmp for it. Nothing reads that path, so drop the hook.
rm -f "$APPDIR/bin/01-path-mapping-hardcoded.hook"

# App data where Qt and KDE look: XDG_DATA_DIRS includes $APPDIR/share.
share=$APPDIR/share
mkdir -p "$share/locale" "$share/knotifications6" "$share/metainfo" "$share/icons/hicolor/scalable/apps" "$share/icons/hicolor/symbolic/apps"
# Translations: compile the catalogs to the layout KDE looks for (msgfmt --check-format fails on a broken format string).
for lang in $(cat po/LINGUAS); do
    mkdir -p "$share/locale/$lang/LC_MESSAGES"
    msgfmt --check-format -o "$share/locale/$lang/LC_MESSAGES/kuickshare.mo" "po/$lang.po"
done
cp data/kuickshare.notifyrc "$share/knotifications6/"
cp "data/$app.metainfo.xml" "$share/metainfo/"
cp "data/icons/hicolor/scalable/apps/$app.svg" "$share/icons/hicolor/scalable/apps/"
cp "data/icons/hicolor/symbolic/apps/$app-symbolic.svg" "$share/icons/hicolor/symbolic/apps/"
# Breeze icons (compiled resource) and colour schemes, for hosts without KDE; a host's own Breeze is found as well.
mkdir -p "$share/icons/breeze" "$share/icons/breeze-dark"
cp /usr/share/icons/breeze/breeze-icons.rcc /usr/share/icons/breeze/index.theme "$share/icons/breeze/"
cp /usr/share/icons/breeze-dark/index.theme "$share/icons/breeze-dark/"
cp -r /usr/share/color-schemes "$share/"

install -m755 build/appimage/anylinux.hook "$APPDIR/bin/00-kuickshare.hook"

mkdir -p dist
out=KuickShare-$arch.AppImage
rm -f "dist/$out" "dist/$out.zsync"
OUTPATH=$root/dist OUTNAME=$out \
    UPINFO="gh-releases-zsync|the-beper|KuickShare|latest|$out.zsync" \
    quick-sharun --make-appimage
[ -f "dist/$out.zsync" ] || { echo "no .zsync file was written" >&2; exit 1; }
"dist/$out" --appimage-updateinfo | grep -q "^gh-releases-zsync|the-beper|KuickShare|latest|$out.zsync$" || { echo "wrong update information" >&2; exit 1; }
echo "Built dist/$out ($(du -h "dist/$out" | cut -f1))"
