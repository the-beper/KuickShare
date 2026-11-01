#!/bin/sh
# Builds dist/KuickShare-x86_64.pkg.tar.zst with makepkg from the source tarball and the rendered
# PKGBUILD, inside the kuickshare-appimage:arch image (rust and the KF6 packages it installs are also
# the PKGBUILD's build and program inputs). makepkg refuses root: outside the container, podman
# --userns=keep-id (build-env.sh) runs this with the invoking user's identity.
#     build/packaging/build-arch.sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
# Run inside the build container (podman creates /run/.containerenv).
[ -e /run/.containerenv ] || {
    KUICKSHARE_BUILD_IMAGE=localhost/kuickshare-appimage:arch \
    KUICKSHARE_BUILD_CONTAINERFILE="$root/build/appimage/Containerfile.arch" \
        exec "$root/build/build-env.sh" "$root/build/packaging/build-arch.sh"
}

version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)
pack=dist/tmp/packaging
out=dist/KuickShare-x86_64.pkg.tar.zst
mkdir -p "$pack"

tarball=$("$root/build/packaging/source-tarball.sh")
# Makepkg works in its own startdir (the rendered PKGBUILD and the checksummed tarball next to it) and
# re-extracts the source itself: the build tree is $start/src, the package lands in PKGDEST.
start=$pack/arch-src
dest=$pack/arch-pkg
rm -rf "$start" "$dest" "$out"
mkdir -p "$start" "$dest"
cp "$tarball" "$start/"
build/packaging/render.sh "$start"

(
    cd "$start"
    # --nocheck: the tests run in the normal checks. The packages of depends: makepkg wants them
    # installed, the image carries them.
    PKGDEST="$root/$dest" makepkg -f --nocheck --noconfirm
)
[ 1 = "$(find "$dest" -maxdepth 1 -name "kuickshare-$version-1-x86_64.pkg.tar.zst" -type f | wc -l)" ] \
    || { echo "build-arch: more than one .pkg.tar.zst in $dest" >&2; exit 1; }
built=$(find "$dest" -maxdepth 1 -name "kuickshare-$version-1-x86_64.pkg.tar.zst" -type f)
install -m 644 "$built" "$out"
echo "Built $out"
