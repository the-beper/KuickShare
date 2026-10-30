#!/bin/sh
# Finishes a Flatpak repository for hosting on a static web server (GitHub Pages): prunes the OSTree repository, makes
# its summary and appstream data, and writes the files users add it with. There are no static deltas: a delta carries the
# signature of its commit and the signed summary must list it, which the split signing of the release cannot do.
# Usage: [GPG_KEY_ID=<key> | GPG_KEY_B64=<key>] build/flatpak/publish-repo.sh <site-dir> <base-url>
#   <site-dir>/repo      the OSTree repository (build it with KUICKSHARE_FLATPAK_REPO=<site-dir>/repo)
#   <base-url>           where <site-dir> is served, e.g. https://the-beper.github.io/KuickShare
#   GPG_KEY_ID           signs the new commits and the summary with this key of the keyring ($GNUPGHOME), and writes
#                        its public key into the .flatpakrepo and .flatpakref
#   GPG_KEY_B64          without a key to sign with (the release's prepare job holds none): the public key, base64 of the
#                        binary key, for the .flatpakrepo and .flatpakref; nothing is signed
# Needs flatpak, and gpg with GPG_KEY_ID.
set -eu
site=$(readlink -f "$1")
url=${2%/}
if [ -n "${GPG_KEY_ID:-}" ]; then
    sign=--gpg-sign=$GPG_KEY_ID
    key=$(gpg --export "$GPG_KEY_ID" | base64 -w0)
else
    sign=
    key=${GPG_KEY_B64:?GPG_KEY_B64 is empty or not set: set it to the base64 of the binary public key (nothing is signed), or set GPG_KEY_ID to sign}
fi

mkdir -p "$site/repo/refs/heads" "$site/repo/refs/mirrors" "$site/repo/refs/remotes" "$site/repo/tmp"
# The deltas of earlier releases would be listed in the new summary: clients fetch objects instead.
rm -rf "$site/repo/deltas"
flatpak build-update-repo ${sign:+"$sign"} --prune --prune-depth=3 "$site/repo"
GPG_KEY_B64=$key "$(dirname "$0")/write-site-files.sh" "$site" "$url"
