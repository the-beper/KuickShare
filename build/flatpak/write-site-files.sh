#!/bin/sh
# Writes the files of the site that are not the OSTree repository: the .flatpakrepo and .flatpakref users add it with, the
# icon, and .nojekyll. build/flatpak/publish-repo.sh calls it; finish-flatpak-repo.sh calls it again on an empty folder to
# compare what the prepare job wrote.
# Usage: GPG_KEY_B64=<base64 of the binary public key> build/flatpak/write-site-files.sh <site-dir> <base-url>
set -eu
site=$(readlink -f "$1")
url=${2%/}
id=io.github.the_beper.KuickShare
key=${GPG_KEY_B64:?GPG_KEY_B64 is empty or not set: it must be the base64 of the binary public key}

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
