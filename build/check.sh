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
# The vendored library is not a workspace member; its tests may leave a Cargo.lock behind. Remove only one that did not
# exist before. (Asking git is wrong here: in a git worktree the .git file points outside the mounted directory, git fails
# in the build container, and a tracked lock file would be deleted.)
rqs_lock=third_party/rqs_lib/Cargo.lock
had_lock=false; [ ! -e "$rqs_lock" ] || had_lock=true
cargo test -p rqs_lib
[ "$had_lock" = true ] || rm -f "$rqs_lock"

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
