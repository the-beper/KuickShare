#!/bin/sh
# Builds dist/KuickShare-x86_64.rpm: same build commands as the tarball (build/build-host-payload.sh),
# as an rpm from the rendered recipe (build/packaging/{render.sh,kuickshare.spec.in}). It runs inside
# the Fedora build image (build/Containerfile, which needs rpm-build, rust and cargo): start it
# plainly, the container is entered automatically:
#     build/packaging/build-rpm.sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
# Run inside the build container (podman creates /run/.containerenv).
[ -e /run/.containerenv ] || exec "$root/build/build-env.sh" "$root/build/packaging/build-rpm.sh"

version=$(sed -n 's/^project(kuickshare VERSION \([0-9.]*\).*/\1/p' src/shell/CMakeLists.txt)
pack=dist/tmp/packaging
out=dist/KuickShare-x86_64.rpm
mkdir -p "$pack"

rm -rf "$pack/spec" "$out" "$pack/output"
build/packaging/render.sh "$pack/spec"

# Under these macros the build and output files stay inside the checkout; every ${?dist} the image's
# rpm macros add is welcome in the file name and in the inner Release. _topdir too: rpm touches it
# where the other macros do not cover it, and the container HOME is the host's (a build must not leave
# repairs / directories there).
rpmbuild -bb --define "_topdir $PWD/$pack/rpmtop" --define "_sourcedir $PWD/$pack" --define "_specdir $PWD/$pack/spec" \
    --define "_builddir $PWD/$pack/rpmbuild" --define "_buildrootdir $PWD/$pack/rpmbuildroot" \
    --define "_rpmdir $PWD/$pack/output" --define "_srcrpmdir $PWD/$pack/output" \
    "$pack/spec/kuickshare.spec"

# Overwrite obsolete rpm from other (older) versions; exactly one .rpm landed in output.
built=$(find "$pack/output" -name "kuickshare-$version-*.rpm" -type f)
[ "$(find "$pack/output" -name '*.rpm' -type f | wc -l)" = 1 ] || { echo "build-rpm: more than one .rpm in $pack/output" >&2; exit 1; }
install -m 644 "$built" "$out"
echo "Built $out"
