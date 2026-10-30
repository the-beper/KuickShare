#!/bin/sh
# Builds an OSTree repository (dist/flatpak-repo, or $KUICKSHARE_FLATPAK_REPO) and dist/KuickShare-x86_64.flatpak
# inside a throw-away container. Runtimes and the Flatpak build cache live in the podman volume "kuickshare-flatpak", not on the host.
# Usage: build/flatpak/build-flatpak.sh [flatpak-builder options...]
# Without --gpg-sign the exported commits are unsigned. The release does that on purpose: this build job has no key, and
# build/release/sign-flatpak-repo.sh signs the commits in another job (see CONTRIBUTING.md, Releases).
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
image=localhost/kuickshare-flatpak:fedora45

if [ ! -e /run/.containerenv ]; then
    podman image exists "$image" || podman build -t "$image" -f "$root/build/flatpak/Containerfile" "$root/build/flatpak"
    exec podman run --rm --privileged --security-opt label=disable --network=host -e KUICKSHARE_FLATPAK_REPO \
        -v kuickshare-flatpak:/var/lib/flatpak -v "$root:$root" -w "$root" \
        "$image" "$root/build/flatpak/build-flatpak.sh" "$@"
fi

cd "$root"
version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)
[ -n "$version" ] || { echo "version not found in src/shell/CMakeLists.txt" >&2; exit 1; }
id=io.github.the_beper.KuickShare
tools=dist/tmp/flatpak-builder-tools
# The build tree holds symlinks to /run, which on the host leads into the document portal and makes file scanners
# (VS Code, rust-analyzer) walk the whole home through it: keep it in the volume, not in the workspace.
builder=/var/lib/flatpak/builder
repo=$(readlink -m "${KUICKSHARE_FLATPAK_REPO:-dist/flatpak-repo}")
# A repository cloned from gh-pages lacks its empty directories (git keeps none).
if [ -d "$repo" ]; then mkdir -p "$repo/refs/heads" "$repo/refs/mirrors" "$repo/refs/remotes" "$repo/tmp"; fi

flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
# The build tools at fixed Flathub commits (x86_64): this is what the release builds with, so a change is a reviewed one.
# Re-check and bump: CONTRIBUTING.md, Pinned tools. The KDE runtime is the one the app runs on; the freedesktop runtime and
# the other extensions it pulls in are not pinned.
sdk=org.kde.Sdk//6.11
platform=org.kde.Platform//6.11
rust=org.freedesktop.Sdk.Extension.rust-stable//25.08
sdk_commit=df314f9c067cba031e153a2f3b0be53bd13c1eca6baa78b68982a1d35b873a86
platform_commit=76fa01ac42ecf45d2c5020ef80de1c474c0c2ae86a51a79e8fec44c48f9dc473
rust_commit=09784a9989ae5a7fda6a2280033ca9aa32aac6907961da8b18be4590dd261507
pin() { # pin REF COMMIT: installs nothing, moves the installed ref to the commit and checks it
    flatpak update -y --noninteractive --commit="$2" "$1"
    pinned "$1" "$2"
}
pinned() { [ "$(flatpak info --show-commit "$1")" = "$2" ] || { echo "$1 is not at the pinned commit $2" >&2; exit 1; }; }
flatpak install -y --noninteractive flathub "$sdk" "$platform" "$rust"
pin "$sdk" "$sdk_commit"
pin "$platform" "$platform_commit"
pin "$rust" "$rust_commit"
mkdir -p dist/tmp
# Pinned: this code is third-party and runs in the release build job (which has no signing key, but builds what is published).
tools_rev=74697c75b630d7330e77250fc13cb5ea688d9479
[ -d "$tools" ] || git clone -q https://github.com/flatpak/flatpak-builder-tools "$tools"
git -C "$tools" checkout -q "$tools_rev"
python3 "$tools/cargo/flatpak-cargo-generator.py" Cargo.lock -o build/flatpak/cargo-sources.json
# Cargo deprecated the extensionless config file; the generator still writes it.
sed -i 's/"dest-filename": "config"/"dest-filename": "config.toml"/' build/flatpak/cargo-sources.json

cd build/flatpak
flatpak-builder --force-clean --disable-rofiles-fuse --default-branch=stable \
    --state-dir="$builder/state" --repo="$repo" "$@" \
    "$builder/build" "$id.yml"
cd "$root"
# No --install-deps-from: flatpak-builder would update every installed dependency to the Flathub head.
pinned "$sdk" "$sdk_commit"
pinned "$platform" "$platform_commit"
pinned "$rust" "$rust_commit"
flatpak build-bundle "$repo" dist/KuickShare-x86_64.flatpak "$id" stable
echo "Built dist/KuickShare-x86_64.flatpak"
