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
# The vendored library is not a workspace member; its tests leave an untracked Cargo.lock behind.
cargo test -p rqs_lib
git ls-files --error-unmatch third_party/rqs_lib/Cargo.lock >/dev/null 2>&1 || rm -f third_party/rqs_lib/Cargo.lock

step "Shell build and tests"
cmake -S src/shell -B dist/tmp/cmake-dev -G Ninja
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
