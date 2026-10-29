#!/bin/sh
# Stages dist/tmp/host: the program built against the system's Qt 6 / KDE Frameworks 6 (inside the build container,
# started automatically), with the installer and the data files. It is what the tarball installs into ~/.local
# and its base (build/build-tarball.sh).
#   dist/tmp/host/usr/share/kuickshare/host/bin/{kuickshare,kuickshared}
#   dist/tmp/host/usr/share/kuickshare/{install.sh,VERSION,*.desktop,*.metainfo.xml,kuickshare.notifyrc,icons/,locale/}
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
# Run inside the build container (podman creates /run/.containerenv).
[ -e /run/.containerenv ] || exec "$root/build/build-env.sh" "$root/build/build-host-payload.sh"
version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)

mkdir -p dist/tmp
cargo build --release --locked
cmake -S src/shell -B dist/tmp/cmake-release -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=OFF
cmake --build dist/tmp/cmake-release

stage=dist/tmp/host
share=$stage/usr/share/kuickshare
rm -rf "$stage"
mkdir -p "$share/host/bin" "$share/icons"
install -m755 target/release/kuickshared "$share/host/bin/kuickshared"
install -m755 dist/tmp/cmake-release/kuickshare "$share/host/bin/kuickshare"
strip --strip-unneeded "$share/host/bin/kuickshared" "$share/host/bin/kuickshare"
install -m755 build/appimage/install.sh "$share/install.sh"
echo "$version" > "$share/VERSION"
cp data/io.github.the_beper.KuickShare.desktop data/io.github.the_beper.KuickShare.metainfo.xml data/kuickshare-send.desktop data/kuickshare.notifyrc "$share/"
cp data/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg \
    data/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg "$share/icons/"

# Translations: compile the catalogs to the layout KDE looks for under ~/.local/share/locale.
for lang in $(cat po/LINGUAS); do
    mkdir -p "$share/locale/$lang/LC_MESSAGES"
    msgfmt --check-format -o "$share/locale/$lang/LC_MESSAGES/kuickshare.mo" "po/$lang.po"
done
echo "Staged $stage ($version)"
