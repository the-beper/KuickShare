#!/bin/sh
# Stages dist/tmp/host: the program built against the system's Qt 6 / KDE Frameworks 6 (inside the build container,
# started automatically), with the installer and the data files. It is what the tarball installs into ~/.local
# and its base (build/build-tarball.sh).
#   dist/tmp/host/usr/share/kuickshare/host/bin/{kuickshare,kuickshared}
#   dist/tmp/host/usr/share/kuickshare/{install.sh,VERSION,*.desktop,*.metainfo.xml,kuickshare.notifyrc,icons/,locale/}
# The installed files all come from one `cmake --install` (src/shell/CMakeLists.txt), so a file added to its
# install rules has to be mapped here too: the `stage` helper fails when the mapping misses one.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
# Run inside the build container (podman creates /run/.containerenv).
[ -e /run/.containerenv ] || exec "$root/build/build-env.sh" "$root/build/build-host-payload.sh"

version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)

mkdir -p dist/tmp
cargo build --release --locked
cmake -S src/shell -B dist/tmp/cmake-release -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=OFF \
    -DKUICKSHARED_PATH="$root/target/release/kuickshared"
cmake --build dist/tmp/cmake-release

# One install: prefix /usr like the packages get, executables stripped, into a DESTDIR.
tree=dist/tmp/installtree
rm -rf "$tree"
DESTDIR="$root/$tree" cmake --install dist/tmp/cmake-release --prefix /usr --strip

# The layout install.sh works with (files gathered from where CMake put them in the DESTDIR tree):
#   stage <installed path> <payload path> <mode>   fail when the installed file is not there, and
# record the mapping (payload path relative to $stage) for the comparison at the bottom.
stage=dist/tmp/host
share=$stage/usr/share/kuickshare
host=$share/host/bin
rm -rf dist/tmp/host
mkdir -p "$host" "$share/icons"
: > dist/tmp/mapping
stage() {
    [ -f "$tree/$1" ] || { echo "build-host-payload: $1 is not in the installed tree; add it to the install rules or to this mapping" >&2; exit 1; }
    install -m "$3" "$tree/$1" "$2"
    echo "${2#"$stage"/}:$1" >> dist/tmp/mapping
}
stage usr/bin/kuickshare "$host/kuickshare" 755
stage usr/bin/kuickshared "$host/kuickshared" 755
stage usr/share/knotifications6/kuickshare.notifyrc "$share/kuickshare.notifyrc" 644
stage usr/share/applications/io.github.the_beper.KuickShare.desktop "$share/io.github.the_beper.KuickShare.desktop" 644
stage usr/share/metainfo/io.github.the_beper.KuickShare.metainfo.xml "$share/io.github.the_beper.KuickShare.metainfo.xml" 644
stage usr/share/kio/servicemenus/kuickshare-send.desktop "$share/kuickshare-send.desktop" 755
stage usr/share/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg "$share/icons/io.github.the_beper.KuickShare.svg" 644
stage usr/share/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg "$share/icons/io.github.the_beper.KuickShare-symbolic.svg" 644

install -m755 build/appimage/install.sh "$share/install.sh"
echo "$version" > "$share/VERSION"

# Translations: the catalogs CMake compiled, at the layout KDE looks for under ~/.local/share/locale.
for lang in $(cat po/LINGUAS); do
    mkdir -p "$share/locale/$lang/LC_MESSAGES"
    stage "usr/share/locale/$lang/LC_MESSAGES/kuickshare.mo" "$share/locale/$lang/LC_MESSAGES/kuickshare.mo" 644
done

# Anything else the install rules added would be missing from the package: resolve every staged file
# through the recorded mapping to the path it claims to have come from, and compare that with what
# the install rules actually put in the DESTDIR tree (the installer and VERSION are payload-only).
# awk, sort and comm: the older published fedora45 image the tarball job runs in has no diff (and no
# cmp) — the first CI run died exactly there, with the mapping itself correct.
find "$stage/usr" -type f | sed "s|^$stage/||" | \
    grep -Ev '^usr/share/kuickshare/(install.sh|VERSION)$' | sort | \
    awk -F: 'NR == FNR { key[$1] = $2; next } ($0 in key) { print key[$0] }
    !($0 in key) { print "build-host-payload: no mapping for " $0 > "/dev/stderr"; fail = 1 }
    END { exit fail }' dist/tmp/mapping - | sort > dist/tmp/mapped.list
find "$tree/usr" -type f | sed "s|^$tree/||" | sort > dist/tmp/install-tree.list
if [ -n "$(comm -3 dist/tmp/mapped.list dist/tmp/install-tree.list)" ]; then
    comm -3 dist/tmp/mapped.list dist/tmp/install-tree.list >&2
    echo "build-host-payload: the install tree and the staging differ: fix the mapping" >&2
    exit 1
fi

echo "Staged $stage ($version)"
