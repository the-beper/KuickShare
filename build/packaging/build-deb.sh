#!/bin/sh
# Builds dist/KuickShare-x86_64.deb as an Ubuntu 26.04 package (dpkg-buildpackage -us -uc -b) from the
# source tarball and the rendered debian/ recipe. Runs inside the ubuntu26 build image
# (build/deb/Containerfile.ubuntu, whose rustc/cargo are the Ubuntu archive's):
#     build/packaging/build-deb.sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
# The archive's rustc/cargo, not the rustup shims the environment normally exposes.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# Run inside the build container (podman creates /run/.containerenv).
[ -e /run/.containerenv ] || {
    KUICKSHARE_BUILD_IMAGE=localhost/kuickshare-build:ubuntu26 \
    KUICKSHARE_BUILD_CONTAINERFILE="$root/build/deb/Containerfile.ubuntu" \
        exec "$root/build/build-env.sh" "$root/build/packaging/build-deb.sh"
}

version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)
pack=dist/tmp/packaging
out=dist/KuickShare-x86_64.deb
mkdir -p "$pack"

tarball=$("$root/build/packaging/source-tarball.sh")
rm -rf "$pack/kuickshare-$version" "$out"
tar -xzf "$tarball" -C "$pack"
build/packaging/render.sh "$pack/kuickshare-$version"

(
    cd "$pack/kuickshare-$version"
    # The same cmake --install the rpm and the tarball use rules of; shlibs deps are computed when the
    # .deb gets packaged, the Depends of debian/control are checked by test-deb.sh.
    dpkg-buildpackage -us -uc -b
)
[ 1 = "$(find "$pack" -maxdepth 1 -name "kuickshare_${version}-*_amd64.deb" -type f | wc -l)" ] \
    || { echo "build-deb: more than one .deb in $pack" >&2; exit 1; }
built=$(find "$pack" -maxdepth 1 -name "kuickshare_${version}-*_amd64.deb" -type f)
install -m 644 "$built" "$out"
echo "Built $out"
