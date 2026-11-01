#!/bin/sh
# Every automated check, the same list locally and in CI. It needs the build image (Qt, KF6, ECM,
# dbus-daemon, gettext): locally run it as `build/build-env.sh build/check.sh`.
set -eu
cd "$(dirname "$0")/.."
export QT_QPA_PLATFORM="${QT_QPA_PLATFORM:-offscreen}"

step() { printf '\n== %s\n' "$*"; }

step "rustfmt"
cargo fmt --check
(cd third_party/rqs_lib && cargo fmt --check)

step "Clippy"
cargo clippy --locked --all-targets --all-features -- -D warnings

step "Rust tests"
cargo test --locked --all-features
# The vendored library is not a workspace member; its Cargo.lock is tracked.
cargo test -p rqs_lib

step "Shell build and tests"
cmake -S src/shell -B dist/tmp/cmake-dev -G Ninja -DBUILD_TESTING=ON
cmake --build dist/tmp/cmake-dev
ctest --test-dir dist/tmp/cmake-dev --output-on-failure

step "qmllint"
qmllint-qt6 -I /usr/lib64/qt6/qml src/shell/qml/*.qml

step "Metainfo and desktop file"
xmllint --noout data/*.metainfo.xml
appstreamcli validate --no-net data/*.metainfo.xml
desktop-file-validate data/io.github.the_beper.KuickShare.desktop
# The Dolphin service menu is a KDE `Type=Service`, which desktop-file-validate rejects: check what KDE needs instead.
# Every name in Actions= has a [Desktop Action] group with Name and Exec, and every such group is listed.
awk -F= '
    /^\[/ { group = ($0 ~ /^\[Desktop Action /) ? substr($0, 17, length($0) - 17) : ($0 == "[Desktop Entry]" ? "-" : "?"); if (group !~ /^[-?]$/) have[group] = 1; next }
    group == "-" && $1 == "Type" { type = $2 }
    group == "-" && $1 == "Actions" { listed = $2 }
    group !~ /^[-?]$/ && $1 == "Name" { name[group] = 1 }
    group !~ /^[-?]$/ && $1 == "Exec" { exec_[group] = 1 }
    END {
        bad = (type != "Service") || (listed == "")
        n = split(listed, acts, ";")
        for (i = 1; i <= n; i++) if (acts[i] != "" && !(acts[i] in have)) bad = 1
        for (g in have) if (!(g in name) || !(g in exec_) || index(";" listed ";", ";" g ";") == 0) bad = 1
        exit bad
    }' data/kuickshare-send.desktop || { echo "data/kuickshare-send.desktop is not a valid service menu" >&2; exit 1; }

step "Catalogs"
while read -r l; do msgfmt -c --statistics -o /dev/null "po/$l.po"; done < po/LINGUAS

step "Flatpak pins"
# The refs pinned in build-flatpak.sh are the runtime of the manifest, and every pin is a full commit id.
manifest=build/flatpak/io.github.the_beper.KuickShare.yml
runtime=$(sed -n 's/^runtime-version: "\(.*\)"$/\1/p' "$manifest")
grep -qx "sdk=org.kde.Sdk//$runtime" build/flatpak/build-flatpak.sh
grep -qx "platform=org.kde.Platform//$runtime" build/flatpak/build-flatpak.sh
[ "$(grep -Ec '^(sdk|platform|rust)_commit=[0-9a-f]{64}$' build/flatpak/build-flatpak.sh)" = 3 ] || { echo "build-flatpak.sh pins do not match the manifest" >&2; exit 1; }
# The tarball job of CI builds with the release's Rust.
rust_version=$(sed -n 's/^  RUST_VERSION: "\(.*\)"$/\1/p' .github/workflows/release.yml)
grep -q -- "--default-toolchain $rust_version\$" .github/workflows/ci.yml || { echo "ci.yml does not install the release's Rust $rust_version" >&2; exit 1; }
# The release builds in images named by digest, never by the tag they carry today (see CONTRIBUTING.md, Pinned tools).
for img in kuickshare-build kuickshare-flatpak kuickshare-appimage; do
    grep -Eq "ghcr\.io/the-beper/$img:[a-z0-9]+@sha256:[0-9a-f]{64}" .github/workflows/release.yml || { echo "release.yml does not pin $img by digest" >&2; exit 1; }
done
# The deb image is a second tag of kuickshare-build (fedora45 is the loop's check above): it is pinned in its own line.
# Until images.yml publishes it from main, the pin is the zeros placeholder of release.yml: the release pull fails then,
# so the placeholder merely documents the paste that CONTRIBUTING.md, Pinned tools wants before the first release.
grep -Eq 'ghcr\.io/the-beper/kuickshare-build:ubuntu26@sha256:[0-9a-f]{64}' .github/workflows/release.yml || { echo "release.yml does not pin kuickshare-build:ubuntu26 by digest" >&2; exit 1; }
if grep -o 'ghcr\.io/the-beper/[^ "]*' .github/workflows/release.yml | grep -Ev '@sha256:[0-9a-f]{64}$'; then echo "release.yml has an image without a digest" >&2; exit 1; fi
