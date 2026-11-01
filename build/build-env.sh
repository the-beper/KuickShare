#!/bin/sh
# Usage: build/build-env.sh [command...] (default: interactive shell)
# The build image is fedora45 (build/Containerfile); a package build selects its own with:
#   KUICKSHARE_BUILD_IMAGE=... KUICKSHARE_BUILD_CONTAINERFILE=build/<dir>/<file> build/build-env.sh ...
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
image=${KUICKSHARE_BUILD_IMAGE:-localhost/kuickshare-build:fedora45}
containerfile=${KUICKSHARE_BUILD_CONTAINERFILE:-$root/build/Containerfile}

podman image exists "$image" || podman build -t "$image" -f "$containerfile" "$(dirname "$containerfile")"

tty=; [ -t 0 ] && tty=-t

exec podman run --rm -i $tty --userns=keep-id --network=host --security-opt label=disable \
    -e HOME="$HOME" -e PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin" \
    -v "$HOME/.cargo:$HOME/.cargo" -v "$HOME/.rustup:$HOME/.rustup" \
    -v "$root:$root" -w "$root" \
    "$image" "${@:-bash}"
