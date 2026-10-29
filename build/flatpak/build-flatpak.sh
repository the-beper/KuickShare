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
mkdir -p dist/tmp
# Pinned: this code is third-party and runs in the release build job (which has no signing key, but builds what is published).
tools_rev=74697c75b630d7330e77250fc13cb5ea688d9479
[ -d "$tools" ] || git clone -q https://github.com/flatpak/flatpak-builder-tools "$tools"
git -C "$tools" checkout -q "$tools_rev"
python3 "$tools/cargo/flatpak-cargo-generator.py" Cargo.lock -o build/flatpak/cargo-sources.json
# Cargo deprecated the extensionless config file; the generator still writes it.
sed -i 's/"dest-filename": "config"/"dest-filename": "config.toml"/' build/flatpak/cargo-sources.json

cd build/flatpak
flatpak-builder --force-clean --disable-rofiles-fuse --install-deps-from=flathub --default-branch=stable \
    --state-dir="$builder/state" --repo="$repo" "$@" \
    "$builder/build" "$id.yml"
cd "$root"
flatpak build-bundle "$repo" dist/KuickShare-x86_64.flatpak "$id" stable
echo "Built dist/KuickShare-x86_64.flatpak"
