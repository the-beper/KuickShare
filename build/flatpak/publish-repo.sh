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
id=io.github.the_beper.KuickShare
if [ -n "${GPG_KEY_ID:-}" ]; then
    sign=--gpg-sign=$GPG_KEY_ID
    key=$(gpg --export "$GPG_KEY_ID" | base64 -w0)
else
    sign=
    key=${GPG_KEY_B64:?set GPG_KEY_ID (to sign) or GPG_KEY_B64 (the public key, nothing is signed)}
fi

mkdir -p "$site/repo/refs/heads" "$site/repo/refs/mirrors" "$site/repo/refs/remotes" "$site/repo/tmp"
# The deltas of earlier releases would be listed in the new summary: clients fetch objects instead.
rm -rf "$site/repo/deltas"
flatpak build-update-repo ${sign:+"$sign"} --prune --prune-depth=3 "$site/repo"

cat > "$site/kuickshare.flatpakrepo" <<REPO
[Flatpak Repo]
Title=KuickShare
Url=$url/repo/
Homepage=https://github.com/the-beper/KuickShare
Comment=Quick Share client for KDE Plasma
Description=Send files, text and links between your Linux desktop and Android phones.
Icon=$url/icon.svg
GPGKey=$key
REPO

cat > "$site/kuickshare.flatpakref" <<REF
[Flatpak Ref]
Title=KuickShare
Name=$id
Branch=stable
Url=$url/repo/
SuggestRemoteName=kuickshare
IsRuntime=false
RuntimeRepo=https://dl.flathub.org/repo/flathub.flatpakrepo
GPGKey=$key
REF

cp "$(dirname "$0")/../../data/icons/hicolor/scalable/apps/$id.svg" "$site/icon.svg"
# GitHub Pages runs Jekyll unless told not to, and Jekyll skips the dot-files OSTree uses.
touch "$site/.nojekyll"
